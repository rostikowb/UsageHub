use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::diagnose;
use crate::models::{AppUsageData, UsageData, UsageSection};
use crate::providers::{ProviderId, ProviderSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PollError {
    AuthRequired,
    NoCredentials,
    TokenExpired,
    RequestFailed,
    NetworkError,
    UnexpectedResponse,
    /// Preserve the last HTTP failure so account status can explain the result.
    HttpStatus(u16),
}

impl PollError {
    pub fn is_auth(self) -> bool {
        matches!(
            self,
            Self::AuthRequired | Self::TokenExpired | Self::HttpStatus(401 | 403)
        )
    }

    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::RequestFailed
                | Self::NetworkError
                | Self::UnexpectedResponse
                | Self::HttpStatus(_)
        ) && !self.is_auth()
    }

    pub fn message(self, language: crate::localization::LanguageId) -> String {
        match self {
            Self::AuthRequired => language.text("Login rejected; sign in again").into(),
            Self::TokenExpired => language
                .text("Login expired and could not be renewed; sign in again")
                .into(),
            Self::NoCredentials => language.text("No usable login found; sign in first").into(),
            Self::RequestFailed => language
                .text("Usage request failed; retrying at the next refresh")
                .into(),
            Self::NetworkError => language
                .text("Service unreachable; retrying at the next refresh")
                .into(),
            Self::UnexpectedResponse => language
                .text("Unexpected usage response; retrying at the next refresh")
                .into(),
            Self::HttpStatus(code) => {
                let reason = ureq::http::StatusCode::from_u16(code)
                    .ok()
                    .and_then(|status| status.canonical_reason())
                    .unwrap_or("Request failed");
                let action = if self.is_auth() {
                    "Sign in again for this account"
                } else {
                    "Retrying at the next refresh"
                };
                format!("HTTP {code}: {reason}. {}", language.text(action))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWatchMode {
    ActiveSource(ProviderId),
    AllSources(ProviderId),
}

pub type CredentialWatchSnapshot = Vec<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollFailure {
    pub provider: ProviderId,
    pub error: PollError,
}

/// Content signature for one account profile: desktop-directory profiles
/// cover their `config.json` + `Local State`, credential-file profiles cover
/// the file plus confirmed credential-manager targets, and legacy
/// provider-default profiles keep the dynamic discovery snapshot. Token
/// rotation changes the signature so only the owning account resumes.
pub fn profile_source_signature(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
) -> String {
    match provider {
        ProviderId::Claude => claude::profile_signature(profile),
        _ => match profile.credential_path(provider) {
            Ok(Some(path)) => crate::accounts::file_signature(&path),
            Ok(None) => crate::accounts::fingerprint(&format!(
                "{:?}",
                credential_watch_snapshot(CredentialWatchMode::ActiveSource(provider))
            )),
            Err(error) => crate::accounts::fingerprint(&error),
        },
    }
}

/// The single explicit source file an account result points at: the desktop
/// `config.json` for desktop-directory profiles, the credentials file for
/// credential-file profiles, and none for legacy provider-default discovery.
/// Used for result provenance and cache-invalidation comparisons; it never
/// touches the network or the credential store.
pub fn explicit_source_path(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
) -> Option<std::path::PathBuf> {
    match provider {
        ProviderId::Claude if !profile.desktop_dir.trim().is_empty() => profile
            .desktop_dir_resolved()
            .ok()
            .flatten()
            .map(|dir| dir.join("config.json")),
        _ => profile.credential_path(provider).ok().flatten(),
    }
}

/// Read-only usage poll: runs the original usageBar server GET per distinct
/// account source and reports exact server percentages plus absolute reset
/// timestamps. It never writes settings, usage caches, credential files, or
/// token stores, and never refreshes or exports tokens.
pub fn poll(
    enabled_providers: ProviderSet,
    settings: &crate::accounts::AccountSettings,
    previous: Option<&AppUsageData>,
    force: bool,
    on_progress: impl FnMut(AppUsageData),
) -> Result<AppUsageData, PollFailure> {
    if enabled_providers
        .iter()
        .any(|provider| settings.get(provider).is_some())
    {
        accounts::poll_accounts(enabled_providers, settings, previous, force, on_progress)
    } else {
        poll_concurrently_with_progress(
            enabled_providers,
            |provider| retry_after::with_manual_refresh(force, || poll_provider_tracked(provider)),
            on_progress,
        )
    }
}

/// Replace only completed accounts/providers. Pending sources retain their
/// previous readings; failures use the normal stale-data and account rules.
pub fn merge_poll_progress(
    update: AppUsageData,
    previous: &AppUsageData,
    settings: &crate::accounts::AccountSettings,
) -> AppUsageData {
    let providers = ProviderSet::from_enabled(
        update
            .iter()
            .map(|(provider, _)| provider)
            .chain(update.accounts.iter().map(|account| account.provider)),
    );
    let update = carry_forward_failures(update, previous, providers);
    let mut merged = previous.clone();
    for (provider, usage) in update.iter() {
        merged.insert(provider, usage.clone());
    }
    for account in update.accounts {
        merged
            .accounts
            .retain(|old| old.provider != account.provider || old.profile.id != account.profile.id);
        merged.accounts.push(account);
    }
    merged.select_accounts(settings);
    merged
}

/// Keep the previous reading for any enabled provider that failed this cycle.
///
/// A poll succeeds as long as one provider answers, so without this a single
/// provider's outage blanks its row on every refresh while the others carry
/// on updating. The carried figures are marked stale rather than passed off as
/// current.
pub fn carry_forward_failures(
    fresh: AppUsageData,
    previous: &AppUsageData,
    enabled: ProviderSet,
) -> AppUsageData {
    let mut merged = fresh;
    accounts::carry_accounts(&mut merged, previous);
    for provider in enabled.iter() {
        if merged
            .accounts
            .iter()
            .any(|account| account.provider == provider)
            || previous
                .accounts
                .iter()
                .any(|account| account.provider == provider)
        {
            continue;
        }
        if merged.get(provider).is_some() {
            continue;
        }
        if let Some(last) = previous.get(provider) {
            let mut carried = last.clone();
            carried.mark_stale();
            merged.insert(provider, carried);
        }
    }
    merged
}

#[cfg(test)]
fn poll_with(
    enabled_providers: ProviderSet,
    mut poll_provider: impl FnMut(ProviderId) -> Result<UsageData, PollError>,
) -> Result<AppUsageData, PollFailure> {
    let results = enabled_providers
        .iter()
        .map(|provider| (provider, poll_provider(provider)))
        .collect::<Vec<_>>();
    merge_poll_results(enabled_providers, results)
}

const MAX_CONCURRENT_PROVIDER_POLLS: usize = 3;

#[cfg(test)]
fn poll_concurrently_with<F>(
    enabled_providers: ProviderSet,
    poll_provider: F,
) -> Result<AppUsageData, PollFailure>
where
    F: Fn(ProviderId) -> Result<UsageData, PollError> + Sync,
{
    poll_concurrently_with_progress(enabled_providers, poll_provider, |_| {})
}

fn poll_concurrently_with_progress<F>(
    enabled_providers: ProviderSet,
    poll_provider: F,
    mut on_progress: impl FnMut(AppUsageData),
) -> Result<AppUsageData, PollFailure>
where
    F: Fn(ProviderId) -> Result<UsageData, PollError> + Sync,
{
    let providers = enabled_providers.iter().collect::<Vec<_>>();

    let worker_count = providers.len().min(MAX_CONCURRENT_PROVIDER_POLLS);
    let next_provider = std::sync::atomic::AtomicUsize::new(0);
    let mut results = std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::channel();
        for _ in 0..worker_count {
            let sender = sender.clone();
            let providers = &providers;
            let poll_provider = &poll_provider;
            let next_provider = &next_provider;
            scope.spawn(move || loop {
                let index = next_provider.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(provider) = providers.get(index).copied() else {
                    break;
                };
                if sender.send((provider, poll_provider(provider))).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        receiver
            .into_iter()
            .map(|(provider, result)| {
                if let Ok(usage) = &result {
                    on_progress(AppUsageData::from_iter([(provider, usage.clone())]));
                }
                (provider, result)
            })
            .collect::<Vec<_>>()
    });
    results.sort_by_key(|(provider, _)| *provider);
    merge_poll_results(enabled_providers, results)
}

fn merge_poll_results(
    enabled_providers: ProviderSet,
    results: impl IntoIterator<Item = (ProviderId, Result<UsageData, PollError>)>,
) -> Result<AppUsageData, PollFailure> {
    let mut data = AppUsageData::default();
    let mut first_error = None;
    for (provider, result) in results {
        match result {
            Ok(usage) => {
                data.insert(provider, usage);
            }
            Err(error) => {
                if enabled_providers.len() > 1 {
                    diagnose::log(format!(
                        "{} usage poll failed: {error:?}",
                        provider.descriptor().display_name
                    ));
                }
                first_error.get_or_insert(PollFailure { provider, error });
            }
        }
    }

    if data.is_empty() {
        Err(first_error.unwrap_or(PollFailure {
            provider: enabled_providers.first().unwrap_or_default(),
            error: PollError::RequestFailed,
        }))
    } else {
        Ok(data)
    }
}

mod accounts;
mod antigravity;
mod claude;
mod claude_desktop;
mod codex;
mod copilot;
mod cursor;
mod grok;
mod opencode;
mod retry_after;
mod windows_credentials;

pub use retry_after::retry_delay_ms;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSourceState {
    Detected,
    Expired,
    Missing,
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialSourceStatus {
    pub state: CredentialSourceState,
    /// Non-secret human-readable source description (never tokens, emails, or raw paths).
    pub source_label: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderTelemetry {
    pub last_poll_at: Option<SystemTime>,
    pub latency_ms: Option<u64>,
    pub last_http_status: Option<u16>,
    pub last_error: Option<PollError>,
    pub retry_after_remaining: Option<Duration>,
    pub credential_status: Option<CredentialSourceStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TelemetryKey {
    pub provider: ProviderId,
    pub account_id: Option<String>,
}

impl TelemetryKey {
    pub fn new(provider: ProviderId, account_id: Option<&str>) -> Self {
        Self {
            provider,
            account_id: account_id.filter(|id| !id.is_empty()).map(str::to_owned),
        }
    }
}

#[derive(Default)]
pub struct TelemetryStore {
    entries: Mutex<HashMap<TelemetryKey, ProviderTelemetry>>,
}

impl TelemetryStore {
    pub fn record_poll(
        &self,
        provider: ProviderId,
        account_id: Option<&str>,
        finished_at: SystemTime,
        elapsed: Duration,
        http_status: Option<u16>,
        error: Option<PollError>,
        credential_status: Option<CredentialSourceStatus>,
    ) {
        let key = TelemetryKey::new(provider, account_id);
        let effective_status = http_status.or_else(|| match error {
            Some(PollError::HttpStatus(code)) => Some(code),
            _ => None,
        });
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(key).or_default();
        entry.last_poll_at = Some(finished_at);
        entry.latency_ms = Some(elapsed.as_millis().min(u64::MAX as u128) as u64);
        if effective_status.is_some() || error.is_none() {
            entry.last_http_status = effective_status;
        }
        entry.last_error = error;
        if let Some(status) = credential_status {
            entry.credential_status = Some(status);
        }
    }

    pub fn set_credential_status(
        &self,
        provider: ProviderId,
        account_id: Option<&str>,
        status: CredentialSourceStatus,
    ) {
        let key = TelemetryKey::new(provider, account_id);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.entry(key).or_default().credential_status = Some(status);
    }

    pub fn get(&self, provider: ProviderId, account_id: Option<&str>) -> ProviderTelemetry {
        let key = TelemetryKey::new(provider, account_id);
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
            .unwrap_or_default()
    }
}

pub(super) fn shared_telemetry() -> &'static TelemetryStore {
    static STORE: OnceLock<TelemetryStore> = OnceLock::new();
    STORE.get_or_init(TelemetryStore::default)
}

pub fn provider_telemetry(provider: ProviderId, account_id: Option<&str>) -> ProviderTelemetry {
    let mut telemetry = shared_telemetry().get(provider, account_id);
    telemetry.retry_after_remaining = retry_after::cooldown_remaining(provider, account_id);
    telemetry
}

pub fn refresh_credential_statuses(settings: &crate::accounts::AccountSettings) {
    for provider in ProviderId::ALL {
        let status = detect_provider_credential_status(provider);
        shared_telemetry().set_credential_status(provider, None, status);
        if let Some(accounts) = settings.get(provider) {
            for profile in &accounts.profiles {
                let account_status = detect_account_credential_status(provider, profile);
                shared_telemetry().set_credential_status(
                    provider,
                    Some(&profile.id),
                    account_status,
                );
            }
        }
    }
}

pub fn test_provider_connection(provider: ProviderId) -> Result<UsageData, PollError> {
    let credential_status = detect_provider_credential_status(provider);
    shared_telemetry().set_credential_status(provider, None, credential_status);
    retry_after::with_manual_refresh(true, || poll_provider_tracked(provider))
}

pub fn test_account_connection(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
) -> Result<UsageData, PollError> {
    let credential_status = detect_account_credential_status(provider, profile);
    shared_telemetry().set_credential_status(provider, Some(&profile.id), credential_status);
    accounts::poll_single_account(provider, profile, true)
}

fn poll_provider_tracked(provider: ProviderId) -> Result<UsageData, PollError> {
    let started = Instant::now();
    let (result, http_status) =
        retry_after::with_poll_scope(provider, None, || poll_provider(provider));
    let finished_at = SystemTime::now();
    let credential_status = Some(detect_provider_credential_status_fast(provider));
    shared_telemetry().record_poll(
        provider,
        None,
        finished_at,
        started.elapsed(),
        http_status,
        result.as_ref().err().copied(),
        credential_status,
    );
    result
}

pub fn detect_provider_credential_status(provider: ProviderId) -> CredentialSourceStatus {
    detect_provider_credential_status_inner(provider, true)
}

fn detect_provider_credential_status_fast(provider: ProviderId) -> CredentialSourceStatus {
    detect_provider_credential_status_inner(provider, false)
}

fn detect_provider_credential_status_inner(
    provider: ProviderId,
    allow_wsl: bool,
) -> CredentialSourceStatus {
    match provider {
        ProviderId::Claude => detect_claude_default_credential_status(allow_wsl),
        ProviderId::Codex => match codex::codex_auth_path() {
            Some(path) => {
                let label = if std::env::var_os("CODEX_HOME").is_some_and(|v| !v.is_empty()) {
                    "Environment variable"
                } else {
                    "Credentials file"
                };
                inspect_codex_auth_file(&path, label)
            }
            None => CredentialSourceStatus {
                state: CredentialSourceState::Missing,
                source_label: "Not found",
            },
        },
        ProviderId::Antigravity => {
            if antigravity::antigravity_credential_watch_signature().contains("|present|") {
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: "Windows Credential Manager",
                }
            } else {
                CredentialSourceStatus {
                    state: CredentialSourceState::Missing,
                    source_label: "Not found",
                }
            }
        }
        ProviderId::OpenCode => {
            let snapshot = opencode::credential_watch_snapshot(false);
            let first = snapshot.first().map(String::as_str).unwrap_or("");
            if first.starts_with("dashboard|present|") {
                let head = first.split(";;").next().unwrap_or("");
                let label = if head.ends_with("|environment") {
                    "Environment variable"
                } else {
                    "Config file"
                };
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: label,
                }
            } else {
                CredentialSourceStatus {
                    state: CredentialSourceState::Missing,
                    source_label: "Not found",
                }
            }
        }
        ProviderId::Cursor => {
            let snapshot = cursor::credential_watch_snapshot(false);
            if snapshot
                .first()
                .is_some_and(|entry| entry.starts_with("environment|present|"))
            {
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: "Environment variable",
                }
            } else if snapshot
                .get(1)
                .is_some_and(|entry| entry.contains("|present|"))
            {
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: "Local database",
                }
            } else {
                CredentialSourceStatus {
                    state: CredentialSourceState::Missing,
                    source_label: "Not found",
                }
            }
        }
        ProviderId::Grok => {
            let snapshot = grok::credential_watch_snapshot(false);
            if snapshot
                .first()
                .is_some_and(|entry| entry.contains("|present|"))
            {
                let label = if std::env::var_os("GROK_HOME").is_some_and(|v| !v.is_empty()) {
                    "Environment variable"
                } else {
                    "Credentials file"
                };
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: label,
                }
            } else {
                CredentialSourceStatus {
                    state: CredentialSourceState::Missing,
                    source_label: "Not found",
                }
            }
        }
        ProviderId::Copilot => {
            let snapshot = copilot::credential_watch_snapshot(false);
            if snapshot
                .iter()
                .take(3)
                .any(|entry| entry.contains("|present|"))
            {
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: "Environment variable",
                }
            } else if snapshot.len() > 3 {
                CredentialSourceStatus {
                    state: CredentialSourceState::Detected,
                    source_label: "Windows Credential Manager",
                }
            } else {
                CredentialSourceStatus {
                    state: CredentialSourceState::Missing,
                    source_label: "Not found",
                }
            }
        }
    }
}

pub fn detect_account_credential_status(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
) -> CredentialSourceStatus {
    match provider {
        ProviderId::Claude => {
            if !profile.desktop_dir.trim().is_empty() {
                return match profile.desktop_dir_resolved() {
                    Err(_) => CredentialSourceStatus {
                        state: CredentialSourceState::Invalid,
                        source_label: "Invalid path",
                    },
                    Ok(None) => CredentialSourceStatus {
                        state: CredentialSourceState::Missing,
                        source_label: "Not found",
                    },
                    Ok(Some(dir)) => match claude_desktop::read_token_for_dir(&dir) {
                        Some(token) if is_unix_ms_expired(token.expires_at) => {
                            CredentialSourceStatus {
                                state: CredentialSourceState::Expired,
                                source_label: "Desktop directory",
                            }
                        }
                        Some(_) => CredentialSourceStatus {
                            state: CredentialSourceState::Detected,
                            source_label: "Desktop directory",
                        },
                        None => CredentialSourceStatus {
                            state: CredentialSourceState::Missing,
                            source_label: "Not found",
                        },
                    },
                };
            }
            match profile.credential_path(ProviderId::Claude) {
                Err(_) => CredentialSourceStatus {
                    state: CredentialSourceState::Invalid,
                    source_label: "Invalid path",
                },
                Ok(None) => detect_provider_credential_status(ProviderId::Claude),
                Ok(Some(path)) => {
                    let file_status = inspect_claude_credential_file(&path);
                    if file_status.state != CredentialSourceState::Missing {
                        return file_status;
                    }
                    for target in &profile.credential_targets {
                        if let Some(secret) = windows_credentials::read_generic(target) {
                            if let Some(state) = inspect_claude_json_state(&secret) {
                                return CredentialSourceStatus {
                                    state,
                                    source_label: "Windows Credential Manager",
                                };
                            }
                        }
                    }
                    CredentialSourceStatus {
                        state: CredentialSourceState::Missing,
                        source_label: "Not found",
                    }
                }
            }
        }
        ProviderId::Codex => match profile.credential_path(ProviderId::Codex) {
            Err(_) => CredentialSourceStatus {
                state: CredentialSourceState::Invalid,
                source_label: "Invalid path",
            },
            Ok(None) => detect_provider_credential_status(ProviderId::Codex),
            Ok(Some(path)) => inspect_codex_auth_file(&path, "Credentials file"),
        },
        _ => detect_provider_credential_status(provider),
    }
}

fn detect_claude_default_credential_status(allow_wsl: bool) -> CredentialSourceStatus {
    let explicit_env = std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty());
    let file_path = if explicit_env {
        crate::accounts::environment_directory(ProviderId::Claude)
            .map(|dir| dir.join(".credentials.json"))
    } else {
        crate::known_folders::home_dir().map(|home| home.join(".claude").join(".credentials.json"))
    };
    if let Some(path) = file_path {
        let status = inspect_claude_credential_file(&path);
        if status.state != CredentialSourceState::Missing {
            return CredentialSourceStatus {
                state: status.state,
                source_label: if explicit_env {
                    "Environment variable"
                } else {
                    "Credentials file"
                },
            };
        }
    }
    if !explicit_env {
        for path in claude_desktop::config_paths() {
            if let Some(token) = claude_desktop::read_token(&path) {
                return CredentialSourceStatus {
                    state: if is_unix_ms_expired(token.expires_at) {
                        CredentialSourceState::Expired
                    } else {
                        CredentialSourceState::Detected
                    },
                    source_label: "Desktop directory",
                };
            }
        }
        if allow_wsl
            && claude::credential_watch_snapshot(false)
                .iter()
                .any(|entry| entry.starts_with("wsl:") && entry.contains("|present|"))
        {
            return CredentialSourceStatus {
                state: CredentialSourceState::Detected,
                source_label: "WSL credentials",
            };
        }
    }
    CredentialSourceStatus {
        state: CredentialSourceState::Missing,
        source_label: "Not found",
    }
}

fn inspect_claude_credential_file(path: &std::path::Path) -> CredentialSourceStatus {
    let Ok(content) = std::fs::read_to_string(path) else {
        return CredentialSourceStatus {
            state: CredentialSourceState::Missing,
            source_label: "Not found",
        };
    };
    match inspect_claude_json_state(&content) {
        Some(state) => CredentialSourceStatus {
            state,
            source_label: "Credentials file",
        },
        None => CredentialSourceStatus {
            state: CredentialSourceState::Missing,
            source_label: "Not found",
        },
    }
}

fn inspect_claude_json_state(content: &str) -> Option<CredentialSourceState> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    let oauth = json.get("claudeAiOauth")?;
    let token = oauth
        .get("accessToken")?
        .as_str()
        .filter(|token| !token.trim().is_empty())?;
    let _ = token;
    let expires_at = oauth.get("expiresAt").and_then(|value| value.as_i64());
    Some(if is_unix_ms_expired(expires_at) {
        CredentialSourceState::Expired
    } else {
        CredentialSourceState::Detected
    })
}

fn inspect_codex_auth_file(path: &std::path::Path, label: &'static str) -> CredentialSourceStatus {
    let Ok(content) = std::fs::read_to_string(path) else {
        return CredentialSourceStatus {
            state: CredentialSourceState::Missing,
            source_label: "Not found",
        };
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return CredentialSourceStatus {
            state: CredentialSourceState::Invalid,
            source_label: label,
        };
    };
    let has_token = json
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|value| value.as_str())
        .is_some_and(|token| !token.trim().is_empty());
    if has_token {
        CredentialSourceStatus {
            state: CredentialSourceState::Detected,
            source_label: label,
        }
    } else {
        CredentialSourceStatus {
            state: CredentialSourceState::Missing,
            source_label: "Not found",
        }
    }
}

fn is_unix_ms_expired(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|expires_at| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        now >= expires_at
    })
}

struct ProviderPoller {
    id: ProviderId,
    poll: fn() -> Result<UsageData, PollError>,
    credential_watch: fn(bool) -> CredentialWatchSnapshot,
}

const PROVIDER_POLLERS: [ProviderPoller; 7] = [
    ProviderPoller {
        id: ProviderId::Claude,
        poll: claude::poll_claude_code,
        credential_watch: claude::credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::Codex,
        poll: codex::poll_codex,
        credential_watch: codex_credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::Antigravity,
        poll: antigravity::poll_antigravity,
        credential_watch: antigravity_credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::OpenCode,
        poll: opencode::poll_opencode,
        credential_watch: opencode::credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::Cursor,
        poll: cursor::poll_cursor,
        credential_watch: cursor::credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::Grok,
        poll: grok::poll_grok,
        credential_watch: grok::credential_watch_snapshot,
    },
    ProviderPoller {
        id: ProviderId::Copilot,
        poll: copilot::poll_copilot,
        credential_watch: copilot::credential_watch_snapshot,
    },
];

fn provider_poller(provider: ProviderId) -> Option<&'static ProviderPoller> {
    PROVIDER_POLLERS.iter().find(|poller| poller.id == provider)
}

fn poll_provider(provider: ProviderId) -> Result<UsageData, PollError> {
    provider_poller(provider)
        .ok_or(PollError::RequestFailed)
        .and_then(|poller| (poller.poll)())
}

pub fn credential_watch_snapshot(mode: CredentialWatchMode) -> CredentialWatchSnapshot {
    let (provider, all_sources) = match mode {
        CredentialWatchMode::ActiveSource(provider) => (provider, false),
        CredentialWatchMode::AllSources(provider) => (provider, true),
    };
    provider_poller(provider)
        .map(|poller| (poller.credential_watch)(all_sources))
        .unwrap_or_default()
}

fn codex_credential_watch_snapshot(_all_sources: bool) -> CredentialWatchSnapshot {
    codex::credential_watch_snapshot()
}

fn antigravity_credential_watch_snapshot(_all_sources: bool) -> CredentialWatchSnapshot {
    vec![antigravity::antigravity_credential_watch_signature()]
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    static AGENT: OnceLock<Result<ureq::Agent, PollError>> = OnceLock::new();
    // Agent clones share their connection pool, cookies, and TLS configuration.
    AGENT
        .get_or_init(|| {
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            Ok(ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .tls_config(tls)
                // Inspect Retry-After before converting error statuses below.
                .http_status_as_error(false)
                .middleware(retry_after::middleware)
                .build()
                .into())
        })
        .clone()
}

/// Loopback fixtures must not inherit the process proxy: ureq resolves a
/// proxy from the environment by default, which would intercept 127.0.0.1
/// traffic and fail the request before the fixture sees it (the retry_after
/// fixture already bypasses it per request with `.proxy(None)`). Tests inject
/// this agent so they exercise the real request/auth/parse path with no
/// production localhost special-casing.
#[cfg(test)]
pub(super) fn build_loopback_test_agent() -> Result<ureq::Agent, PollError> {
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .tls_config(tls)
        // Inspect Retry-After before converting error statuses below.
        .http_status_as_error(false)
        .middleware(retry_after::middleware)
        .proxy(None)
        .build()
        .into())
}

type HttpResponse = ureq::http::Response<ureq::Body>;

fn check_http_status(response: HttpResponse) -> Result<HttpResponse, ureq::Error> {
    let status = response.status();
    if status.is_client_error() || status.is_server_error() {
        Err(ureq::Error::StatusCode(status.as_u16()))
    } else {
        Ok(response)
    }
}

fn unix_to_system_time(unix_secs: Option<i64>) -> Option<SystemTime> {
    let secs = unix_secs?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Parse an ISO 8601 timestamp string into a SystemTime.
fn parse_iso8601(s: Option<&str>) -> Option<SystemTime> {
    let unix_secs = parse_datetime_to_unix(s?)?;
    UNIX_EPOCH.checked_add(Duration::from_secs(unix_secs))
}

/// Minimal datetime parser — avoids pulling in chrono/time crates.
fn parse_datetime_to_unix(s: &str) -> Option<u64> {
    let (datetime, offset_seconds) = split_timezone(s)?;
    let datetime = match datetime.split_once('.') {
        Some((base, fraction))
            if !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        Some(_) => return None,
        None => datetime,
    };
    let bytes = datetime.as_bytes();
    if bytes.len() != 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }

    let year = parse_digits(&bytes[0..4])?;
    let month = parse_digits(&bytes[5..7])?;
    let day = parse_digits(&bytes[8..10])?;
    let hour = parse_digits(&bytes[11..13])?;
    let minute = parse_digits(&bytes[14..16])?;
    let second = parse_digits(&bytes[17..19])?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }

    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }

    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    let local_seconds = days
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + minute * 60 + second)?;
    u64::try_from(
        i64::try_from(local_seconds)
            .ok()?
            .checked_sub(offset_seconds)?,
    )
    .ok()
}

fn split_timezone(s: &str) -> Option<(&str, i64)> {
    if let Some(datetime) = s.strip_suffix('Z') {
        return Some((datetime, 0));
    }

    if s.len() >= 25 {
        let offset_start = s.len() - 6;
        let offset = &s.as_bytes()[offset_start..];
        if matches!(offset[0], b'+' | b'-') && offset[3] == b':' {
            let hours = parse_digits(&offset[1..3])?;
            let minutes = parse_digits(&offset[4..6])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let seconds = i64::try_from(hours * 3_600 + minutes * 60).ok()?;
            return Some((
                &s[..offset_start],
                if offset[0] == b'+' { seconds } else { -seconds },
            ));
        }
    }

    Some((s, 0))
}

fn parse_digits(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
    })
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if is_leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    }
}

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

/// Calculate how long until the display text would change
pub fn time_until_display_change(resets_at: Option<SystemTime>) -> Option<Duration> {
    let reset = resets_at?;
    let remaining = reset.duration_since(SystemTime::now()).ok()?;
    Some(time_until_display_change_from_secs(remaining.as_secs()))
}

fn time_until_display_change_from_secs(total_secs: u64) -> Duration {
    let total_mins = total_secs / 60;
    let total_hours = total_secs / 3600;
    let total_days = total_secs / 86400;

    let current_bucket_start = if total_days >= 1 {
        total_days * 86400
    } else if total_hours >= 1 {
        total_hours * 3600
    } else if total_mins >= 1 {
        total_mins * 60
    } else {
        total_secs
    };

    Duration::from_secs(total_secs.saturating_sub(current_bucket_start) + 1)
}

/// Returns true if a reported usage window has reached its reset time.
pub fn is_past_reset(data: &UsageData) -> bool {
    if data.stale {
        return false;
    }
    let now = SystemTime::now();
    let past = |s: &UsageSection| matches!(s.resets_at, Some(t) if now.duration_since(t).is_ok());
    data.sections().any(past)
}

pub fn app_is_past_reset(data: &AppUsageData) -> bool {
    data.all_usage().any(is_past_reset)
}

#[cfg(test)]
mod tests;
