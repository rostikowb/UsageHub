//! Named credential sources. Settings contain paths and labels, never tokens.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::providers::ProviderId;

/// Stable non-secret key for source paths and file metadata (not token contents).
pub fn fingerprint(value: &str) -> String {
    let hash = value.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

pub fn file_signature(path: &Path) -> String {
    let metadata = std::fs::metadata(path).ok();
    fingerprint(&format!(
        "{}|{:?}|{:?}",
        source_key(path),
        metadata.as_ref().map(|m| m.len()),
        metadata.and_then(|m| m.modified().ok())
    ))
}

pub fn source_key(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().to_lowercase(),
    }
}

pub fn default_credential_path(provider: ProviderId) -> Option<PathBuf> {
    let (folder, file) = match provider {
        ProviderId::Claude => (".claude", ".credentials.json"),
        ProviderId::Codex => (".codex", "auth.json"),
        _ => return None,
    };
    let directory = environment_directory(provider)
        .or_else(|| crate::known_folders::home_dir().map(|home| home.join(folder)))?;
    Some(directory.join(file))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountProfile {
    pub id: String,
    pub name: String,
    pub config_dir: String,
    pub credentials_path: String,
    pub enabled: bool,
    /// Explicit Desktop user-data-dir (config.json + Local State source).
    /// Empty means "not configured"; provider-default discovery stays in the poller.
    pub desktop_dir: String,
    /// Managed (created/adopted by UsageHub) vs external (read-only by default).
    pub ownership: AccountOwnership,
    /// Display-only presentation. Never affects identity, paths, or polling.
    pub pinned: bool,
    pub sort_order: i32,
    pub color: String,
    pub label: String,
    /// Confirmed Windows Credential Manager targets owned by this account.
    /// Only listed targets may be backed up or deleted; unknown targets are
    /// reported, never guessed.
    pub credential_targets: Vec<String>,
}

/// Lifetime ownership of an account profile.
///
/// IDs are immutable once allocated (`used_ids` is never reused); renames only
/// change display fields. New profiles and adopted legacy paths (e.g. Second)
/// are `Managed`; the external Default stays `External` (read-only by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountOwnership {
    Managed,
    #[default]
    External,
}

impl Default for AccountProfile {
    fn default() -> Self {
        Self {
            id: "default".into(),
            name: "Default".into(),
            config_dir: String::new(),
            credentials_path: String::new(),
            enabled: true,
            desktop_dir: String::new(),
            ownership: AccountOwnership::External,
            pinned: false,
            sort_order: 0,
            color: String::new(),
            label: String::new(),
            credential_targets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderAccounts {
    pub profiles: Vec<AccountProfile>,
    pub selected: String,
    /// Retain retired IDs so existing theme bindings never target a new account.
    pub used_ids: std::collections::BTreeSet<String>,
}

impl Default for ProviderAccounts {
    fn default() -> Self {
        Self {
            profiles: vec![AccountProfile::default()],
            selected: "default".into(),
            used_ids: ["default".into()].into(),
        }
    }
}

impl ProviderAccounts {
    pub fn selected(&self) -> Option<&AccountProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.enabled && profile.id == self.selected)
            .or_else(|| self.profiles.iter().find(|profile| profile.enabled))
    }

    pub fn find(&self, id: &str) -> Option<&AccountProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut AccountProfile> {
        self.profiles.iter_mut().find(|profile| profile.id == id)
    }

    /// Display order for account rows: pinned first, then `sort_order`,
    /// then name. Identity (`id`) is never reordered or rewritten.
    pub fn ordered(&self) -> Vec<&AccountProfile> {
        let mut profiles: Vec<&AccountProfile> = self.profiles.iter().collect();
        profiles.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| a.sort_order.cmp(&b.sort_order))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        profiles
    }

    /// Display-only rename. The stable `id` is preserved, so theme bindings,
    /// backup directories, and shortcuts keep targeting the same account.
    pub fn rename(&mut self, id: &str, new_name: &str) -> Result<(), String> {
        let Some(profile) = self.find_mut(id) else {
            return Err(format!("Unknown account '{id}'"));
        };
        profile.rename_display(new_name)
    }

    pub fn add(&mut self) {
        self.normalize();
        let (id, number) = self.allocate_id();
        self.profiles.push(AccountProfile {
            id,
            name: format!("Account {number}"),
            enabled: false,
            ..Default::default()
        });
    }

    fn allocate_id(&mut self) -> (String, usize) {
        let mut number = 1;
        while self.used_ids.contains(&format!("account_{number}")) {
            number += 1;
        }
        let id = format!("account_{number}");
        self.used_ids.insert(id.clone());
        (id, number)
    }

    pub fn normalize(&mut self) {
        let selected_index = self
            .profiles
            .iter()
            .position(|profile| profile.id == self.selected)
            .or_else(|| {
                self.profiles
                    .iter()
                    .position(|profile| profile.id.eq_ignore_ascii_case(&self.selected))
            });
        self.used_ids = std::mem::take(&mut self.used_ids)
            .into_iter()
            .map(|id| id.to_ascii_lowercase())
            .collect();
        // Reserve every existing ID before allocating replacements. An earlier
        // invalid profile must not steal a later valid profile's binding.
        self.used_ids.extend(
            self.profiles
                .iter()
                .map(|profile| profile.id.to_ascii_lowercase()),
        );
        let mut seen = std::collections::HashSet::new();
        for index in 0..self.profiles.len() {
            let profile = &self.profiles[index];
            if profile.id.is_empty()
                || !profile
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || !seen.insert(profile.id.to_ascii_lowercase())
            {
                let (id, _) = self.allocate_id();
                seen.insert(id.to_ascii_lowercase());
                self.profiles[index].id = id;
            }
            let profile = &mut self.profiles[index];
            if profile.name.trim().is_empty() {
                profile.name = profile.id.clone();
            }
        }
        if let Some(index) = selected_index {
            self.selected = self.profiles[index].id.clone();
        }
        self.selected = self.selected().map(|p| p.id.clone()).unwrap_or_default();
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountSettings {
    pub claude: ProviderAccounts,
    pub codex: ProviderAccounts,
}

impl AccountSettings {
    pub fn get(&self, provider: ProviderId) -> Option<&ProviderAccounts> {
        match provider {
            ProviderId::Claude => Some(&self.claude),
            ProviderId::Codex => Some(&self.codex),
            _ => None,
        }
    }
}

pub fn environment_directory(provider: ProviderId) -> Option<PathBuf> {
    let variable = match provider {
        ProviderId::Claude => "CLAUDE_CONFIG_DIR",
        ProviderId::Codex => "CODEX_HOME",
        _ => return None,
    };
    environment_directory_value(std::env::var_os(variable))
}

fn environment_directory_value(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).and_then(|value| {
        let path = Path::new(&value);
        expand_path(path).or_else(|| std::env::current_dir().ok().map(|cwd| cwd.join(path)))
    })
}

pub fn expand_path(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return crate::known_folders::home_dir();
    }
    if let Some(tail) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        return Some(crate::known_folders::home_dir()?.join(tail));
    }
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        // Settings must behave the same when launched from a terminal or at login.
        None
    }
}

impl AccountProfile {
    pub fn same_source(&self, other: &Self) -> bool {
        self.id == other.id
            && self.config_dir == other.config_dir
            && self.credentials_path == other.credentials_path
            && self.desktop_dir == other.desktop_dir
    }

    /// Explicit source kind selected by this profile. The runtime/poller MUST
    /// distinguish `DesktopDir` directly: it reads config.json + Local State in
    /// that directory and never falls back to CLI credential discovery for it.
    pub fn source_kind(&self, provider: ProviderId) -> SourceKind {
        if !self.desktop_dir.trim().is_empty() && provider == ProviderId::Claude {
            return SourceKind::DesktopDir;
        }
        if !self.credentials_path.trim().is_empty() || !self.config_dir.trim().is_empty() {
            return SourceKind::CredentialFile;
        }
        SourceKind::ProviderDefault
    }

    /// Legacy credential-file resolution for CLI/Codex callers. Preserved
    /// unchanged: explicit file wins, then config_dir file, then provider
    /// defaults (`None` for the default profile) or an error when the profile
    /// cannot supply a source.
    pub fn credential_path(&self, provider: ProviderId) -> Result<Option<PathBuf>, String> {
        if !self.credentials_path.trim().is_empty() {
            return expand_path(Path::new(self.credentials_path.trim()))
                .map(Some)
                .ok_or_else(|| "Use an absolute credentials file path or ~/path".into());
        }
        let directory = if !self.config_dir.trim().is_empty() {
            expand_path(Path::new(self.config_dir.trim()))
                .ok_or("Use an absolute config directory or ~/path")?
        } else if self.id.eq_ignore_ascii_case("default") {
            return Ok(None);
        } else {
            return Err(
                "Set a config directory or credentials file before enabling this account".into(),
            );
        };
        Ok(Some(directory.join(match provider {
            ProviderId::Claude => ".credentials.json",
            ProviderId::Codex => "auth.json",
            _ => return Err("This provider does not support account profiles".into()),
        })))
    }

    /// Resolved explicit Desktop user-data-dir, if configured.
    pub fn desktop_dir_resolved(&self) -> Result<Option<PathBuf>, String> {
        if self.desktop_dir.trim().is_empty() {
            return Ok(None);
        }
        expand_path(Path::new(self.desktop_dir.trim()))
            .map(Some)
            .ok_or_else(|| "Use an absolute desktop data directory or ~/path".into())
    }

    /// Resolved explicit CLI config directory, if configured.
    pub fn config_dir_resolved(&self) -> Result<Option<PathBuf>, String> {
        if self.config_dir.trim().is_empty() {
            return Ok(None);
        }
        expand_path(Path::new(self.config_dir.trim()))
            .map(Some)
            .ok_or_else(|| "Use an absolute config directory or ~/path".into())
    }

    /// Display-only rename. The stable `id` is never touched, so theme
    /// bindings and backup directories keep targeting the same account.
    pub fn rename_display(&mut self, new_name: &str) -> Result<(), String> {
        let trimmed = new_name.trim();
        if trimmed.is_empty() {
            return Err("Account name must not be empty".into());
        }
        self.name = trimmed.to_string();
        Ok(())
    }

    pub fn is_managed(&self) -> bool {
        self.ownership == AccountOwnership::Managed
    }
}

/// Explicit credential-source selection for a profile/provider pair.
///
/// Authoritative limits AND resets are produced only by the original usageBar
/// server poller for the resolved source. No switcher estimates, local-history
/// inference, weekly corrections, or manual reset overrides are permitted in
/// any authoritative usage path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    CredentialFile,
    DesktopDir,
    ProviderDefault,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_ids_are_not_reused_after_saving_and_restarting() {
        let mut accounts = ProviderAccounts::default();
        accounts.add();
        let removed_id = accounts.profiles.pop().unwrap().id;
        let json = serde_json::to_string(&accounts).unwrap();
        let mut reloaded: ProviderAccounts = serde_json::from_str(&json).unwrap();
        reloaded.add();
        assert_ne!(reloaded.profiles.last().unwrap().id, removed_id);
        assert!(reloaded.used_ids.contains(&removed_id));
    }

    #[test]
    fn legacy_ids_are_reserved_and_case_collisions_keep_the_selected_account() {
        let mut accounts: ProviderAccounts = serde_json::from_str(
            r#"{
            "profiles": [
                {"id":"Work","name":"First","config_dir":"C:/first"},
                {"id":"work","name":"Second","config_dir":"C:/second"},
                {"id":"account_1","name":"Existing"}
            ], "selected":"work"
        }"#,
        )
        .unwrap();
        accounts.normalize();
        assert_eq!(accounts.profiles[0].id, "Work");
        assert_eq!(accounts.profiles[2].id, "account_1");
        assert_ne!(accounts.profiles[1].id.to_ascii_lowercase(), "work");
        assert_eq!(accounts.selected().unwrap().name, "Second");
        let normalized = accounts.clone();
        accounts.normalize();
        assert_eq!(accounts, normalized);
        accounts.profiles.clear();
        accounts.add();
        assert!(!normalized
            .profiles
            .iter()
            .any(|old| old.id.eq_ignore_ascii_case(&accounts.profiles[0].id)));
    }

    #[test]
    fn environment_overrides_handle_empty_tilde_and_spaces_without_global_mutation() {
        assert_eq!(environment_directory_value(None), None);
        assert_eq!(environment_directory_value(Some("".into())), None);
        assert_eq!(
            environment_directory_value(Some("C:\\Users\\Two Words\\.claude-work".into())),
            Some(PathBuf::from("C:\\Users\\Two Words\\.claude-work"))
        );
        assert_eq!(
            environment_directory_value(Some("~/.codex-work".into())),
            crate::known_folders::home_dir().map(|home| home.join(".codex-work"))
        );
    }

    #[test]
    fn explicit_file_overrides_directory_and_invalid_profiles_never_use_default() {
        let profile = AccountProfile {
            id: "work".into(),
            config_dir: "C:\\work".into(),
            credentials_path: "C:\\private\\work.json".into(),
            ..Default::default()
        };
        assert_eq!(
            profile.credential_path(ProviderId::Claude).unwrap(),
            Some(PathBuf::from("C:\\private\\work.json"))
        );
        let empty = AccountProfile {
            id: "work".into(),
            ..Default::default()
        };
        assert!(empty.credential_path(ProviderId::Codex).is_err());
        assert!(expand_path(Path::new("relative/path")).is_none());
        assert_eq!(
            AccountProfile::default()
                .credential_path(ProviderId::Claude)
                .unwrap(),
            None
        );
        // Legacy/hand-edited `"Default"` still resolves as provider-default.
        let upper_default = AccountProfile {
            id: "Default".into(),
            ..Default::default()
        };
        assert_eq!(
            upper_default.credential_path(ProviderId::Claude).unwrap(),
            None
        );
        // Only Claude/Codex have provider-default credential files.
        assert_eq!(default_credential_path(ProviderId::Antigravity), None);
        assert_eq!(default_credential_path(ProviderId::OpenCode), None);
        assert!(default_credential_path(ProviderId::Claude).is_some());
        assert!(default_credential_path(ProviderId::Codex).is_some());
    }

    #[test]
    fn disabled_or_removed_selection_uses_first_enabled_account() {
        let mut accounts = ProviderAccounts::default();
        accounts.add();
        accounts.profiles[1].enabled = true;
        accounts.profiles[0].enabled = false;
        accounts.normalize();
        assert_eq!(accounts.selected, "account_1");
        accounts.profiles.clear();
        accounts.normalize();
        assert!(accounts.selected().is_none());
    }
}
