use super::*;
use crate::accounts::{fingerprint, AccountProfile, AccountSettings, SourceKind};
use crate::models::AccountUsage;
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Target {
    provider: ProviderId,
    profile: Option<AccountProfile>,
    kind: SourceKind,
    credential: Result<Option<PathBuf>, String>,
    desktop_dir: Option<PathBuf>,
    /// Stable grouping identity: same source shares one poll job, while
    /// content-sensitive rotation stays in `signature()` so only the owning
    /// account resumes.
    group: String,
}

impl Target {
    fn signature(&self) -> String {
        match &self.profile {
            None => String::new(),
            Some(profile) => profile_source_signature(self.provider, profile),
        }
    }

    fn credential_path(&self) -> Option<&Path> {
        self.credential
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(PathBuf::as_path)
    }
}

pub(super) fn poll_accounts(
    enabled: ProviderSet,
    settings: &AccountSettings,
    previous: Option<&AppUsageData>,
    force: bool,
    on_progress: impl FnMut(AppUsageData),
) -> Result<AppUsageData, PollFailure> {
    poll_accounts_with_progress(
        enabled,
        settings,
        previous,
        force,
        |target: &Target| match target.provider {
            ProviderId::Claude => match target.kind {
                SourceKind::DesktopDir => match &target.desktop_dir {
                    Some(dir) => claude::poll_desktop_dir(dir),
                    None => Err(PollError::NoCredentials),
                },
                _ => match &target.credential {
                    Err(_) => Err(PollError::NoCredentials),
                    Ok(Some(path)) => claude::poll_credential_file(
                        path,
                        target
                            .profile
                            .as_ref()
                            .map(|profile| profile.credential_targets.as_slice())
                            .unwrap_or(&[]),
                    ),
                    Ok(None) => poll_provider(target.provider),
                },
            },
            ProviderId::Codex => match &target.credential {
                Err(_) => Err(PollError::NoCredentials),
                Ok(Some(path)) => codex::poll_account(path),
                Ok(None) => poll_provider(target.provider),
            },
            _ => poll_provider(target.provider),
        },
        on_progress,
    )
}

#[cfg(test)]
fn poll_accounts_with<F>(
    enabled: ProviderSet,
    settings: &AccountSettings,
    poll: F,
) -> Result<AppUsageData, PollFailure>
where
    F: Fn(&Target) -> Result<UsageData, PollError> + Sync,
{
    poll_accounts_with_history(enabled, settings, None, false, poll)
}

#[cfg(test)]
/// Account-exclusive guards are process-wide named mutexes: serialize
/// account-poll tests so parallel tests never observe each other as busy.
static ACCOUNT_POLL_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn poll_accounts_with_history<F>(
    enabled: ProviderSet,
    settings: &AccountSettings,
    previous: Option<&AppUsageData>,
    force: bool,
    poll: F,
) -> Result<AppUsageData, PollFailure>
where
    F: Fn(&Target) -> Result<UsageData, PollError> + Sync,
{
    let _serial = ACCOUNT_POLL_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    poll_accounts_with_progress(enabled, settings, previous, force, poll, |_| {})
}

/// Resolve one enabled profile to its explicit poll source. Desktop
/// directories are Claude-only and read `config.json` + `Local State`
/// directly; every other profile keeps the legacy credential-file
/// resolution. Nothing here guesses or borrows another account's login.
fn resolve_target(provider: ProviderId, profile: &AccountProfile) -> Target {
    if !profile.desktop_dir.trim().is_empty() && provider != ProviderId::Claude {
        let error = format!(
            "desktop_dir is Claude-only; account '{}' sets it for {provider:?}",
            profile.id
        );
        return Target {
            provider,
            profile: Some(profile.clone()),
            kind: SourceKind::ProviderDefault,
            credential: Err(error.clone()),
            desktop_dir: None,
            group: format!("invalid|{}", fingerprint(&error)),
        };
    }
    let kind = profile.source_kind(provider);
    let desktop_dir = if matches!(kind, SourceKind::DesktopDir) {
        profile.desktop_dir_resolved().ok().flatten()
    } else {
        None
    };
    // Desktop profiles never consult credential files; other profiles keep
    // the legacy file/env discovery fallbacks for their own login only.
    let credential = if matches!(kind, SourceKind::DesktopDir) {
        Ok(None)
    } else {
        profile.credential_path(provider).map(|path| {
            path.or_else(|| match provider {
                ProviderId::Codex => codex::codex_auth_path(),
                ProviderId::Claude => crate::accounts::environment_directory(provider)
                    .map(|directory| directory.join(".credentials.json"))
                    .or_else(claude::native_credential_path),
                _ => None,
            })
        })
    };
    let group = group_key_for(provider, kind, &credential, &desktop_dir, profile);
    Target {
        provider,
        profile: Some(profile.clone()),
        kind,
        credential,
        desktop_dir,
        group,
    }
}

/// Stable grouping identity: paths and target names only, never file
/// contents, secrets, or write stamps, so a token rotation never splits a
/// shared source into two concurrent jobs.
fn group_key_for(
    provider: ProviderId,
    kind: SourceKind,
    credential: &Result<Option<PathBuf>, String>,
    desktop_dir: &Option<PathBuf>,
    profile: &AccountProfile,
) -> String {
    if provider == ProviderId::Claude {
        match kind {
            SourceKind::DesktopDir => match desktop_dir {
                Some(dir) => format!("claude-desktop|{}", crate::accounts::source_key(dir)),
                None => format!(
                    "claude-desktop-invalid|{}",
                    fingerprint(&profile.desktop_dir)
                ),
            },
            _ => match credential {
                Ok(Some(path)) => {
                    let mut targets = profile.credential_targets.clone();
                    targets.sort();
                    targets.dedup();
                    format!(
                        "claude-file|{}|targets:{}",
                        crate::accounts::source_key(path),
                        targets.join(",")
                    )
                }
                Ok(None) => "claude-default".to_string(),
                Err(error) => format!("claude-invalid|{}", fingerprint(error)),
            },
        }
    } else {
        match credential {
            Ok(Some(path)) => crate::accounts::source_key(path),
            other => format!("{other:?}"),
        }
    }
}

fn poll_accounts_with_progress<F>(
    enabled: ProviderSet,
    settings: &AccountSettings,
    previous: Option<&AppUsageData>,
    force: bool,
    poll: F,
    mut on_progress: impl FnMut(AppUsageData),
) -> Result<AppUsageData, PollFailure>
where
    F: Fn(&Target) -> Result<UsageData, PollError> + Sync,
{
    let mut targets = Vec::new();
    for provider in enabled.iter() {
        if let Some(accounts) = settings.get(provider) {
            for profile in accounts.profiles.iter().filter(|profile| profile.enabled) {
                targets.push(resolve_target(provider, profile));
            }
        } else {
            targets.push(Target {
                provider,
                profile: None,
                kind: SourceKind::ProviderDefault,
                credential: Ok(None),
                desktop_dir: None,
                group: format!("legacy|{provider:?}"),
            });
        }
    }
    // Profiles sharing a source must not race token refresh or credit writes.
    // Each distinct source has one job, while unrelated accounts run concurrently.
    let mut groups: Vec<Vec<Target>> = Vec::new();
    for target in targets {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].provider == target.provider && group[0].group == target.group)
        {
            group.push(target);
        } else {
            groups.push(vec![target]);
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut results = std::thread::scope(|scope| {
        let (sender, receiver) =
            std::sync::mpsc::channel::<(usize, Target, String, Result<UsageData, PollError>)>();
        for _ in 0..groups.len().min(MAX_CONCURRENT_PROVIDER_POLLS) {
            let sender = sender.clone();
            let groups = &groups;
            let next = &next;
            let poll = &poll;
            scope.spawn(move || loop {
                let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(group) = groups.get(index) else {
                    break;
                };
                let target = &group[0];
                let mut signature = target.signature();
                let paused_error = if force {
                    None
                } else {
                    group.iter().find_map(|target| {
                        previous?.auth_error_for_source(
                            target.provider,
                            target.profile.as_ref()?,
                            &signature,
                        )
                    })
                };
                // One in-flight job per account: an account whose guard is
                // busy (open/rename/backup/cleanup in progress) skips this
                // cycle with a transient error instead of borrowing a source.
                // The guards stay held through the response and the
                // source-signature check below.
                let mut _account_guards = Vec::new();
                let mut busy = false;
                for target in group.iter() {
                    if let Some(profile) = target.profile.as_ref() {
                        match crate::manager::state::AccountOperationGuard::acquire(
                            target.provider,
                            &profile.id,
                            0,
                        ) {
                            Ok(guard) => _account_guards.push(guard),
                            Err(_) => {
                                busy = true;
                                break;
                            }
                        }
                    }
                }
                let mut result = Err(paused_error.unwrap_or(PollError::RequestFailed));
                if busy {
                    diagnose::log_lazy(|| {
                        format!(
                            "{} account {} poll skipped: account busy",
                            target.provider.descriptor().display_name,
                            target
                                .profile
                                .as_ref()
                                .map(|profile| profile.name.as_str())
                                .unwrap_or("default"),
                        )
                    });
                    result = Err(PollError::RequestFailed);
                }
                diagnose::log_lazy(|| {
                    format!(
                        "{} account {} polling group={} paused={paused_error:?} busy={busy:?}",
                        target.provider.descriptor().display_name,
                        target
                            .profile
                            .as_ref()
                            .map(|profile| profile.name.as_str())
                            .unwrap_or("default"),
                        target.group
                    )
                });
                for _ in 0..if paused_error.is_some() || busy { 0 } else { 2 } {
                    result = retry_after::with_manual_refresh(force, || match &target.credential {
                        Err(_) => Err(PollError::NoCredentials),
                        Ok(_) => poll(target),
                    });
                    let current_signature = target.signature();
                    if signature == current_signature {
                        break;
                    }
                    // Re-read once after login/token rotation; never associate
                    // a response (or a stale reading) with replaced credentials.
                    signature = current_signature;
                    result = Err(PollError::RequestFailed);
                }
                for target in group {
                    let _ = sender.send((index, target.clone(), signature.clone(), result.clone()));
                }
            });
        }
        drop(sender);
        receiver
            .into_iter()
            .inspect(|(_, target, signature, result)| {
                let mut update = AppUsageData::default();
                append_account_result(&mut update, target, signature, result);
                if !update.is_empty() || !update.accounts.is_empty() {
                    on_progress(update);
                }
            })
            .collect::<Vec<_>>()
    });
    results.sort_by_key(|(index, _, _, _)| *index);
    let mut data = AppUsageData::default();
    let mut first_error = None;
    for (_, target, signature, result) in results {
        if let Ok(usage) = &result {
            diagnose::log_lazy(|| {
                format!(
                    "{} account {} usage received: session={} weekly={} stale={}",
                    target.provider.descriptor().display_name,
                    target
                        .profile
                        .as_ref()
                        .map(|profile| profile.name.as_str())
                        .unwrap_or("default"),
                    usage.session.percentage,
                    usage.weekly.percentage,
                    usage.stale
                )
            });
        }
        if let Err(error) = &result {
            crate::diagnose::log_lazy(|| {
                format!(
                    "{} account {} usage poll failed: {error:?}",
                    target.provider.descriptor().display_name,
                    target
                        .profile
                        .as_ref()
                        .map(|profile| profile.name.as_str())
                        .unwrap_or("default")
                )
            });
            first_error.get_or_insert(PollFailure {
                provider: target.provider,
                error: *error,
            });
        }
        append_account_result(&mut data, &target, &signature, &result);
    }
    data.select_accounts(settings);
    // Account failures are data too: publish their status to the dashboard,
    // and keep polling other accounts instead of pausing the whole provider.
    match first_error {
        Some(error) if data.accounts.is_empty() && data.is_empty() => Err(error),
        _ => Ok(data),
    }
}

fn append_account_result(
    data: &mut AppUsageData,
    target: &Target,
    signature: &str,
    result: &Result<UsageData, PollError>,
) {
    if let Some(profile) = &target.profile {
        data.accounts.push(AccountUsage {
            provider: target.provider,
            profile: profile.clone(),
            source_signature: signature.to_string(),
            source_path: explicit_source_path(target.provider, profile),
            usage: result.as_ref().ok().cloned(),
            error: result.as_ref().err().copied(),
            selected: false,
        });
    } else if let Ok(usage) = result {
        data.insert(target.provider, usage.clone());
    }
}

pub(super) fn carry_accounts(fresh: &mut AppUsageData, previous: &AppUsageData) {
    for account in &mut fresh.accounts {
        if account.usage.is_some() {
            continue;
        }
        // Only transient failures may keep a reading, and only for the same
        // configured source and unchanged content signature.
        if !account.error.is_some_and(PollError::is_transient) {
            continue;
        }
        if let Some(last) = previous
            .accounts
            .iter()
            .find(|last| {
                last.provider == account.provider
                    && last.profile.same_source(&account.profile)
                    && last.source_signature == account.source_signature
            })
            .and_then(|last| last.usage.as_ref())
        {
            let mut carried = last.clone();
            carried.stale = true;
            account.usage = Some(carried);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::ProviderAccounts;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn settings() -> AccountSettings {
        AccountSettings {
            claude: ProviderAccounts {
                profiles: ["personal", "work"]
                    .into_iter()
                    .map(|name| AccountProfile {
                        id: name.into(),
                        name: name.into(),
                        config_dir: format!("C:\\account-tests\\{name}"),
                        ..Default::default()
                    })
                    .collect(),
                selected: "work".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn usage(percentage: f64) -> UsageData {
        UsageData {
            session: UsageSection {
                available: true,
                percentage,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn ready_accounts_are_published_while_another_provider_is_waiting() {
        let mut settings = settings();
        // Shares the account-poll serial so parallel tests never observe
        // this test's in-flight jobs as busy (see poll_accounts_with_history).
        let _serial = ACCOUNT_POLL_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        settings.claude.profiles.truncate(1);
        settings.claude.selected = "personal".into();
        settings.codex.profiles[0].config_dir = "C:\\account-tests\\codex".into();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let mut visible = AppUsageData::default();
        let mut order = Vec::new();
        let final_data = poll_accounts_with_progress(
            ProviderSet::from_enabled([ProviderId::Claude, ProviderId::Codex]),
            &settings,
            None,
            false,
            |target| {
                if target.provider == ProviderId::Claude {
                    wait.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                Ok(usage(42.0))
            },
            |update| {
                let provider = update.accounts[0].provider;
                order.push(provider);
                visible = merge_poll_progress(update, &visible, &settings);
                if provider == ProviderId::Codex {
                    assert!(visible.get(ProviderId::Codex).is_some());
                    assert!(visible.get(ProviderId::Claude).is_none());
                    release.send(()).unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(order, [ProviderId::Codex, ProviderId::Claude]);
        for provider in [ProviderId::Claude, ProviderId::Codex] {
            assert_eq!(visible.get(provider), final_data.get(provider));
        }
    }

    #[test]
    fn partial_account_results_preserve_selection_and_error_rules() {
        let settings = settings();
        let previous =
            poll_accounts_with(ProviderSet::default(), &settings, |_| Ok(usage(25.0))).unwrap();
        let personal = previous
            .accounts
            .iter()
            .find(|account| account.profile.id == "personal")
            .unwrap()
            .clone();
        let work = previous
            .accounts
            .iter()
            .find(|account| account.profile.id == "work")
            .unwrap()
            .clone();
        let delta = |account| {
            let mut data = AppUsageData::default();
            data.accounts.push(account);
            data
        };
        // The unselected account finishing first must not replace the selected
        // account's old reading or display itself as the selected account.
        let mut refreshed = personal.clone();
        refreshed.usage = Some(usage(60.0));
        let merged = merge_poll_progress(delta(refreshed.clone()), &previous, &settings);
        assert_eq!(
            merged.get(ProviderId::Claude).unwrap().session.percentage,
            25.0
        );
        assert!(
            merge_poll_progress(delta(refreshed), &AppUsageData::default(), &settings)
                .get(ProviderId::Claude)
                .is_none()
        );
        for (error, changed, keeps_reading) in [
            (PollError::RequestFailed, false, true),
            (PollError::AuthRequired, false, false),
            (PollError::RequestFailed, true, false),
        ] {
            let mut failed = work.clone();
            failed.usage = None;
            failed.error = Some(error);
            if changed {
                failed.source_signature.push('x');
            }
            let update = merge_poll_progress(delta(failed), &merged, &settings);
            assert_eq!(update.accounts.len(), 2);
            assert_eq!(update.get(ProviderId::Claude).is_some(), keeps_reading);
            if keeps_reading {
                assert!(update.get(ProviderId::Claude).unwrap().stale);
            }
            assert_eq!(
                update
                    .accounts
                    .iter()
                    .find(|account| account.profile.id == "personal")
                    .unwrap()
                    .usage
                    .as_ref()
                    .unwrap()
                    .session
                    .percentage,
                60.0
            );
        }
    }

    #[test]
    fn auth_failures_pause_only_the_failed_account_and_notify_once() {
        let settings = settings();
        for error in [
            PollError::AuthRequired,
            PollError::TokenExpired,
            PollError::HttpStatus(401),
            PollError::HttpStatus(403),
        ] {
            let first = poll_accounts_with(ProviderSet::default(), &settings, |target| {
                if target
                    .credential_path()
                    .is_some_and(|path| path.to_string_lossy().contains("work"))
                {
                    Err(error)
                } else {
                    Ok(usage(25.0))
                }
            })
            .unwrap();
            assert_eq!(first.new_auth_failures(None, false).len(), 1);
            let calls = AtomicUsize::new(0);
            let second = poll_accounts_with_history(
                ProviderSet::default(),
                &settings,
                Some(&first),
                false,
                |target| {
                    assert!(
                        !target
                            .credential_path()
                            .is_some_and(|path| path.to_string_lossy().contains("work")),
                        "paused account must not refresh its CLI token"
                    );
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(usage(30.0))
                },
            )
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                second.accounts[0]
                    .usage
                    .as_ref()
                    .unwrap()
                    .session
                    .percentage,
                30.0
            );
            assert_eq!(second.accounts[1].error, Some(error));
            assert!(second.new_auth_failures(Some(&first), false).is_empty());

            let forced = poll_accounts_with_history(
                ProviderSet::default(),
                &settings,
                Some(&second),
                true,
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(error)
                },
            )
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            assert_eq!(forced.new_auth_failures(Some(&second), true).len(), 2);
        }
    }

    #[test]
    fn all_accounts_can_wait_for_login_and_resume_after_credential_change() {
        let directory = std::env::temp_dir().join(format!(
            "usage-auth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(".credentials.json");
        std::fs::write(&path, "old fixture").unwrap();
        let mut settings = settings();
        settings.claude.profiles.truncate(1);
        settings.claude.profiles[0].config_dir = directory.to_string_lossy().into_owned();
        let first = poll_accounts_with(ProviderSet::default(), &settings, |_| {
            Err(PollError::TokenExpired)
        })
        .unwrap();
        assert!(first.is_empty());
        let waiting = poll_accounts_with_history(
            ProviderSet::default(),
            &settings,
            Some(&first),
            false,
            |_| panic!("unchanged expired credentials must stay paused"),
        )
        .unwrap();
        assert!(waiting.new_auth_failures(Some(&first), false).is_empty());
        std::fs::write(&path, "new credentials fixture after login").unwrap();
        let resumed = poll_accounts_with_history(
            ProviderSet::default(),
            &settings,
            Some(&waiting),
            false,
            |_| Ok(usage(80.0)),
        )
        .unwrap();
        assert!(resumed.new_auth_failures(Some(&waiting), false).is_empty());
        assert_eq!(
            resumed.accounts[0]
                .usage
                .as_ref()
                .unwrap()
                .session
                .percentage,
            80.0
        );
        let failed_again = poll_accounts_with_history(
            ProviderSet::default(),
            &settings,
            Some(&resumed),
            false,
            |_| Err(PollError::AuthRequired),
        )
        .unwrap();
        assert_eq!(
            failed_again.new_auth_failures(Some(&resumed), false).len(),
            1
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn failing_selected_account_never_borrows_another_accounts_usage() {
        let settings = settings();
        let data = poll_accounts_with(ProviderSet::default(), &settings, |target| {
            if target
                .credential_path()
                .is_some_and(|path| path.to_string_lossy().contains("work"))
            {
                Err(PollError::AuthRequired)
            } else {
                Ok(usage(25.0))
            }
        })
        .unwrap();
        assert_eq!(data.accounts.len(), 2);
        assert!(data.get(ProviderId::Claude).is_none());
        assert_eq!(
            data.accounts[0].usage.as_ref().unwrap().session.percentage,
            25.0
        );
        assert_eq!(data.accounts[1].error, Some(PollError::AuthRequired));
        assert_eq!(data.selected_account_name(ProviderId::Claude), Some("work"));
    }

    #[test]
    fn stale_data_requires_the_same_profile_source_and_transient_error() {
        let settings = settings();
        let previous =
            poll_accounts_with(ProviderSet::default(), &settings, |_| Ok(usage(42.0))).unwrap();
        for (error, change_source, change_path, expected) in [
            (PollError::RequestFailed, false, false, true),
            (PollError::RequestFailed, true, false, false),
            (PollError::RequestFailed, false, true, false),
            (PollError::HttpStatus(429), false, false, true),
            (PollError::HttpStatus(503), false, false, true),
            (PollError::HttpStatus(429), true, false, false),
            (PollError::HttpStatus(429), false, true, false),
            (PollError::HttpStatus(401), false, false, false),
            (PollError::HttpStatus(403), false, false, false),
            (PollError::AuthRequired, false, false, false),
            (PollError::TokenExpired, false, false, false),
            (PollError::NoCredentials, false, false, false),
        ] {
            let mut fresh =
                poll_accounts_with(ProviderSet::default(), &settings, |_| Err(error)).unwrap();
            if change_source {
                fresh.accounts[1].source_signature.push('x');
            }
            if change_path {
                fresh.accounts[1].profile.config_dir.push('x');
            }
            carry_accounts(&mut fresh, &previous);
            assert_eq!(fresh.accounts[1].usage.is_some(), expected);
            if expected {
                assert!(fresh.accounts[1].usage.as_ref().unwrap().stale);
            }
        }
    }

    #[test]
    fn http_failure_survives_cache_and_clears_after_success() {
        let settings = settings();
        let failed = poll_accounts_with(ProviderSet::default(), &settings, |_| {
            Err(PollError::HttpStatus(429))
        })
        .unwrap();
        let cache = serde_json::to_string(&failed).unwrap();
        let failed: AppUsageData = serde_json::from_str(&cache).unwrap();
        assert_eq!(failed.accounts[0].error, Some(PollError::HttpStatus(429)));
        assert!(failed.accounts[0].usage.is_none());
        let calls = AtomicUsize::new(0);
        let recovered = poll_accounts_with_history(
            ProviderSet::default(),
            &settings,
            Some(&failed),
            false,
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(usage(35.0))
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(recovered
            .accounts
            .iter()
            .all(|account| account.error.is_none()));
        assert_eq!(
            recovered
                .get(ProviderId::Claude)
                .unwrap()
                .session
                .percentage,
            35.0
        );
        assert_eq!(
            serde_json::from_str::<PollError>("\"request_failed\"").unwrap(),
            PollError::RequestFailed
        );
    }

    #[test]
    fn cache_round_trip_selection_removal_and_path_changes_are_isolated() {
        let mut settings = settings();
        let mut data = poll_accounts_with(ProviderSet::default(), &settings, |target| {
            Ok(usage(
                if target
                    .credential_path()
                    .is_some_and(|path| path.to_string_lossy().contains("work"))
                {
                    80.0
                } else {
                    20.0
                },
            ))
        })
        .unwrap();
        let json = serde_json::to_string(&data).unwrap();
        data = serde_json::from_str(&json).unwrap();
        assert_eq!(
            data.get(ProviderId::Claude).unwrap().session.percentage,
            80.0
        );
        settings.claude.selected = "personal".into();
        data.select_accounts(&settings);
        assert_eq!(
            data.get(ProviderId::Claude).unwrap().session.percentage,
            20.0
        );
        settings.claude.profiles[0].config_dir = "C:\\a-different-account".into();
        data.select_accounts(&settings);
        assert!(data.get(ProviderId::Claude).is_none());
        assert_eq!(data.accounts.len(), 1);
        settings.claude.profiles.clear();
        data.select_accounts(&settings);
        assert!(data.is_empty());
        assert!(data.accounts.is_empty());
    }

    #[test]
    fn invalid_or_disabled_profiles_make_no_requests() {
        let mut settings = settings();
        settings.claude.profiles[0].config_dir.clear();
        settings.claude.profiles[1].enabled = false;
        let data = poll_accounts_with(ProviderSet::default(), &settings, |_| {
            panic!("invalid profile polled")
        })
        .unwrap();
        assert_eq!(data.accounts.len(), 1);
        assert_eq!(data.accounts[0].error, Some(PollError::NoCredentials));
        assert!(data.accounts[0].usage.is_none());
    }

    #[test]
    fn account_concurrency_is_bounded_and_duplicate_sources_are_polled_once() {
        let mut settings = settings();
        for index in 2..6 {
            settings.claude.profiles.push(AccountProfile {
                id: format!("profile_{index}"),
                config_dir: format!("C:\\account-tests\\{index}"),
                ..Default::default()
            });
        }
        let mut duplicate = settings.claude.profiles[0].clone();
        duplicate.id = "alias".into();
        duplicate.config_dir = duplicate.config_dir.to_uppercase();
        settings.claude.profiles.push(duplicate);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let calls = AtomicUsize::new(0);
        let data = poll_accounts_with(ProviderSet::default(), &settings, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(usage(0.0))
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        assert_eq!(data.accounts.len(), 7);
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_PROVIDER_POLLS);
    }

    #[test]
    fn credential_rotation_during_poll_retries_before_publishing() {
        let directory = std::env::temp_dir().join(format!(
            "usage-account-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(".credentials.json");
        std::fs::write(&path, "fixture before rotation").unwrap();
        let mut settings = settings();
        settings.claude.profiles.truncate(1);
        settings.claude.profiles[0].config_dir = directory.to_string_lossy().into_owned();
        let calls = AtomicUsize::new(0);
        let data = poll_accounts_with(ProviderSet::default(), &settings, |_| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(&path, "fixture after token rotation, different length").unwrap();
                Ok(usage(10.0))
            } else {
                Ok(usage(80.0))
            }
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            data.get(ProviderId::Claude).unwrap().session.percentage,
            80.0
        );
        assert_eq!(
            data.accounts[0].source_signature,
            super::super::claude::credential_file_signature(&path, &[])
        );
        let mut cached = data;
        std::fs::write(&path, "another login").unwrap();
        cached.invalidate_changed_credentials();
        cached.select_accounts(&settings);
        assert!(cached.get(ProviderId::Claude).is_none());
        assert!(cached.accounts[0].usage.is_none());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn desktop_dir_profiles_share_one_job_per_directory() {
        let root = std::env::temp_dir().join(format!(
            "usage-desktop-group-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let shared = root.join("shared");
        let other = root.join("other");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let settings = AccountSettings {
            claude: ProviderAccounts {
                profiles: vec![
                    ("first", shared.clone()),
                    ("second", shared.clone()),
                    ("third", other.clone()),
                ]
                .into_iter()
                .map(|(id, dir)| AccountProfile {
                    id: id.into(),
                    name: id.into(),
                    desktop_dir: dir.to_string_lossy().into_owned(),
                    ..Default::default()
                })
                .collect(),
                selected: "first".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let calls = AtomicUsize::new(0);
        let data = poll_accounts_with(ProviderSet::default(), &settings, |target| {
            assert_eq!(target.kind, SourceKind::DesktopDir);
            assert!(target.desktop_dir.is_some());
            assert!(target.credential_path().is_none());
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(usage(42.0))
        })
        .unwrap();
        // Two directories mean two jobs, even with three profiles.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(data.accounts.len(), 3);
        assert!(data
            .accounts
            .iter()
            .all(|account| account.usage.as_ref().is_some_and(|usage| !usage.stale)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rotation_in_one_account_does_not_resume_a_paused_sibling() {
        let root = std::env::temp_dir().join(format!(
            "usage-sibling-pause-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let personal_dir = root.join("sibling-personal");
        let work_dir = root.join("sibling-work");
        std::fs::create_dir_all(&personal_dir).unwrap();
        std::fs::create_dir_all(&work_dir).unwrap();
        let personal_file = personal_dir.join(".credentials.json");
        let work_file = work_dir.join(".credentials.json");
        std::fs::write(&personal_file, "personal login").unwrap();
        std::fs::write(&work_file, "work login").unwrap();
        let mut settings = settings();
        settings.claude.profiles[0].config_dir = personal_dir.to_string_lossy().into_owned();
        settings.claude.profiles[1].config_dir = work_dir.to_string_lossy().into_owned();
        let first = poll_accounts_with(ProviderSet::default(), &settings, |target| {
            if target
                .credential_path()
                .is_some_and(|path| path.to_string_lossy().contains("sibling-personal"))
            {
                Err(PollError::TokenExpired)
            } else {
                Ok(usage(25.0))
            }
        })
        .unwrap();
        assert_eq!(first.new_auth_failures(None, false).len(), 1);
        // The sibling rotates its own login while personal stays paused.
        std::fs::write(&work_file, "work login after rotation, longer").unwrap();
        let calls = AtomicUsize::new(0);
        let second = poll_accounts_with_history(
            ProviderSet::default(),
            &settings,
            Some(&first),
            false,
            |target| {
                let personal = target
                    .credential_path()
                    .is_some_and(|path| path.to_string_lossy().contains("sibling-personal"));
                assert!(!personal, "paused sibling must not be re-polled");
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(usage(30.0))
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            second
                .accounts
                .iter()
                .find(|account| account.profile.id == "personal")
                .unwrap()
                .error,
            Some(PollError::TokenExpired)
        );
        assert_eq!(
            second
                .accounts
                .iter()
                .find(|account| account.profile.id == "work")
                .and_then(|account| account.usage.as_ref())
                .unwrap()
                .session
                .percentage,
            30.0
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn busy_account_skips_with_a_transient_error() {
        // The account guard is held by an in-progress exclusive operation:
        // the poll job skips this account instead of borrowing a source.
        let _serial = super::ACCOUNT_POLL_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _held = crate::manager::state::AccountOperationGuard::acquire(
            ProviderId::Claude,
            "personal",
            0,
        )
        .expect("account guard acquires");
        let mut settings = settings();
        settings.claude.profiles.truncate(1);
        assert_eq!(settings.claude.profiles[0].id, "personal");
        let data = poll_accounts_with_progress(
            ProviderSet::default(),
            &settings,
            None,
            false,
            |_| panic!("busy account must not run its poll job"),
            |_| {},
        )
        .unwrap();
        assert_eq!(data.accounts.len(), 1);
        assert!(data.accounts[0].usage.is_none());
        assert_eq!(data.accounts[0].error, Some(PollError::RequestFailed));
        assert!(PollError::RequestFailed.is_transient());
    }
}
