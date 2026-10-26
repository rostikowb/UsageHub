use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::claude_desktop;
use super::{build_agent, parse_iso8601, PollError};
use crate::diagnose;
use crate::models::{CreditsSection, UsageData};

mod limits;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageBucket>,
    seven_day: Option<UsageBucket>,
    spend: Option<SpendResponse>,
    limits: Option<Vec<serde_json::Value>>,
    #[serde(flatten)]
    extra: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Paid credits that carry the account past its plan limits. Amounts are
/// minor units with their own exponent, so the currency is self-describing.
#[derive(Deserialize)]
struct SpendResponse {
    #[serde(default)]
    enabled: bool,
    used: Option<SpendAmount>,
    limit: Option<SpendAmount>,
}

#[derive(Deserialize)]
struct SpendAmount {
    amount_minor: f64,
    #[serde(default)]
    exponent: u32,
}

impl SpendAmount {
    fn major(&self) -> f64 {
        self.amount_minor / 10f64.powi(self.exponent as i32)
    }
}

#[derive(Deserialize)]
struct UsageBucket {
    utilization: f64,
    resets_at: Option<String>,
}

struct Credentials {
    access_token: String,
    expires_at: Option<i64>,
    source: CredentialSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CredentialSource {
    Windows(PathBuf),
    /// A confirmed Windows Credential Manager target owned by one account
    /// profile (`AccountProfile::credential_targets`). Only listed targets
    /// are ever read; nothing is guessed or enumerated by prefix.
    CredentialManager(String),
    /// The Claude desktop app's own token cache, used when Claude Code has
    /// only ever run inside the desktop app and no CLI login wrote
    /// `~/.claude/.credentials.json`.
    DesktopApp(PathBuf),
    Wsl {
        distro: String,
    },
}

pub(super) fn poll_claude_code() -> Result<UsageData, PollError> {
    let creds = match read_first_credentials() {
        Some(c) => c,
        None => {
            diagnose::log("poll failed: no Claude credentials found");
            return Err(PollError::NoCredentials);
        }
    };

    if is_token_expired(creds.expires_at) {
        return Err(PollError::TokenExpired);
    }

    fetch_usage(&creds.access_token)
}

/// Named credential-file profiles are pinned to their own login material. An
/// expired login reports `TokenExpired` instead of borrowing another
/// account's token: there is no fallback to the default CLI path, the global
/// discovery order, or another profile's desktop directory.
///
/// Resolution order: the profile's own credentials file, then — only when
/// the file holds no usable token — the profile's confirmed
/// `credential_targets` through the Windows Credential Manager. Desktop
/// logins are reachable exclusively through `SourceKind::DesktopDir` profiles
/// (`poll_desktop_dir`), never implicitly from here.
pub(super) fn poll_credential_file(
    path: &Path,
    credential_targets: &[String],
) -> Result<UsageData, PollError> {
    if let Some(credentials) =
        read_credentials_from_source(&CredentialSource::Windows(path.to_path_buf()))
    {
        if is_token_expired(credentials.expires_at) {
            return Err(PollError::TokenExpired);
        }
        return fetch_usage(&credentials.access_token);
    }
    for target in credential_targets {
        let Some(credentials) = read_named_credential(target) else {
            continue;
        };
        if is_token_expired(credentials.expires_at) {
            return Err(PollError::TokenExpired);
        }
        return fetch_usage(&credentials.access_token);
    }
    Err(PollError::NoCredentials)
}

/// Explicit desktop-directory profiles read the original Rust OSCrypt reader
/// over `config.json` + `Local State` in that directory only, then run the
/// unchanged usageBar server GET parser. No CLI credential discovery, no
/// other directory's cache, no token refresh.
pub(super) fn poll_desktop_dir(dir: &Path) -> Result<UsageData, PollError> {
    let token = claude_desktop::read_token_for_dir(dir).ok_or_else(|| {
        diagnose::log(format!(
            "no usable Claude desktop login in {}",
            dir.display()
        ));
        PollError::NoCredentials
    })?;
    if is_token_expired(token.expires_at) {
        return Err(PollError::TokenExpired);
    }
    fetch_usage(&token.access_token)
}

/// Read one confirmed credential-manager target owned by the calling profile.
/// Unknown or unreadable targets behave like a missing file: the caller
/// moves on without borrowing anything else.
fn read_named_credential(target: &str) -> Option<Credentials> {
    let secret = super::windows_credentials::read_generic(target)?;
    parse_credentials(
        &secret,
        CredentialSource::CredentialManager(target.to_string()),
    )
}

/// Content signature for one named Claude profile: its credentials file plus
/// the last-write stamps of its confirmed credential-manager targets. Token
/// rotation changes the signature so only the owning account resumes; other
/// accounts keep their own signatures and stay paused or live independently.
pub(super) fn credential_file_signature(path: &Path, credential_targets: &[String]) -> String {
    let mut signature = format!("file:{}", crate::accounts::file_signature(path));
    let mut targets: Vec<&str> = credential_targets.iter().map(String::as_str).collect();
    targets.sort_unstable();
    targets.dedup();
    for target in targets {
        match super::windows_credentials::last_written(target) {
            Some(stamp) => signature.push_str(&format!("|credman:{target}|{stamp}")),
            None => signature.push_str(&format!("|credman:{target}|missing")),
        }
    }
    crate::accounts::fingerprint(&signature)
}

/// Full content signature for a Claude account profile: desktop-directory
/// profiles cover `config.json` + `Local State`, credential-file profiles
/// cover the file + confirmed credential-manager targets, and the legacy
/// provider-default profile keeps the dynamic global-discovery snapshot so a
/// fresh login still resumes polling.
pub(super) fn profile_signature(profile: &crate::accounts::AccountProfile) -> String {
    if !profile.desktop_dir.trim().is_empty() {
        return match profile.desktop_dir_resolved() {
            Ok(Some(dir)) => claude_desktop::signature_for_dir(&dir),
            _ => crate::accounts::fingerprint("desktop-dir|unresolved"),
        };
    }
    match profile.credential_path(crate::providers::ProviderId::Claude) {
        Ok(Some(path)) => credential_file_signature(&path, &profile.credential_targets),
        Ok(None) => {
            crate::accounts::fingerprint(&format!("{:?}", credential_watch_snapshot(false)))
        }
        Err(error) => crate::accounts::fingerprint(&error),
    }
}

/// Ordinary monitoring reads the dedicated usage endpoint exactly as returned:
/// no model prompts, no quota probes, no CLI refresh. A missing reset stays
/// unknown (`None`); only the server's own timestamps are reported. Any
/// endpoint failure surfaces explicitly with its own status.
pub(super) fn fetch_usage(token: &str) -> Result<UsageData, PollError> {
    let agent = build_agent()?;

    let mut resp = match agent
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .call()
        .and_then(super::check_http_status)
    {
        Ok(resp) => resp,
        Err(error) => {
            diagnose::log(format!("claude usage endpoint request failed ({error})"));
            return Err(usage_request_error(&error));
        }
    };

    parse_usage_body(resp.body_mut())
}

fn parse_usage_body(body: &mut ureq::Body) -> Result<UsageData, PollError> {
    let response: UsageResponse = match body.read_json() {
        Ok(response) => response,
        Err(error) => {
            diagnose::log_error("unexpected Claude usage response", error);
            return Err(PollError::UnexpectedResponse);
        }
    };
    validated_usage_from_response(response)
}

fn validated_usage_from_response(response: UsageResponse) -> Result<UsageData, PollError> {
    let data = usage_from_response(response);
    if !data.sections().any(|section| section.available) {
        diagnose::log("unexpected Claude usage response: no usable usage limits");
        return Err(PollError::UnexpectedResponse);
    }
    Ok(data)
}

fn usage_from_response(response: UsageResponse) -> UsageData {
    let mut data = UsageData {
        limits: limits::parse(
            response.limits.as_deref().unwrap_or_default(),
            &response.extra,
        ),
        ..Default::default()
    };

    if let Some(bucket) = &response.five_hour {
        data.session.available = true;
        data.session.percentage = bucket.utilization;
        data.session.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    if let Some(bucket) = &response.seven_day {
        data.weekly.available = true;
        data.weekly.percentage = bucket.utilization;
        data.weekly.resets_at = parse_iso8601(bucket.resets_at.as_deref());
    }

    // New-format responses may omit the legacy fields. Scoped quotas never
    // replace the all-model session or weekly values used by built-in themes.
    for limit in &data.limits {
        if limit.scope.is_none() {
            match limit.kind.as_str() {
                "session" if !data.session.available => data.session = limit.usage.clone(),
                "weekly_all" if !data.weekly.available => data.weekly = limit.usage.clone(),
                _ => {}
            }
        }
    }

    data.credits = response
        .spend
        .as_ref()
        .and_then(|spend| claude_credits(spend, &data));

    data
}

fn usage_request_error(error: &ureq::Error) -> PollError {
    match error {
        ureq::Error::StatusCode(status) => PollError::HttpStatus(*status),
        _ => PollError::NetworkError,
    }
}

/// Unlike Codex, the plan states its own ceiling, so the gauge needs no
/// history: `used` is already the spend against the current cap, and a
/// non-zero figure is the same "credits are in play" observation that the
/// Codex balance gives by falling. Accounts with extra usage switched off
/// report it disabled and get no gauge rather than an empty one.
fn claude_credits(spend: &SpendResponse, data: &UsageData) -> Option<CreditsSection> {
    let used = spend.used.as_ref()?.major();
    let total = spend.limit.as_ref()?.major();
    if !spend.enabled || !total.is_finite() || total <= 0.0 {
        return None;
    }

    // Hold the ordinary windows until one of them is spent and credits have
    // started covering the overflow.
    let limit_reached = data.session.percentage >= 100.0 || data.weekly.percentage >= 100.0;
    if !limit_reached || used <= 0.0 {
        return None;
    }

    Some(CreditsSection {
        percentage: ((used / total) * 100.0).clamp(0.0, 100.0),
        remaining: (total - used).max(0.0),
        total,
    })
}

pub(super) fn credential_watch_snapshot(all_sources: bool) -> Vec<String> {
    let sources = if all_sources {
        all_known_credential_sources()
    } else {
        read_first_credentials()
            .map(|credentials| vec![credentials.source])
            .unwrap_or_else(all_known_credential_sources)
    };

    let mut snapshot: Vec<String> = sources
        .into_iter()
        .filter_map(|source| credential_watch_signature(&source))
        .collect();
    snapshot.sort();
    snapshot.dedup();
    snapshot
}

fn read_first_credentials() -> Option<Credentials> {
    credential_sources_in_order().find_map(|source| read_credentials_from_source(&source))
}

fn read_windows_credentials(path: &Path) -> Option<Credentials> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!("unable to read Windows credentials at {}", path.display()),
                    error,
                );
            }
            return None;
        }
    };
    parse_credentials(&content, CredentialSource::Windows(path.to_path_buf()))
}

fn read_desktop_app_credentials(path: &Path) -> Option<Credentials> {
    let token = claude_desktop::read_token(path)?;
    diagnose::log("using the Claude desktop app token cache");
    Some(Credentials {
        access_token: token.access_token,
        expires_at: token.expires_at,
        source: CredentialSource::DesktopApp(path.to_path_buf()),
    })
}

fn read_credentials_from_source(source: &CredentialSource) -> Option<Credentials> {
    match source {
        CredentialSource::Windows(path) => read_windows_credentials(path),
        CredentialSource::CredentialManager(target) => read_named_credential(target),
        CredentialSource::DesktopApp(path) => read_desktop_app_credentials(path),
        CredentialSource::Wsl { distro } => read_wsl_credentials(distro),
    }
}

fn read_wsl_credentials(distro: &str) -> Option<Credentials> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg("cat ~/.claude/.credentials.json"),
        Duration::from_secs(5),
    )?;

    if !output.status.success() {
        diagnose::log(format!(
            "WSL credentials probe failed for distro {distro} with status {}",
            output.status
        ));
        return None;
    }

    let content = String::from_utf8(output.stdout).ok()?;
    parse_credentials(
        &content,
        CredentialSource::Wsl {
            distro: distro.to_string(),
        },
    )
}

fn parse_credentials(content: &str, source: CredentialSource) -> Option<Credentials> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;
    let oauth = json.get("claudeAiOauth")?;
    Some(Credentials {
        access_token: oauth
            .get("accessToken")?
            .as_str()
            .filter(|token| !token.trim().is_empty())?
            .to_string(),
        expires_at: oauth.get("expiresAt").and_then(|value| value.as_i64()),
        source,
    })
}

/// Credential sources, cheapest first. The WSL probe stays lazy so a machine
/// that resolves a token locally never has to spawn `wsl.exe`.
fn credential_sources_in_order() -> impl Iterator<Item = CredentialSource> {
    let explicit = std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|value| !value.is_empty());
    windows_credential_source()
        .into_iter()
        .chain(
            (!explicit)
                .then(claude_desktop::config_paths)
                .into_iter()
                .flatten()
                .map(CredentialSource::DesktopApp),
        )
        .chain(
            std::iter::once_with(move || {
                if explicit {
                    Vec::new()
                } else {
                    list_wsl_distros()
                }
            })
            .flatten()
            .map(|distro| CredentialSource::Wsl { distro }),
        )
}

fn all_known_credential_sources() -> Vec<CredentialSource> {
    credential_sources_in_order().collect()
}

fn windows_credential_source() -> Option<CredentialSource> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|value| !value.is_empty()) {
        return crate::accounts::environment_directory(crate::providers::ProviderId::Claude)
            .map(|directory| CredentialSource::Windows(directory.join(".credentials.json")));
    }
    Some(CredentialSource::Windows(
        crate::known_folders::home_dir()?
            .join(".claude")
            .join(".credentials.json"),
    ))
}

pub(super) fn native_credential_path() -> Option<PathBuf> {
    match windows_credential_source()? {
        CredentialSource::Windows(path) if read_windows_credentials(&path).is_some() => Some(path),
        _ => None,
    }
}

fn credential_watch_signature(source: &CredentialSource) -> Option<String> {
    match source {
        CredentialSource::Windows(path) => Some(windows_credential_watch_signature(path)),
        CredentialSource::CredentialManager(target) => {
            Some(credential_manager_watch_signature(target))
        }
        CredentialSource::DesktopApp(path) => Some(claude_desktop::watch_signature(path)),
        CredentialSource::Wsl { distro } => wsl_credential_watch_signature(distro),
    }
}

fn credential_manager_watch_signature(target: &str) -> String {
    match super::windows_credentials::last_written(target) {
        Some(stamp) => format!("credman:{target}|{stamp}"),
        None => format!("credman:{target}|missing"),
    }
}

fn windows_credential_watch_signature(path: &PathBuf) -> String {
    let key = format!("win:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}

fn wsl_credential_watch_signature(distro: &str) -> Option<String> {
    let output = run_with_timeout(
        Command::new("wsl.exe")
            .arg("-d")
            .arg(distro)
            .arg("--")
            .arg("sh")
            .arg("-lc")
            .arg(
                "if [ -f ~/.claude/.credentials.json ]; then stat -c 'present|%s|%Y' ~/.claude/.credentials.json; else echo missing; fi",
            ),
        Duration::from_secs(5),
    )?;
    let state = if output.status.success() {
        decode_wsl_text(&output.stdout).trim().to_string()
    } else {
        format!("status-{}", output.status)
    };
    Some(format!("wsl:{distro}|{state}"))
}

fn list_wsl_distros() -> Vec<String> {
    let output = match run_with_timeout(
        Command::new("wsl.exe").args(["-l", "-q"]),
        Duration::from_secs(5),
    ) {
        Some(output) if output.status.success() => output,
        _ => {
            diagnose::log("unable to enumerate WSL distros");
            return Vec::new();
        }
    };
    decode_wsl_text(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn decode_wsl_text(bytes: &[u8]) -> String {
    decode_utf16le(bytes).unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

fn decode_utf16le(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let body = if bytes.starts_with(&[0xFF, 0xFE]) {
        &bytes[2..]
    } else if looks_like_utf16le(bytes) {
        bytes
    } else {
        return None;
    };
    Some(String::from_utf16_lossy(
        &body
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>(),
    ))
}

fn looks_like_utf16le(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(128);
    let units = sample_len / 2;
    units > 0
        && bytes[..sample_len]
            .chunks_exact(2)
            .filter(|chunk| chunk[1] == 0)
            .count()
            * 2
            >= units
}

fn is_token_expired(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|expires_at| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        now >= expires_at
    })
}

fn run_with_timeout(command: &mut Command, timeout: Duration) -> Option<std::process::Output> {
    // Shared bounded runner owns the deadline, the concurrent pipe drain and
    // the kill-on-timeout job tree. 1MiB cap: credential JSON is tiny, so a
    // capped (possibly truncated) buffer is never parsed as credentials.
    const CREDENTIAL_CAPTURE_CAP: usize = 1024 * 1024;
    match crate::manager::process::run_bounded(
        command,
        timeout,
        CREDENTIAL_CAPTURE_CAP,
        "claude credentials",
    ) {
        Ok(output)
            if output.stdout.len() < CREDENTIAL_CAPTURE_CAP
                && output.stderr.len() < CREDENTIAL_CAPTURE_CAP =>
        {
            Some(output)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_failures_keep_their_status_for_account_display() {
        for status in [401, 403, 429, 500, 503] {
            assert_eq!(
                usage_request_error(&ureq::Error::StatusCode(status)),
                PollError::HttpStatus(status)
            );
        }
    }

    /// Ignored by default: proves an explicit desktop-directory profile
    /// resolves usage on a machine where only the desktop app holds a token.
    /// Run it with `cargo test -- --ignored` while signed in to the desktop app.
    #[test]
    #[ignore = "requires a signed-in Claude desktop app on this machine"]
    fn desktop_dir_profile_resolves_usage_from_the_desktop_app() {
        let dir = claude_desktop::data_directories()
            .into_iter()
            .find(|dir| claude_desktop::config_path_for_dir(dir).is_file())
            .expect("a Claude desktop data directory with a config file");
        let outcome = poll_desktop_dir(&dir);
        assert!(
            outcome.is_ok(),
            "the desktop profile should resolve usage from the desktop app, got {outcome:?}"
        );
    }

    #[test]
    fn credential_file_polls_never_reach_another_login() {
        // Missing files and unknown credential-manager targets report their
        // own status. Nothing falls through to the desktop app, the global
        // discovery order, or another account's login.
        let path = std::env::temp_dir().join("claude-pinned-export.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            poll_credential_file(&path, &[]),
            Err(PollError::NoCredentials)
        );
        assert_eq!(
            poll_credential_file(
                &path,
                &["UsageHub-test-nonexistent-target-9f3c4a".to_string()]
            ),
            Err(PollError::NoCredentials)
        );
    }

    #[test]
    fn desktop_dir_signature_ignores_window_state_and_tracks_rotation() {
        use crate::accounts::AccountProfile;
        let directory = std::env::temp_dir().join(format!(
            "claude-desktop-sig-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let desktop = directory.join("config.json");
        let profile = AccountProfile {
            desktop_dir: directory.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let missing = profile_signature(&profile);
        std::fs::write(
            &desktop,
            r#"{"oauth:tokenCache":"legacy","oauth:tokenCacheV2":"first","window":1}"#,
        )
        .unwrap();
        let logged_in = profile_signature(&profile);
        assert_ne!(missing, logged_in);
        // Unrelated window-state rewrites keep the signature.
        std::fs::write(
            &desktop,
            r#"{"oauth:tokenCache":"legacy","oauth:tokenCacheV2":"first","window":2}"#,
        )
        .unwrap();
        assert_eq!(logged_in, profile_signature(&profile));
        // Token rotation changes it so only this account resumes.
        std::fs::write(
            &desktop,
            r#"{"oauth:tokenCache":"legacy","oauth:tokenCacheV2":"rotated","window":2}"#,
        )
        .unwrap();
        assert_ne!(logged_in, profile_signature(&profile));
        std::fs::remove_file(&desktop).unwrap();
        assert_eq!(missing, profile_signature(&profile));
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn desktop_dir_profiles_are_independent_sources() {
        use crate::accounts::AccountProfile;
        let root = std::env::temp_dir().join(format!(
            "claude-desktop-independence-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for dir in [&first, &second] {
            std::fs::write(
                dir.join("config.json"),
                r#"{"oauth:tokenCacheV2":"shared-login"}"#,
            )
            .unwrap();
        }
        let first_profile = AccountProfile {
            desktop_dir: first.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let second_profile = AccountProfile {
            desktop_dir: second.to_string_lossy().into_owned(),
            ..Default::default()
        };
        // Path-bound signatures never alias two directories ...
        assert_ne!(
            profile_signature(&first_profile),
            profile_signature(&second_profile)
        );
        // ... and rotation in one leaves the other untouched.
        std::fs::write(
            first.join("config.json"),
            r#"{"oauth:tokenCacheV2":"rotated-first-login"}"#,
        )
        .unwrap();
        let unchanged = profile_signature(&second_profile);
        assert_ne!(profile_signature(&first_profile), unchanged);
        assert_eq!(profile_signature(&second_profile), unchanged);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn credential_file_signature_tracks_file_and_confirmed_targets() {
        use crate::accounts::AccountProfile;
        let directory = std::env::temp_dir().join(format!(
            "claude-file-sig-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(".credentials.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"a"}}"#).unwrap();
        let mut profile = AccountProfile {
            config_dir: directory.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let before = profile_signature(&profile);
        // Longer replacement content changes file metadata deterministically.
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"a-longer-token"}}"#,
        )
        .unwrap();
        assert_ne!(before, profile_signature(&profile));
        // A confirmed target joins the signature even while it is missing,
        // so a later login under that target resumes this account.
        profile
            .credential_targets
            .push("UsageHub-test-nonexistent-target-9f3c4a".to_string());
        let with_target = profile_signature(&profile);
        assert_ne!(before, with_target);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn explicit_missing_or_expired_export_never_uses_another_login() {
        let directory = std::env::temp_dir().join(format!(
            "claude-profile-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("export.json");
        assert_eq!(
            poll_credential_file(&path, &[]),
            Err(PollError::NoCredentials)
        );
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"fixture-token","expiresAt":0}}"#,
        )
        .unwrap();
        assert_eq!(
            poll_credential_file(&path, &[]),
            Err(PollError::TokenExpired)
        );
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn malformed_usage_responses_are_distinct_from_network_and_auth_failures() {
        for json in [
            "not json",
            "{}",
            r#"{"five_hour":{"utilization":"wrong"}}"#,
            r#"{"limits":[{"kind":"weekly_scoped"}]}"#,
        ] {
            let mut body = ureq::Body::builder().data(json.as_bytes().to_vec());
            assert_eq!(
                parse_usage_body(&mut body),
                Err(PollError::UnexpectedResponse)
            );
        }
        assert!(PollError::UnexpectedResponse.is_transient());
        assert!(!PollError::UnexpectedResponse.is_auth());
    }

    #[test]
    fn array_only_usage_fills_standard_windows_without_using_scoped_caps() {
        let mut body = ureq::Body::builder().data(br#"{"limits":[
            {"kind":"session","percent":29},
            {"kind":"weekly_all","percent":26},
            {"kind":"weekly_scoped","percent":99,"is_active":true,"scope":{"model":{"display_name":"Fable"}}}
        ]}"#.to_vec());
        let data = parse_usage_body(&mut body).unwrap();
        assert_eq!(data.session.percentage, 29.0);
        assert_eq!(data.weekly.percentage, 26.0);
        assert_eq!(data.limits.len(), 3);
        assert!(data.session.available && data.weekly.available);
        let data = usage_from_json(
            r#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"limits":[{"kind":"session","percent":90},{"kind":"weekly_all","percent":95}]}"#,
        );
        assert_eq!(data.session.percentage, 10.0);
        assert_eq!(data.weekly.percentage, 20.0);
    }

    #[test]
    fn scoped_limits_survive_cache_and_reach_custom_theme_bindings() {
        use crate::providers::ProviderId;
        use crate::theme_engine::{evaluate, format_template, Canvas, DataContext, ThemeRuntime};
        let usage = usage_from_json(
            r#"{"five_hour":{"utilization":29},"seven_day":{"utilization":26},"limits":[{"kind":"weekly_scoped","percent":43,"is_active":true,"scope":{"model":{"id":null,"display_name":"Fable"}}}]}"#,
        );
        let data = crate::models::AppUsageData::from_iter([(ProviderId::Claude, usage)]);
        let json = serde_json::to_string(&data).unwrap();
        let cached: crate::models::AppUsageData = serde_json::from_str(&json).unwrap();
        assert_eq!(data, cached);
        let context = DataContext::from_usage_with_runtime(
            Some(&cached),
            &Canvas::default(),
            ThemeRuntime::default().with_countdown(true),
        );
        for key in [
            "claude.limits.weekly_scoped_fable",
            "claude.model.fable",
            "claude.scoped",
        ] {
            assert_eq!(
                evaluate(&format!("{key}.percentage"), &context).unwrap(),
                43.0
            );
            assert_eq!(evaluate(&format!("{key}.display"), &context).unwrap(), 57.0);
            assert_eq!(
                format_template(&format!("{{{key}.label}} {{{key}:usage_line}}"), &context),
                "Fable 43%"
            );
            assert_eq!(
                format_template(&format!("{{{key}.display:usage_badge}}"), &context),
                "57%"
            );
        }
        assert_eq!(
            evaluate("claude.headline.percentage", &context).unwrap(),
            29.0
        );
        assert_eq!(
            evaluate("claude.weekly.percentage", &context).unwrap(),
            26.0
        );
    }

    fn usage_from_json(json: &str) -> UsageData {
        let response: UsageResponse =
            serde_json::from_str(json).expect("the fixture should deserialize");
        usage_from_response(response)
    }

    #[test]
    fn reported_windows_without_resets_remain_available_at_zero_usage() {
        for percentage in [0.0, 42.0] {
            let data = usage_from_json(&format!(
                r#"{{"five_hour":{{"utilization":{percentage},"resets_at":null}},"seven_day":null}}"#,
            ));
            assert!(data.session.available);
            assert_eq!(data.session.percentage, percentage);
            assert!(data.session.resets_at.is_none());
            assert!(!data.weekly.available);
        }
    }

    #[test]
    fn usage_resets_keep_exact_server_moments_and_absent_resets_stay_unknown() {
        let data = usage_from_json(
            r#"{"five_hour":{"utilization":42.5,"resets_at":"2026-10-26T12:34:56+02:00"},"seven_day":{"utilization":10.0}}"#,
        );
        assert!(data.session.available);
        assert_eq!(data.session.percentage, 42.5);
        // The offset timestamp is preserved exactly: the same instant written
        // in UTC parses to the identical moment, never an estimate.
        let moment = parse_iso8601(Some("2026-10-26T12:34:56+02:00"));
        assert!(moment.is_some());
        assert_eq!(data.session.resets_at, moment);
        assert_eq!(
            data.session.resets_at,
            parse_iso8601(Some("2026-10-26T10:34:56Z"))
        );
        // A window the server reports without any reset stays unknown.
        assert!(data.weekly.available);
        assert_eq!(data.weekly.percentage, 10.0);
        assert!(data.weekly.resets_at.is_none());
    }

    #[test]
    fn spend_becomes_a_credit_gauge_against_the_plan_cap() {
        // Shape taken from a live /api/oauth/usage response.
        let data = usage_from_json(
            r#"{
                "seven_day": {"utilization": 100.0, "resets_at": null},
                "spend": {
                    "used": {"amount_minor": 1359, "currency": "USD", "exponent": 2},
                    "limit": {"amount_minor": 5000, "currency": "USD", "exponent": 2},
                    "percent": 27,
                    "enabled": true
                }
            }"#,
        );

        let credits = data.credits.expect("enabled spend should expose a gauge");
        assert!((credits.percentage - 27.18).abs() < 0.01, "{credits:?}");
        assert!((credits.remaining - 36.41).abs() < 0.001, "{credits:?}");
        assert_eq!(credits.total, 50.0);
    }

    #[test]
    fn disabled_or_uncapped_spend_gets_no_gauge() {
        assert!(usage_from_json(
            r#"{"seven_day": {"utilization": 100.0},
                "spend": {"used": {"amount_minor": 0, "exponent": 2},
                          "limit": {"amount_minor": 5000, "exponent": 2}, "enabled": false}}"#
        )
        .credits
        .is_none());

        assert!(usage_from_json(
            r#"{"seven_day": {"utilization": 100.0},
                "spend": {"used": {"amount_minor": 10, "exponent": 2},
                          "limit": {"amount_minor": 0, "exponent": 2}, "enabled": true}}"#
        )
        .credits
        .is_none());

        assert!(usage_from_json(r#"{"seven_day": {"utilization": 1.0}}"#)
            .credits
            .is_none());
    }

    #[test]
    fn the_gauge_waits_for_a_spent_window_and_for_credits_to_be_in_play() {
        let spend = r#""spend": {"used": {"amount_minor": 1359, "exponent": 2},
                                 "limit": {"amount_minor": 5000, "exponent": 2}, "enabled": true}"#;

        // Room left in both windows, so the bars stay on the ordinary limits.
        let json = format!(r#"{{"five_hour": {{"utilization": 40.0}}, {spend}}}"#);
        assert!(usage_from_json(&json).credits.is_none());

        // A spent five-hour window is enough; it need not be the weekly one.
        let json = format!(r#"{{"five_hour": {{"utilization": 100.0}}, {spend}}}"#);
        assert!(usage_from_json(&json).credits.is_some());

        // Spent window, but nothing charged to credits yet.
        let json = r#"{"five_hour": {"utilization": 100.0},
                       "spend": {"used": {"amount_minor": 0, "exponent": 2},
                                 "limit": {"amount_minor": 5000, "exponent": 2},
                                 "enabled": true}}"#;
        assert!(usage_from_json(json).credits.is_none());
    }
}
