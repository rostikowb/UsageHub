use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use zeroize::Zeroize;

use super::{build_agent, parse_iso8601, PollError};
use crate::diagnose;
use crate::manager::credentials::{base64_decode, base64_encode};
use crate::models::{UsageData, UsageSection};

const GO_STATUS_URL: &str = "https://opencode.ai/console/api/go/status";
const DASHBOARD_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0 Safari/537.36";
const WORKSPACE_ID_ENV: &str = "OPENCODE_GO_WORKSPACE_ID";
const AUTH_COOKIE_ENV: &str = "OPENCODE_GO_AUTH_COOKIE";
const AUTH_COOKIE_ENCRYPTED_ENV: &str = "OPENCODE_GO_AUTH_COOKIE_ENCRYPTED";
const CONFIG_FILE_ENV: &str = "OPENCODE_GO_CONFIG_FILE";

#[derive(Deserialize)]
struct DashboardConfig {
    #[serde(alias = "workspaceId", alias = "workspaceID")]
    workspace_id: String,
    #[serde(default, alias = "authCookie", alias = "cookie")]
    auth_cookie: Option<String>,
    #[serde(default, alias = "authCookieEncrypted", alias = "encryptedCookie")]
    auth_cookie_encrypted: Option<String>,
}

struct DashboardCredentials {
    workspace_id: String,
    auth_cookie: String,
    source: String,
}

#[derive(Clone, Debug, PartialEq)]
struct UsageWindow {
    usage_percent: f64,
    resets_at: Option<SystemTime>,
}

#[derive(Debug, Default, PartialEq)]
struct DashboardUsage {
    rolling: Option<UsageWindow>,
    weekly: Option<UsageWindow>,
    monthly: Option<UsageWindow>,
}

// The console JSON API serializes microcent amounts as strings (JavaScript BigInts).
#[derive(Deserialize)]
struct GoStatus {
    access: Option<GoAccess>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoAccess {
    ends_at: String,
    meters: GoMeters,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoMeters {
    five_hour: GoTimedMeter,
    week: GoTimedMeter,
    month: GoMeter,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoTimedMeter {
    #[serde(flatten)]
    meter: GoMeter,
    // An unused rolling window has no reset time until the first request.
    resets_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoMeter {
    limit_micro_cents: String,
    used_micro_cents: String,
}

pub(super) fn poll_opencode() -> Result<UsageData, PollError> {
    let credentials = read_dashboard_credentials().ok_or_else(|| {
        diagnose::log("OpenCode usage poll failed: no dashboard credentials found");
        PollError::NoCredentials
    })?;
    poll_dashboard(&credentials)
}

pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    vec![credential_watch_signature()]
}

fn poll_dashboard(credentials: &DashboardCredentials) -> Result<UsageData, PollError> {
    let usage = fetch_dashboard_usage(credentials).inspect_err(|error| {
        diagnose::log(format!(
            "OpenCode dashboard poll failed via {}: {error:?}",
            credentials.source
        ));
    })?;

    if usage.rolling.is_none() && usage.weekly.is_none() && usage.monthly.is_none() {
        diagnose::log(format!(
            "OpenCode dashboard returned no usage windows from {}",
            credentials.source
        ));
        return Err(PollError::RequestFailed);
    }

    let session = usage
        .rolling
        .as_ref()
        .map(section_from_window)
        .unwrap_or_default();
    let (weekly, weekly_label) = select_long_window(&usage);

    Ok(UsageData {
        limits: Vec::new(),
        session,
        weekly,
        weekly_label,
        // The monthly window is kept available to themes alongside the
        // auto-selected `weekly` slot (which prefers the more constrained
        // of the two windows, as before).
        monthly: usage.monthly.as_ref().map(section_from_window),
        credits: None,
        stale: false,
        ..Default::default()
    })
}

fn select_long_window(usage: &DashboardUsage) -> (UsageSection, Option<String>) {
    match (&usage.weekly, &usage.monthly) {
        (Some(weekly), Some(monthly)) if monthly.usage_percent > weekly.usage_percent => {
            (section_from_window(monthly), Some("30d".to_string()))
        }
        (Some(weekly), _) => (section_from_window(weekly), Some("7d".to_string())),
        (None, Some(monthly)) => (section_from_window(monthly), Some("30d".to_string())),
        (None, None) => (UsageSection::default(), None),
    }
}

fn section_from_window(window: &UsageWindow) -> UsageSection {
    UsageSection {
        available: true,
        percentage: window.usage_percent.clamp(0.0, 100.0),
        resets_at: window.resets_at,
    }
}

fn read_dashboard_credentials() -> Option<DashboardCredentials> {
    if let Some(credentials) = read_env_credentials(
        non_empty_environment(WORKSPACE_ID_ENV),
        non_empty_environment(AUTH_COOKIE_ENV),
        non_empty_environment(AUTH_COOKIE_ENCRYPTED_ENV),
    ) {
        return Some(credentials);
    }

    dashboard_config_paths()
        .into_iter()
        .find_map(|path| read_dashboard_config(&path))
}

fn read_env_credentials(
    workspace_id: Option<String>,
    plain_cookie: Option<String>,
    encrypted_cookie: Option<String>,
) -> Option<DashboardCredentials> {
    let workspace_id = workspace_id?;
    if !valid_workspace_id(&workspace_id) {
        return None;
    }
    if let Some(auth_cookie) = plain_cookie.filter(|value| valid_cookie(value)) {
        return Some(DashboardCredentials {
            workspace_id,
            auth_cookie,
            source: "environment".to_string(),
        });
    }
    if let Some(encoded) = encrypted_cookie {
        match decrypt_auth_cookie(&encoded) {
            Ok(auth_cookie) => {
                return Some(DashboardCredentials {
                    workspace_id,
                    auth_cookie,
                    source: "environment (encrypted)".to_string(),
                });
            }
            Err(error) => {
                diagnose::log(format!(
                    "OpenCode encrypted environment cookie could not be decrypted: {error}"
                ));
            }
        }
    }
    None
}

fn read_dashboard_config(path: &Path) -> Option<DashboardCredentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: DashboardConfig = serde_json::from_str(&content).ok()?;
    let workspace_id = config.workspace_id.trim().to_string();
    if !valid_workspace_id(&workspace_id) {
        return None;
    }

    let encrypted = config
        .auth_cookie_encrypted
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let auth_cookie = if let Some(encoded) = encrypted {
        match decrypt_auth_cookie(encoded) {
            Ok(cookie) => cookie,
            Err(error) => {
                diagnose::log(format!(
                    "OpenCode encrypted auth cookie in {} could not be decrypted: {error}",
                    path.display()
                ));
                return None;
            }
        }
    } else {
        let cookie = config.auth_cookie?.trim().to_string();
        if !valid_cookie(&cookie) {
            return None;
        }
        cookie
    };

    Some(DashboardCredentials {
        workspace_id,
        auth_cookie,
        source: path.display().to_string(),
    })
}

/// Protect an OpenCode dashboard cookie with Windows DPAPI (CurrentUser scope)
/// and encode it as standard base64 for storage in `authCookieEncrypted`.
#[allow(dead_code)]
pub(crate) fn encrypt_auth_cookie(cookie: &str) -> Result<String, String> {
    let trimmed = cookie.trim();
    if !valid_cookie(trimmed) {
        return Err("Invalid OpenCode auth cookie".to_string());
    }
    let protected = dpapi_protect(trimmed.as_bytes())?;
    Ok(base64_encode(&protected))
}

/// Decode and DPAPI-unprotect an `authCookieEncrypted` value.
pub(crate) fn decrypt_auth_cookie(encoded: &str) -> Result<String, String> {
    let blob = base64_decode(encoded.trim())?;
    let mut plaintext = dpapi_unprotect(&blob)?;
    let result = String::from_utf8(plaintext.clone())
        .map_err(|_| "Decrypted OpenCode auth cookie is not valid UTF-8".to_string())
        .and_then(|value| {
            let trimmed = value.trim().to_string();
            if valid_cookie(&trimmed) {
                Ok(trimmed)
            } else {
                Err("Decrypted OpenCode auth cookie is invalid".to_string())
            }
        });
    plaintext.zeroize();
    result
}

// ---------------------------------------------------------------------------
// DPAPI protection (CurrentUser scope, UI forbidden) — mirrors manager::backup.
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[repr(C)]
struct CryptBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(windows)]
#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        data_in: *const CryptBlob,
        data_descr: *const u16,
        optional_entropy: *const CryptBlob,
        reserved: *mut std::ffi::c_void,
        prompt_struct: *mut std::ffi::c_void,
        flags: u32,
        data_out: *mut CryptBlob,
    ) -> i32;
    fn CryptUnprotectData(
        data_in: *const CryptBlob,
        data_descr: *mut *mut u16,
        optional_entropy: *const CryptBlob,
        reserved: *mut std::ffi::c_void,
        prompt_struct: *mut std::ffi::c_void,
        flags: u32,
        data_out: *mut CryptBlob,
    ) -> i32;
}

#[cfg(windows)]
extern "system" {
    fn LocalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

#[cfg(windows)]
fn dpapi_protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let input = CryptBlob {
        cb_data: u32::try_from(plaintext.len())
            .map_err(|_| "OpenCode cookie too large for DPAPI".to_string())?,
        pb_data: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CryptBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return Err("Unable to protect OpenCode cookie with DPAPI".to_string());
    }
    Ok(unsafe {
        let protected =
            std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut std::ffi::c_void);
        protected
    })
}

#[cfg(windows)]
fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, String> {
    let input = CryptBlob {
        cb_data: u32::try_from(blob.len())
            .map_err(|_| "OpenCode cookie blob too large for DPAPI".to_string())?,
        pb_data: blob.as_ptr() as *mut u8,
    };
    let mut output = CryptBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return Err("Unable to unprotect OpenCode cookie with DPAPI".to_string());
    }
    Ok(unsafe {
        let plaintext =
            std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut std::ffi::c_void);
        plaintext
    })
}

#[cfg(not(windows))]
fn dpapi_protect(_plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Err("Encrypted OpenCode cookies require Windows DPAPI".to_string())
}

#[cfg(not(windows))]
fn dpapi_unprotect(_blob: &[u8]) -> Result<Vec<u8>, String> {
    Err("Encrypted OpenCode cookies require Windows DPAPI".to_string())
}

fn fetch_dashboard_usage(credentials: &DashboardCredentials) -> Result<DashboardUsage, PollError> {
    fetch_go_status(credentials, GO_STATUS_URL)
}

fn fetch_go_status(
    credentials: &DashboardCredentials,
    url: &str,
) -> Result<DashboardUsage, PollError> {
    fetch_go_status_with(&build_agent()?, credentials, url)
}

fn fetch_go_status_with(
    agent: &ureq::Agent,
    credentials: &DashboardCredentials,
    url: &str,
) -> Result<DashboardUsage, PollError> {
    let cookie = if credentials.auth_cookie.split(';').any(|part| {
        let part = part.trim_start();
        part.starts_with("auth=") || part.starts_with("__Host-console_session=")
    }) {
        credentials.auth_cookie.clone()
    } else {
        // Bare legacy auth values may contain '=' padding; that alone does
        // not identify a complete Cookie header.
        format!("auth={}", credentials.auth_cookie)
    };

    let mut response = match agent
        .get(url)
        .header("Accept", "application/json")
        .header("x-org-id", &credentials.workspace_id)
        .header("Cookie", &cookie)
        .header("User-Agent", DASHBOARD_USER_AGENT)
        .call()
        .and_then(super::check_http_status)
    {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => return Err(PollError::AuthRequired),
        Err(error) => {
            diagnose::log_error("OpenCode Go status request failed", error);
            return Err(PollError::RequestFailed);
        }
    };

    let status = response
        .body_mut()
        .read_json::<Option<GoStatus>>()
        .map_err(|error| {
            diagnose::log_error("OpenCode Go status response is not valid JSON", error);
            PollError::RequestFailed
        })?;
    usage_from_status(status)
}

fn usage_from_status(status: Option<GoStatus>) -> Result<DashboardUsage, PollError> {
    let access = status.and_then(|status| status.access).ok_or_else(|| {
        diagnose::log("OpenCode Go status returned no active subscription access");
        PollError::RequestFailed
    })?;
    Ok(DashboardUsage {
        rolling: Some(window_from_meter(
            &access.meters.five_hour.meter,
            access.meters.five_hour.resets_at.as_deref(),
        )?),
        weekly: Some(window_from_meter(
            &access.meters.week.meter,
            access.meters.week.resets_at.as_deref(),
        )?),
        monthly: Some(window_from_meter(
            &access.meters.month,
            Some(&access.ends_at),
        )?),
    })
}

fn window_from_meter(meter: &GoMeter, resets_at: Option<&str>) -> Result<UsageWindow, PollError> {
    let limit = meter.limit_micro_cents.parse::<u128>().map_err(|_| {
        diagnose::log("OpenCode Go status contains an invalid usage limit");
        PollError::RequestFailed
    })?;
    let used = meter.used_micro_cents.parse::<u128>().map_err(|_| {
        diagnose::log("OpenCode Go status contains an invalid usage amount");
        PollError::RequestFailed
    })?;
    let resets_at = resets_at
        .map(|value| {
            parse_iso8601(Some(value)).ok_or_else(|| {
                diagnose::log("OpenCode Go status contains an invalid reset time");
                PollError::RequestFailed
            })
        })
        .transpose()?;
    Ok(UsageWindow {
        usage_percent: if limit == 0 {
            0.0
        } else {
            used as f64 / limit as f64 * 100.0
        },
        resets_at,
    })
}

fn dashboard_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = non_empty_environment(CONFIG_FILE_ENV).map(PathBuf::from) {
        paths.push(path);
    }
    if let Some(app_data) = non_empty_environment("APPDATA").map(PathBuf::from) {
        paths.push(app_data.join("opencode-go").join("config.json"));
    }
    if let Some(config_home) = non_empty_environment("XDG_CONFIG_HOME").map(PathBuf::from) {
        paths.push(config_home.join("opencode-bar").join("opencode-go.json"));
        paths.push(config_home.join("opencode-quota").join("opencode-go.json"));
    }
    if let Some(home) = crate::known_folders::home_dir() {
        paths.push(
            home.join(".config")
                .join("opencode-bar")
                .join("opencode-go.json"),
        );
        paths.push(
            home.join(".config")
                .join("opencode-quota")
                .join("opencode-go.json"),
        );
    }
    paths
}

fn non_empty_environment(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn valid_workspace_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_cookie(value: &str) -> bool {
    !value.is_empty() && !value.bytes().any(|byte| matches!(byte, b'\r' | b'\n'))
}

fn credential_watch_signature() -> String {
    let mut parts = Vec::new();
    match read_dashboard_credentials() {
        Some(credentials) => {
            let mut hasher = DefaultHasher::new();
            credentials.workspace_id.hash(&mut hasher);
            credentials.auth_cookie.hash(&mut hasher);
            parts.push(format!(
                "dashboard|present|{}|{}|{:x}|{}",
                credentials.workspace_id.len(),
                credentials.auth_cookie.len(),
                hasher.finish(),
                credentials.source
            ));
        }
        None => parts.push("dashboard|missing".to_string()),
    }
    for path in dashboard_config_paths() {
        parts.push(path_signature("config", &path));
    }
    parts.join(";;")
}

fn path_signature(kind: &str, path: &Path) -> String {
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_secs())
                .unwrap_or(0);
            format!(
                "{kind}:{}|present|{}|{modified}",
                path.display(),
                metadata.len()
            )
        }
        Err(_) => format!("{kind}:{}|missing", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wire shape verified against the console's Go status schema and usage UI.
    const GO_STATUS_JSON: &str = r#"{
        "subscriberUserID": "usr_example",
        "useBalance": false,
        "access": {
            "startsAt": "2026-09-01T00:00:00.000Z",
            "endsAt": "2026-10-01T00:00:00.000Z",
            "meters": {
                "fiveHour": {
                    "limitMicroCents": "2000000000",
                    "usedMicroCents": "250000000",
                    "resetsAt": "2026-09-17T05:00:00.000Z"
                },
                "week": {
                    "limitMicroCents": "10000000000",
                    "usedMicroCents": "4500000000",
                    "resetsAt": "2026-09-21T00:00:00.000Z"
                },
                "month": {
                    "limitMicroCents": "20000000000",
                    "usedMicroCents": "12000000000"
                }
            }
        }
    }"#;

    #[test]
    fn console_status_maps_meter_percentages_and_absolute_resets() {
        let usage = usage_from_status(serde_json::from_str(GO_STATUS_JSON).unwrap()).unwrap();
        let rolling = usage.rolling.as_ref().unwrap();
        assert_eq!(rolling.usage_percent, 12.5);
        assert_eq!(
            rolling.resets_at,
            parse_iso8601(Some("2026-09-17T05:00:00Z"))
        );
        let weekly = usage.weekly.as_ref().unwrap();
        assert_eq!(weekly.usage_percent, 45.0);
        assert_eq!(
            weekly.resets_at,
            parse_iso8601(Some("2026-09-21T00:00:00Z"))
        );
        let (long, label) = select_long_window(&usage);
        assert_eq!(long.percentage, 60.0);
        assert_eq!(label.as_deref(), Some("30d"));
        assert_eq!(long.resets_at, parse_iso8601(Some("2026-10-01T00:00:00Z")));
    }

    #[test]
    fn console_status_preserves_idle_rolling_window_without_reset() {
        let json = GO_STATUS_JSON
            .replace("\"250000000\"", "\"0\"")
            .replace("\"2026-09-17T05:00:00.000Z\"", "null");
        let usage = usage_from_status(serde_json::from_str(&json).unwrap()).unwrap();
        let section = section_from_window(usage.rolling.as_ref().unwrap());
        assert!(section.available);
        assert_eq!(section.percentage, 0.0);
        assert_eq!(section.resets_at, None);
    }

    #[test]
    fn console_status_rejects_missing_access_and_invalid_meter_values() {
        for json in ["null", r#"{"access":null}"#] {
            assert_eq!(
                usage_from_status(serde_json::from_str(json).unwrap()),
                Err(PollError::RequestFailed)
            );
        }
        for json in [
            GO_STATUS_JSON.replace("250000000", "-1"),
            GO_STATUS_JSON.replace("250000000", "NaN"),
            GO_STATUS_JSON.replace("250000000", "1.5"),
            GO_STATUS_JSON.replace("2026-09-17T05:00:00.000Z", "invalid-date"),
        ] {
            assert_eq!(
                usage_from_status(serde_json::from_str(&json).unwrap()),
                Err(PollError::RequestFailed)
            );
        }
    }

    #[test]
    fn console_meter_handles_zero_limits_large_amounts_and_overages() {
        let meter = |used: &str, limit: &str| GoMeter {
            used_micro_cents: used.into(),
            limit_micro_cents: limit.into(),
        };
        assert_eq!(
            window_from_meter(&meter("0", "0"), None)
                .unwrap()
                .usage_percent,
            0.0
        );
        assert_eq!(
            window_from_meter(&meter("10000000000000000000", "20000000000000000000"), None)
                .unwrap()
                .usage_percent,
            50.0
        );
        let window = window_from_meter(&meter("120", "100"), None).unwrap();
        assert_eq!(section_from_window(&window).percentage, 100.0);
    }

    /// Read a complete HTTP request (headers plus any Content-Length body) so
    /// the fixture never closes the socket with unread request bytes, which
    /// surfaces as WSAECONNRESET on Windows.
    fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
        use std::io::Read;
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            assert!(request.len() < 64 * 1024, "request headers too large");
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "client closed before sending headers");
            request.extend_from_slice(&buffer[..count]);
        }
        let header_end = request
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        assert!(content_length < 1024 * 1024, "request body too large");
        while request.len() < header_end + content_length {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "client closed before sending body");
            request.extend_from_slice(&buffer[..count]);
        }
        request
    }

    fn mock_status_request(
        status: u16,
        body: &str,
        cookie: &str,
    ) -> (Result<DashboardUsage, PollError>, String) {
        use std::io::Write;
        use std::net::TcpListener;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/console/api/go/status",
            listener.local_addr().unwrap()
        );
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let request = read_http_request(&mut stream);
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
            // Hold the server socket until the client has consumed or rejected
            // the complete response. This avoids a Windows FIN/RST race under
            // the parallel suite without retrying a failed request.
            finished_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("client did not finish the fixture request");
            let _ = stream.shutdown(std::net::Shutdown::Both);
            String::from_utf8(request).unwrap()
        });
        // Bypass the process proxy for loopback: ureq resolves a proxy from
        // the environment by default, which would route 127.0.0.1 through the
        // proxy and fail before the fixture responds. Production keeps the
        // shared agent (and its proxy behavior) unchanged.
        let agent =
            super::super::build_loopback_test_agent().expect("loopback test agent should build");
        let result = fetch_go_status_with(
            &agent,
            &DashboardCredentials {
                workspace_id: "wrk_example".into(),
                auth_cookie: cookie.into(),
                source: "test".into(),
            },
            &url,
        );
        let _ = finished_tx.send(());
        (result, server.join().unwrap())
    }

    #[test]
    fn console_request_sends_workspace_context_and_normalizes_auth_cookie() {
        for (cookie, expected) in [
            ("test-token", "auth=test-token"),
            ("test-token==", "auth=test-token=="),
            ("auth=test-token; theme=dark", "auth=test-token; theme=dark"),
            (
                "__Host-console_session=SessionToken==",
                "__Host-console_session=SessionToken==",
            ),
            (
                "__Host-console_session=SessionToken; __stripe_mid=StripeValue",
                "__Host-console_session=SessionToken; __stripe_mid=StripeValue",
            ),
            (
                "theme=dark; __Host-console_session=SessionToken",
                "theme=dark; __Host-console_session=SessionToken",
            ),
            (
                "auth=; __Host-console_session=SessionToken",
                "auth=; __Host-console_session=SessionToken",
            ),
        ] {
            let (result, request) = mock_status_request(200, GO_STATUS_JSON, cookie);
            assert_eq!(result.unwrap().rolling.unwrap().usage_percent, 12.5);
            let sent_cookie = request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("cookie")
                        .then_some(value.trim_start())
                })
                .unwrap();
            assert_eq!(sent_cookie, expected);
            let request = request.to_ascii_lowercase();
            assert!(request.starts_with("get /console/api/go/status http/1.1\r\n"));
            assert!(request.contains("\r\nx-org-id: wrk_example\r\n"));
            assert!(request.contains("\r\naccept: application/json\r\n"));
        }
    }

    #[test]
    fn console_request_distinguishes_auth_errors_from_bad_responses() {
        for status in [401, 403] {
            assert_eq!(
                mock_status_request(status, "{}", "test-token").0,
                Err(PollError::AuthRequired)
            );
        }
        for (status, body) in [
            (400, "{}"),
            (500, "{}"),
            (200, "<html><div id=app></div></html>"),
            (200, "{invalid"),
            (200, "{}"),
            (200, "null"),
            (200, r#"{"access":{"meters":{}}}"#),
        ] {
            assert_eq!(
                mock_status_request(status, body, "test-token").0,
                Err(PollError::RequestFailed)
            );
        }
    }

    #[test]
    fn most_constrained_long_window_is_selected() {
        let usage = DashboardUsage {
            weekly: Some(UsageWindow {
                usage_percent: 40.0,
                resets_at: Some(UNIX_EPOCH),
            }),
            monthly: Some(UsageWindow {
                usage_percent: 70.0,
                resets_at: Some(UNIX_EPOCH),
            }),
            ..Default::default()
        };
        let (section, label) = select_long_window(&usage);
        assert_eq!(section.percentage, 70.0);
        assert_eq!(label.as_deref(), Some("30d"));
    }

    #[test]
    fn idle_opencode_windows_are_available_and_missing_windows_are_not() {
        let window = UsageWindow {
            usage_percent: 0.0,
            resets_at: None,
        };
        let section = section_from_window(&window);
        assert!(section.available);
        assert_eq!(section.percentage, 0.0);
        let usage = DashboardUsage {
            monthly: Some(window),
            ..Default::default()
        };
        let (section, label) = select_long_window(&usage);
        assert!(section.available);
        assert_eq!(label.as_deref(), Some("30d"));
        assert!(!select_long_window(&DashboardUsage::default()).0.available);
    }

    #[test]
    fn dashboard_identifiers_and_cookie_headers_reject_request_injection() {
        assert!(valid_workspace_id("wrk_01-test"));
        assert!(!valid_workspace_id("../other"));
        assert!(valid_cookie("auth=abc; theme=dark"));
        assert!(!valid_cookie("auth=abc\r\nX-Test: injected"));
    }

    #[test]
    fn dpapi_cookie_roundtrip_and_tamper_rejection() {
        let original = "__Host-console_session=SessionToken123==; __stripe_mid=StripeVal";
        let encrypted = encrypt_auth_cookie(original).expect("DPAPI encrypt should succeed");
        assert_ne!(encrypted, original);
        assert!(!encrypted.contains("SessionToken123"));

        let decrypted = decrypt_auth_cookie(&encrypted).expect("DPAPI decrypt should succeed");
        assert_eq!(decrypted, original);

        // Tampering with the base64 ciphertext must fail verification/decryption.
        let mut raw_blob = base64_decode(&encrypted).unwrap();
        raw_blob[0] ^= 0xff;
        let tampered = base64_encode(&raw_blob);
        assert!(decrypt_auth_cookie(&tampered).is_err());

        // Header-injection cookies must be rejected before encryption.
        assert!(encrypt_auth_cookie("auth=abc\r\nX-Evil: 1").is_err());
    }

    #[test]
    fn config_file_supports_encrypted_and_legacy_plaintext_cookies() {
        let temp = tempfile::tempdir().unwrap();

        // 1. DPAPI-encrypted config.
        let encrypted_cookie = encrypt_auth_cookie("__Host-console_session=dpapi_tok").unwrap();
        let enc_path = temp.path().join("encrypted.json");
        std::fs::write(
            &enc_path,
            serde_json::json!({
                "workspaceId": "wrk_encrypted",
                "authCookieEncrypted": encrypted_cookie,
            })
            .to_string(),
        )
        .unwrap();
        let creds = read_dashboard_config(&enc_path).unwrap();
        assert_eq!(creds.workspace_id, "wrk_encrypted");
        assert_eq!(creds.auth_cookie, "__Host-console_session=dpapi_tok");

        // 2. Legacy plaintext config remains fully supported.
        let legacy_path = temp.path().join("legacy.json");
        std::fs::write(
            &legacy_path,
            r#"{"workspaceId":"wrk_legacy","authCookie":"legacy_secret_cookie"}"#,
        )
        .unwrap();
        let legacy_creds = read_dashboard_config(&legacy_path).unwrap();
        assert_eq!(legacy_creds.workspace_id, "wrk_legacy");
        assert_eq!(legacy_creds.auth_cookie, "legacy_secret_cookie");

        // 3. Corrupted encrypted cookie returns None rather than panicking.
        let bad_path = temp.path().join("corrupt.json");
        std::fs::write(
            &bad_path,
            r#"{"workspaceId":"wrk_bad","authCookieEncrypted":"not_valid_base64!!!"}"#,
        )
        .unwrap();
        assert!(read_dashboard_config(&bad_path).is_none());

        // 4. Encrypted cookie takes precedence over plaintext when both exist.
        let both_path = temp.path().join("both.json");
        let newer_enc = encrypt_auth_cookie("__Host-console_session=preferred_dpapi").unwrap();
        std::fs::write(
            &both_path,
            serde_json::json!({
                "workspaceId": "wrk_both",
                "authCookie": "old_plaintext",
                "authCookieEncrypted": newer_enc,
            })
            .to_string(),
        )
        .unwrap();
        let both_creds = read_dashboard_config(&both_path).unwrap();
        assert_eq!(
            both_creds.auth_cookie,
            "__Host-console_session=preferred_dpapi"
        );
    }

    #[test]
    fn env_credentials_preserve_plaintext_and_support_encrypted() {
        let plain = read_env_credentials(
            Some("wrk_env".into()),
            Some("plain_env_cookie".into()),
            None,
        )
        .unwrap();
        assert_eq!(plain.workspace_id, "wrk_env");
        assert_eq!(plain.auth_cookie, "plain_env_cookie");
        assert_eq!(plain.source, "environment");

        let enc_val = encrypt_auth_cookie("dpapi_env_cookie").unwrap();
        let enc = read_env_credentials(Some("wrk_env".into()), None, Some(enc_val)).unwrap();
        assert_eq!(enc.workspace_id, "wrk_env");
        assert_eq!(enc.auth_cookie, "dpapi_env_cookie");
        assert_eq!(enc.source, "environment (encrypted)");
    }
}
