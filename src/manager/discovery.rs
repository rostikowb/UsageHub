//! Automatic local account discovery.
//!
//! Scans the places a provider stores its own sign-in state (Claude Desktop
//! data directories, the Claude CLI home, an explicit `CLAUDE_CONFIG_DIR`,
//! an explicit `CODEX_HOME`, and ChatGPT sign-ins stored by omp) and keeps the
//! registry in sync:
//! - a source that is not registered yet becomes a profile once;
//! - a profile created by discovery is removed from the registry after three
//!   consecutive scans that no longer find its source;
//! - user-created profiles are never removed, and discovery never deletes
//!   account data.
//!
//! The scan is bounded and read-only; callers decide when it runs (startup,
//! every few hours, or an explicit "scan now").

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::accounts::{AccountOwnership, AccountProfile};
use crate::manager::{state, ManagerResult};
use crate::providers::ProviderId;

/// Consecutive missed scans before an auto-discovered profile is unregistered.
pub const REMOVE_AFTER_MISSES: u32 = 3;
/// Upper bound per root so a huge directory can never stall the worker.
const MAX_ENTRIES_PER_ROOT: usize = 50;

#[derive(Clone, Debug, Serialize)]
pub struct AddedAccount {
    pub provider: &'static str,
    pub id: String,
    pub name: String,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PendingRemoval {
    pub provider: &'static str,
    pub id: String,
    pub misses: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DiscoveryReport {
    pub added: Vec<AddedAccount>,
    pub removed: Vec<String>,
    pub pending_removal: Vec<PendingRemoval>,
    pub warnings: Vec<String>,
    pub scanned_sources: usize,
    pub applied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub provider: ProviderId,
    pub name: String,
    pub key: String,
    pub desktop_dir: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    /// Set for omp-hosted ChatGPT sign-ins instead of a filesystem directory.
    pub omp_email: Option<String>,
}

fn canonical(path: &Path) -> String {
    crate::manager::native::canonical_spelling(path)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    let value = std::env::var_os(name)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

fn has_file(dir: &Path, name: &str) -> bool {
    dir.join(name).is_file()
}

/// Directory roots that already belong to a registered profile.
fn registered_paths(settings: &crate::app_settings::SettingsFile) -> BTreeSet<String> {
    let mut known = BTreeSet::new();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let Some(accounts) = settings.accounts.get(provider) else {
            continue;
        };
        for profile in &accounts.profiles {
            for field in [&profile.desktop_dir, &profile.config_dir] {
                let field = field.trim();
                if !field.is_empty() {
                    known.insert(canonical(Path::new(field)));
                }
            }
            if !profile.discovery_key.is_empty() {
                known.insert(profile.discovery_key.to_ascii_lowercase());
            }
        }
    }
    known
}

/// Codex account IDs already covered by a registered profile (including the
/// canonical `~/.codex` Default). Prevents importing the same ChatGPT account
/// twice under a different source.
fn registered_codex_account_ids(settings: &crate::app_settings::SettingsFile) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    let default_auth =
        crate::known_folders::home_dir().map(|home| home.join(".codex").join("auth.json"));
    let Some(accounts) = settings.accounts.get(ProviderId::Codex) else {
        return ids;
    };
    for profile in &accounts.profiles {
        let path = if profile.config_dir.trim().is_empty() {
            default_auth.clone()
        } else {
            Some(Path::new(&profile.config_dir).join("auth.json"))
        };
        let Some(path) = path else { continue };
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(account_id) = value
            .get("tokens")
            .and_then(|tokens| tokens.get("account_id"))
            .and_then(|id| id.as_str())
            .filter(|id| !id.trim().is_empty())
        {
            ids.insert(account_id.trim().to_ascii_lowercase());
        }
    }
    ids
}

/// omp emails already imported into a registered Codex profile.
fn imported_omp_emails(settings: &crate::app_settings::SettingsFile) -> BTreeSet<String> {
    let mut emails = BTreeSet::new();
    let Some(accounts) = settings.accounts.get(ProviderId::Codex) else {
        return emails;
    };
    for profile in &accounts.profiles {
        for field in [&profile.desktop_dir, &profile.config_dir] {
            if field.trim().is_empty() {
                continue;
            }
            let path = Path::new(field).join("auth.json");
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
                continue;
            };
            if let Some(email) = value.get("usagehub_omp_email").and_then(|v| v.as_str()) {
                emails.insert(email.trim().to_ascii_lowercase());
            }
        }
    }
    emails
}

fn claude_candidates(known: &BTreeSet<String>, warnings: &mut Vec<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut push_dir = |dir: PathBuf, name: String| {
        let key = canonical(&dir);
        if known.contains(&key) {
            return;
        }
        let desktop = has_file(&dir, "config.json");
        let cli = has_file(&dir, ".credentials.json")
            || has_file(&dir, "settings.json")
            || has_file(&dir, "history.json");
        if !desktop && !cli {
            return;
        }
        out.push(Candidate {
            provider: ProviderId::Claude,
            name,
            key,
            desktop_dir: desktop.then(|| dir.clone()),
            config_dir: (!desktop && cli).then_some(dir),
            omp_email: None,
        });
    };

    if let Some(local) = crate::known_folders::local_app_data_dir() {
        let root = local.join("Claude-Data");
        if let Ok(entries) = std::fs::read_dir(&root) {
            let mut seen = 0;
            for entry in entries.flatten().take(MAX_ENTRIES_PER_ROOT) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                else {
                    continue;
                };
                seen += 1;
                push_dir(path, format!("Claude {name}"));
            }
            if seen == 0 {
                warnings.push(format!(
                    "No Claude Desktop data directories found in {}",
                    root.display()
                ));
            }
        }
    }
    if let Some(home) = crate::known_folders::home_dir() {
        push_dir(home.join(".claude"), "Claude CLI".into());
    }
    if let Some(dir) = absolute_env("CLAUDE_CONFIG_DIR") {
        push_dir(dir, "Claude CLI (CLAUDE_CONFIG_DIR)".into());
    }
    out
}

fn codex_dir_candidates(known: &BTreeSet<String>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Some(default_home) = crate::known_folders::home_dir().map(|home| home.join(".codex"))
    else {
        return out;
    };
    let mut push = |dir: PathBuf, name: String| {
        let key = canonical(&dir);
        if known.contains(&key) || !has_file(&dir, "auth.json") {
            return;
        }
        out.push(Candidate {
            provider: ProviderId::Codex,
            name,
            key,
            desktop_dir: None,
            config_dir: Some(dir),
            omp_email: None,
        });
    };
    if let Some(dir) = absolute_env("CODEX_HOME") {
        if canonical(&dir) != canonical(&default_home) {
            push(dir, "Codex CLI (CODEX_HOME)".into());
        }
    }
    out
}

fn omp_candidates(
    known: &BTreeSet<String>,
    imported: &BTreeSet<String>,
    account_ids: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Vec<Candidate> {
    let credentials = match crate::manager::omp_auth::codex_credentials() {
        Ok(credentials) => credentials,
        Err(error) => {
            // A missing omp install is normal, not a warning-worthy failure.
            if !error.contains("not found") {
                warnings.push(error);
            }
            return Vec::new();
        }
    };
    let now = now_unix();
    credentials
        .into_iter()
        .filter(|credential| credential.usable_at(now))
        .filter(|credential| !credential.email.trim().is_empty())
        .filter(|credential| {
            let key = crate::manager::omp_auth::discovery_key(&credential.email);
            let account_known = credential
                .account_id
                .as_deref()
                .map(|id| id.trim().to_ascii_lowercase())
                .is_some_and(|id| account_ids.contains(&id));
            !known.contains(&key)
                && !imported.contains(&credential.email.trim().to_ascii_lowercase())
                && !account_known
        })
        .map(|credential| {
            let email = credential.email.trim().to_string();
            let name = email
                .split('@')
                .next()
                .filter(|value| !value.is_empty())
                .unwrap_or("Codex")
                .to_string();
            Candidate {
                provider: ProviderId::Codex,
                name,
                key: crate::manager::omp_auth::discovery_key(&email),
                desktop_dir: None,
                config_dir: None,
                omp_email: Some(email),
            }
        })
        .collect()
}

/// Read-only scan of every supported source.
pub fn candidates() -> DiscoveryReport {
    let settings = state::load_fresh();
    let known = registered_paths(&settings);
    let imported = imported_omp_emails(&settings);
    let account_ids = registered_codex_account_ids(&settings);
    let mut warnings = Vec::new();
    let mut found = claude_candidates(&known, &mut warnings);
    found.extend(codex_dir_candidates(&known));
    found.extend(omp_candidates(
        &known,
        &imported,
        &account_ids,
        &mut warnings,
    ));
    let mut report = DiscoveryReport {
        warnings,
        scanned_sources: known.len(),
        ..Default::default()
    };
    // Records what a fresh install would add; used by `--manage discover`.
    report.added = found
        .iter()
        .map(|candidate| AddedAccount {
            provider: candidate.provider.descriptor().key,
            id: String::new(),
            name: candidate.name.clone(),
            source: candidate.key.clone(),
        })
        .collect();
    report
}

fn next_id(used: &BTreeSet<String>, provider: ProviderId) -> String {
    for index in 1..=9_999 {
        let id = format!("auto_{}_{index}", provider.descriptor().key);
        if !used.contains(&id) {
            return id;
        }
    }
    "auto_account".into()
}

/// Update the miss counter for one auto-discovered profile. Returns whether
/// the profile stays registered.
fn age_profile(profile: &mut AccountProfile, present: bool) -> bool {
    if present {
        profile.discovery_misses = 0;
        return true;
    }
    profile.discovery_misses = profile.discovery_misses.saturating_add(1);
    profile.discovery_misses < REMOVE_AFTER_MISSES
}

/// An empty managed Codex profile the user created but never signed in. The
/// first discovered omp sign-in is imported into it instead of creating a
/// third profile for the same intent.
fn claimable_codex_profile(settings: &crate::app_settings::SettingsFile) -> Option<String> {
    settings
        .accounts
        .codex
        .profiles
        .iter()
        .find(|profile| {
            profile.ownership == AccountOwnership::Managed
                && !profile.auto_discovered
                && profile.discovery_key.is_empty()
                && {
                    let root = if !profile.config_dir.trim().is_empty() {
                        Path::new(&profile.config_dir)
                    } else {
                        Path::new(&profile.desktop_dir)
                    };
                    let auth = root.join("auth.json");
                    let Ok(content) = std::fs::read_to_string(auth) else {
                        return true;
                    };
                    !serde_json::from_str::<serde_json::Value>(&content)
                        .ok()
                        .is_some_and(|value| {
                            value
                                .get("tokens")
                                .and_then(|tokens| tokens.get("access_token"))
                                .and_then(|token| token.as_str())
                                .is_some_and(|token| !token.trim().is_empty())
                        })
                }
        })
        .map(|profile| profile.id.clone())
}

fn directory_profile(candidate: &Candidate, id: String) -> AccountProfile {
    AccountProfile {
        id,
        name: candidate.name.clone(),
        desktop_dir: candidate
            .desktop_dir
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        config_dir: candidate
            .config_dir
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default(),
        enabled: true,
        pinned: false,
        ownership: AccountOwnership::External,
        auto_discovered: true,
        discovery_key: candidate.key.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
fn source_present(key: &str, omp_emails: &BTreeSet<String>) -> bool {
    if let Some(email) = key.strip_prefix("omp:") {
        return omp_emails.contains(email);
    }
    Path::new(key).exists()
}

/// Scan and (when `apply`) persist additions and missed-source removals.
pub fn run(apply: bool) -> ManagerResult<DiscoveryReport> {
    let mut report = candidates();
    report.applied = apply;
    if !apply {
        return Ok(report);
    }
    // The public report above only previews candidates; a real run records the
    // profiles that were actually committed.
    report.added.clear();

    // `None` means the omp store could not be read right now. Omp-hosted
    // profiles are then treated as present instead of being aged out because
    // of a transient read failure.
    let omp_emails = crate::manager::omp_auth::codex_credentials()
        .ok()
        .map(|credentials| {
            credentials
                .into_iter()
                .map(|credential| credential.email.trim().to_ascii_lowercase())
                .collect::<BTreeSet<_>>()
        });

    // Add filesystem candidates and age auto-discovered ones in one commit.
    let mut omp_to_add: Vec<Candidate> = Vec::new();
    let known = registered_paths(&state::load_fresh());

    // Re-scan for precise candidates (the public report above only carries
    // display data). Directory profiles are added directly; omp entries need
    // the lifecycle create+import path and are collected for the second pass.
    let settings = state::load_fresh();
    let imported = imported_omp_emails(&settings);
    let account_ids = registered_codex_account_ids(&settings);
    let mut warnings = Vec::new();
    let mut fresh = claude_candidates(&known, &mut warnings);
    fresh.extend(codex_dir_candidates(&known));
    fresh.extend(omp_candidates(
        &known,
        &imported,
        &account_ids,
        &mut warnings,
    ));

    state::mutate_settings(|settings| {
        let now = now_unix();
        let mut removed = Vec::new();
        let mut pending = Vec::new();
        for provider in [ProviderId::Claude, ProviderId::Codex] {
            let accounts = match provider {
                ProviderId::Claude => &mut settings.accounts.claude,
                ProviderId::Codex => &mut settings.accounts.codex,
                _ => continue,
            };
            accounts.profiles.retain_mut(|profile| {
                if !profile.auto_discovered || profile.discovery_key.is_empty() {
                    return true;
                }
                // A missing omp database must not age out omp-hosted profiles.
                let present = match (&omp_emails, profile.discovery_key.strip_prefix("omp:")) {
                    (Some(emails), Some(email)) => emails.contains(email),
                    (None, Some(_)) => true,
                    (_, None) => Path::new(&profile.discovery_key).exists(),
                };
                if age_profile(profile, present) {
                    if !present {
                        pending.push(PendingRemoval {
                            provider: provider.descriptor().key,
                            id: profile.id.clone(),
                            misses: profile.discovery_misses,
                        });
                    }
                    return true;
                }
                removed.push(profile.id.clone());
                false
            });
        }
        report.removed = removed;
        report.pending_removal = pending;

        for candidate in &fresh {
            if candidate.omp_email.is_some() {
                omp_to_add.push(candidate.clone());
                continue;
            }
            let accounts = match candidate.provider {
                ProviderId::Claude => &mut settings.accounts.claude,
                ProviderId::Codex => &mut settings.accounts.codex,
                _ => continue,
            };
            let id = next_id(&accounts.used_ids, candidate.provider);
            accounts.used_ids.insert(id.clone());
            let profile = directory_profile(candidate, id.clone());
            report.added.push(AddedAccount {
                provider: candidate.provider.descriptor().key,
                id,
                name: candidate.name.clone(),
                source: candidate.key.clone(),
            });
            accounts.profiles.push(profile);
        }
        settings.last_discovery_unix = now;
        Ok::<_, String>(())
    })?;

    // omp-hosted ChatGPT sign-ins become managed CLI profiles.
    for candidate in omp_to_add {
        let Some(email) = candidate.omp_email.as_deref() else {
            continue;
        };
        let claim = claimable_codex_profile(&state::load_fresh());
        let (id, name, claimed) = match claim {
            Some(id) => (id.clone(), id, true),
            None => {
                let created = crate::manager::lifecycle::create_account(
                    ProviderId::Codex,
                    &candidate.name,
                    crate::manager::lifecycle::NewAccountOpts {
                        ownership: AccountOwnership::Managed,
                        pinned: false,
                        ..Default::default()
                    },
                );
                match created {
                    Ok(profile) => (profile.id, profile.name, false),
                    Err(error) => {
                        report
                            .warnings
                            .push(format!("Unable to add omp account '{email}': {error}"));
                        continue;
                    }
                }
            }
        };
        let imported =
            crate::manager::lifecycle::import_omp_sign_in(ProviderId::Codex, &id, Some(email));
        if let Err(error) = imported {
            // A brand-new profile is rolled back; a claimed user profile stays.
            if !claimed {
                let _ = state::mutate_settings(|settings| {
                    settings
                        .accounts
                        .codex
                        .profiles
                        .retain(|profile| profile.id != id);
                    Ok::<_, String>(())
                });
            }
            report
                .warnings
                .push(format!("Unable to import omp sign-in '{email}': {error}"));
            continue;
        }
        let _ = state::mutate_settings(|settings| {
            if let Some(profile) = settings
                .accounts
                .codex
                .profiles
                .iter_mut()
                .find(|profile| profile.id == id)
            {
                // A claimed user profile stays user-owned: never auto-removed.
                profile.auto_discovered = !claimed;
                profile.discovery_key = candidate.key.clone();
            }
            Ok::<_, String>(())
        });
        report.added.push(AddedAccount {
            provider: "codex",
            id,
            name,
            source: candidate.key,
        });
    }

    report.warnings.extend(warnings);
    let summary = format!(
        "added {}, removed {}, pending {}",
        report.added.len(),
        report.removed.len(),
        report.pending_removal.len()
    );
    let _ = state::mutate_settings(|settings| {
        settings.last_discovery_unix = now_unix();
        settings.last_discovery_added = report.added.len() as u32;
        settings.last_discovery_removed = report.removed.len() as u32;
        settings.last_discovery_pending = report.pending_removal.len() as u32;
        settings.last_discovery_summary =
            if report.added.is_empty() && report.removed.is_empty() && report.warnings.is_empty() {
                "No new accounts found".to_string()
            } else {
                summary
            };
        Ok::<_, String>(())
    });
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_never_collide_with_reserved_ids() {
        let mut used = BTreeSet::new();
        assert_eq!(next_id(&used, ProviderId::Claude), "auto_claude_1");
        used.insert("auto_claude_1".into());
        assert_eq!(next_id(&used, ProviderId::Claude), "auto_claude_2");
        assert_eq!(next_id(&used, ProviderId::Codex), "auto_codex_1");
    }

    #[test]
    fn omp_and_directory_keys_report_presence_independently() {
        let emails = BTreeSet::from(["user@example.com".to_string()]);
        assert!(source_present("omp:user@example.com", &emails));
        assert!(!source_present("omp:other@example.com", &emails));
        let dir = std::env::temp_dir();
        assert!(source_present(&dir.to_string_lossy(), &emails));
        assert!(!source_present(
            &dir.join("definitely-missing-usagehub").to_string_lossy(),
            &emails
        ));
    }

    #[test]
    fn auto_profiles_are_aged_out_only_after_consecutive_misses() {
        let mut profile = AccountProfile {
            auto_discovered: true,
            discovery_key: "c:\\x".into(),
            ..Default::default()
        };
        for _ in 0..REMOVE_AFTER_MISSES - 1 {
            assert!(age_profile(&mut profile, false));
        }
        assert!(!age_profile(&mut profile, false));
        // A single successful scan resets the counter.
        let mut profile = AccountProfile::default();
        profile.discovery_misses = REMOVE_AFTER_MISSES - 1;
        assert!(age_profile(&mut profile, true));
        assert_eq!(profile.discovery_misses, 0);
    }

    #[test]
    fn only_an_unsigned_managed_codex_profile_can_be_claimed() {
        let mut settings = crate::app_settings::SettingsFile::default();
        assert_eq!(claimable_codex_profile(&settings), None);
        settings.accounts.codex.profiles.push(AccountProfile {
            id: "account_9".into(),
            ownership: AccountOwnership::Managed,
            config_dir: "C:\\usagehub-missing-claim\\account_9".into(),
            ..Default::default()
        });
        assert_eq!(
            claimable_codex_profile(&settings).as_deref(),
            Some("account_9")
        );
    }

    #[test]
    fn directory_profiles_are_external_and_unpinned() {
        let candidate = Candidate {
            provider: ProviderId::Claude,
            name: "Claude test".into(),
            key: "c:\\claude-test".into(),
            desktop_dir: Some(PathBuf::from("C:\\claude-test")),
            config_dir: None,
            omp_email: None,
        };
        let profile = directory_profile(&candidate, "auto_claude_1".into());
        assert_eq!(profile.ownership, AccountOwnership::External);
        assert!(profile.auto_discovered);
        assert!(!profile.pinned);
        assert_eq!(profile.discovery_key, "c:\\claude-test");
    }
}
