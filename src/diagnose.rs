use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Maximum active diagnostic log file size before rotating to `.prev` (5 MiB).
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

const REDACTED_MARKER: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LogFileStatus {
    pub current_bytes: u64,
    pub max_bytes: u64,
    pub prev_bytes: Option<u64>,
}

struct LogTarget {
    file: File,
    path: PathBuf,
}

struct DiagnoseState {
    file: Mutex<Option<LogTarget>>,
    enabled: AtomicBool,
    max_bytes: u64,
}

impl DiagnoseState {
    const fn new() -> Self {
        Self::with_max_bytes(MAX_LOG_BYTES)
    }

    const fn with_max_bytes(max_bytes: u64) -> Self {
        Self {
            file: Mutex::new(None),
            enabled: AtomicBool::new(false),
            max_bytes,
        }
    }

    fn enable(&self, path: &Path, append: bool) -> Result<bool, String> {
        let mut guard = self.file.lock().map_err(|error| error.to_string())?;
        if guard.is_some() {
            return Ok(false);
        }
        if append
            && std::fs::metadata(path)
                .map(|metadata| metadata.len())
                .unwrap_or(0)
                >= self.max_bytes
        {
            let _ = rotate_log_file(path);
        }
        let file = open_log(path, append).map_err(|error| {
            format!(
                "Unable to open diagnostic log file {}: {error}",
                path.display()
            )
        })?;
        *guard = Some(LogTarget {
            file,
            path: path.to_path_buf(),
        });
        self.enabled.store(true, Ordering::Release);
        Ok(true)
    }

    fn disable(&self) {
        let file = self.file.lock();
        self.enabled.store(false, Ordering::Release);
        if let Ok(mut file) = file {
            *file = None;
        }
    }

    fn write(&self, line: &[u8]) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut guard) = self.file.lock() {
            Self::write_with_rotation(&mut guard, self.max_bytes, line);
        }
    }

    fn try_write(&self, line: &[u8]) {
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        // Recovery must still proceed if the frozen thread owns the log mutex.
        if let Ok(mut guard) = self.file.try_lock() {
            Self::write_with_rotation(&mut guard, self.max_bytes, line);
        }
    }

    fn write_with_rotation(guard: &mut Option<LogTarget>, max_bytes: u64, line: &[u8]) {
        let current_len = match guard.as_ref() {
            Some(target) => target.file.metadata().map(|meta| meta.len()).unwrap_or(0),
            None => return,
        };
        if current_len > 0 && current_len.saturating_add(line.len() as u64) > max_bytes {
            let path = match guard.take() {
                Some(target) => target.path,
                None => return,
            };
            let reopened = if rotate_log_file(&path).is_ok() {
                open_log(&path, true)
            } else {
                // Keep the file bounded even if another reader prevented rename.
                open_log(&path, false)
            };
            if let Ok(file) = reopened {
                *guard = Some(LogTarget { file, path });
            }
        }
        if let Some(target) = guard.as_mut() {
            let _ = target.file.write_all(line);
            let _ = target.file.flush();
        }
    }
}

static DIAGNOSE_STATE: DiagnoseState = DiagnoseState::new();

pub fn log_path() -> PathBuf {
    std::env::temp_dir().join("usagehub.log")
}

pub fn rotated_log_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".prev");
    PathBuf::from(os)
}

pub fn prev_log_path() -> PathBuf {
    rotated_log_path(&log_path())
}

pub fn log_status() -> LogFileStatus {
    let path = log_path();
    let prev = rotated_log_path(&path);
    LogFileStatus {
        current_bytes: std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
        max_bytes: MAX_LOG_BYTES,
        prev_bytes: std::fs::metadata(&prev).ok().map(|meta| meta.len()),
    }
}

fn rotate_log_file(path: &Path) -> std::io::Result<()> {
    let prev = rotated_log_path(path);
    if let Err(first_err) = std::fs::rename(path, &prev) {
        let _ = std::fs::remove_file(&prev);
        if let Err(retry_err) = std::fs::rename(path, &prev) {
            return Err(if retry_err.kind() == std::io::ErrorKind::NotFound {
                first_err
            } else {
                retry_err
            });
        }
    }
    Ok(())
}

/// Record panics even when diagnostic recording is disabled.
pub fn install_panic_hook() {
    let path = log_path();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Use a separate append handle: the panicking thread may hold the logger's mutex.
        // Never unwrap I/O errors here, since a second panic would abort the hook itself.
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let thread = std::thread::current();
        let payload = info.payload_as_str().unwrap_or("non-string panic payload");
        let location = info
            .location()
            .map(|location| location.to_string())
            .unwrap_or_else(|| "unknown location".to_owned());
        let line = redact(&format!(
            "[{timestamp}] [pid {}] PANIC [thread {:?} ({})] at {location}: {payload}\n",
            std::process::id(),
            thread.id(),
            thread.name().unwrap_or("unnamed")
        ));
        if std::fs::metadata(&path).is_ok_and(|meta| {
            meta.len() > 0 && meta.len().saturating_add(line.len() as u64) > MAX_LOG_BYTES
        }) {
            let _ = rotate_log_file(&path);
        }
        if let Ok(mut file) = open_log(&path, true) {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
        previous_hook(info);
    }));
}

/// Only load the tail: long diagnostic sessions must not freeze the dashboard.
pub fn read_tail(path: &std::path::Path, max_bytes: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let start = if start > 0 {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    } else {
        0
    };
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

pub fn init() -> Result<PathBuf, String> {
    init_file(false)
}

pub fn init_append() -> Result<PathBuf, String> {
    init_file(true)
}

fn init_file(append: bool) -> Result<PathBuf, String> {
    let path = log_path();
    if !DIAGNOSE_STATE.enable(&path, append)? {
        return Ok(path);
    }

    log(if append {
        "diagnostic logging enabled (append)"
    } else {
        "diagnostic logging enabled"
    });
    log(format!(
        "app version={} executable={}",
        env!("CARGO_PKG_VERSION"),
        std::env::current_exe().unwrap_or_default().display()
    ));
    Ok(path)
}

fn open_log(path: &std::path::Path, append: bool) -> std::io::Result<File> {
    // Both processes must always append, even after a --diagnose reset.
    // A plain write handle would overwrite lines appended by the dashboard.
    if !append {
        File::create(path)?;
    }
    OpenOptions::new().create(true).append(true).open(path)
}

pub fn is_enabled() -> bool {
    DIAGNOSE_STATE.enabled.load(Ordering::Acquire)
}

pub fn disable() {
    log("diagnostic recording disabled");
    DIAGNOSE_STATE.disable();
}

pub fn log(message: impl AsRef<str>) {
    if !is_enabled() {
        return;
    }

    DIAGNOSE_STATE.write(format_line(message.as_ref()).as_bytes());
}

/// Best-effort recovery logging: skip a busy logger instead of waiting on it.
pub fn log_recovery(message: impl AsRef<str>) {
    if is_enabled() {
        DIAGNOSE_STATE.try_write(format_line(message.as_ref()).as_bytes());
    }
}

fn format_line(message: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let redacted = redact(message);

    format!("[{timestamp}] [pid {}] {}\n", std::process::id(), redacted)
}

pub fn log_error(context: &str, error: impl std::fmt::Display) {
    log_lazy(|| format!("{context}: {error}"));
}

pub fn log_lazy(message: impl FnOnce() -> String) {
    if is_enabled() {
        log(message());
    }
}

/// Mask sensitive values (Bearer tokens, session cookies, Authorization/Cookie
/// headers, email addresses, and user profile paths) in diagnostic text.
pub fn redact(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let custom_profile = custom_user_profile_prefix();
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut last_emit = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if !input.is_char_boundary(i) {
            i += 1;
            continue;
        }

        if let Some((end, replacement)) = try_redact_header(input, i)
            .or_else(|| try_redact_bearer(input, i))
            .or_else(|| try_redact_cookie_kv(input, i))
            .or_else(|| try_redact_user_path(input, i, custom_profile.as_deref()))
            .or_else(|| try_redact_email(input, i))
        {
            out.push_str(&input[last_emit..i]);
            out.push_str(&replacement);
            last_emit = end;
            i = end;
            continue;
        }

        i += input[i..].chars().next().map_or(1, char::len_utf8);
    }

    if last_emit == 0 {
        return input.to_owned();
    }
    out.push_str(&input[last_emit..]);
    out
}

fn custom_user_profile_prefix() -> Option<String> {
    let profile = std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())?;
    let trimmed = profile.trim().trim_end_matches(['\\', '/']);
    if trimmed.len() < 4 {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("\\users\\") || lower.contains("/users/") || lower.starts_with("/home/") {
        return None;
    }
    Some(trimmed.to_owned())
}

fn has_prefix_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn is_boundary_before(bytes: &[u8], i: usize) -> bool {
    i == 0
        || matches!(
            bytes[i - 1],
            b' ' | b'\t'
                | b'\r'
                | b'\n'
                | b'"'
                | b'\''
                | b'('
                | b'['
                | b'{'
                | b','
                | b';'
                | b'='
                | b':'
        )
}

fn try_redact_header(input: &str, i: usize) -> Option<(usize, String)> {
    let bytes = input.as_bytes();
    let rest = &input[i..];
    let headers = ["authorization", "set-cookie", "cookie"];

    // JSON-style quoted header key: `"Authorization": "value"`
    if bytes[i] == b'"' && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
        for name in headers {
            if rest[1..].len() > name.len()
                && has_prefix_ignore_ascii_case(&rest[1..], name)
                && rest.as_bytes().get(1 + name.len()) == Some(&b'"')
            {
                let after_key = 1 + name.len() + 1;
                let mut pos = i + after_key;
                while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
                    pos += 1;
                }
                if bytes.get(pos) != Some(&b':') {
                    continue;
                }
                pos += 1;
                while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
                    pos += 1;
                }
                if bytes.get(pos) != Some(&b'"') {
                    continue;
                }
                let val_start = pos + 1;
                let mut val_end = val_start;
                while val_end < bytes.len() {
                    if bytes[val_end] == b'\\' && val_end + 1 < bytes.len() {
                        val_end += 2;
                        continue;
                    }
                    if matches!(bytes[val_end], b'"' | b'\r' | b'\n') {
                        break;
                    }
                    val_end += 1;
                }
                if bytes.get(val_end) == Some(&b'"') && val_end > val_start {
                    let prefix = &input[i..val_start];
                    return Some((val_end + 1, format!("{prefix}{REDACTED_MARKER}\"")));
                }
            }
        }
    }

    if !is_boundary_before(bytes, i) {
        return None;
    }
    let quoted_wrap = (i > 0 && matches!(bytes[i - 1], b'"' | b'\'')).then(|| bytes[i - 1]);

    for name in headers {
        if has_prefix_ignore_ascii_case(rest, name) && bytes.get(i + name.len()) == Some(&b':') {
            let colon_pos = i + name.len();
            let mut val_start = colon_pos + 1;
            while val_start < bytes.len() && matches!(bytes[val_start], b' ' | b'\t') {
                val_start += 1;
            }
            let mut val_end = val_start;
            while val_end < bytes.len() {
                let b = bytes[val_end];
                if matches!(b, b'\r' | b'\n') || quoted_wrap == Some(b) {
                    break;
                }
                val_end += 1;
            }
            let trimmed = input[val_start..val_end].trim_end();
            if trimmed.is_empty() || trimmed == REDACTED_MARKER {
                return None;
            }
            let header_label = &input[i..colon_pos];
            return Some((val_end, format!("{header_label}: {REDACTED_MARKER}")));
        }
    }
    None
}

fn is_bearer_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b':' | b'%'
        )
}

fn try_redact_bearer(input: &str, i: usize) -> Option<(usize, String)> {
    let bytes = input.as_bytes();
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        return None;
    }
    let rest = &input[i..];
    if !has_prefix_ignore_ascii_case(rest, "bearer") {
        return None;
    }
    let mut pos = i + 6;
    let ws_start = pos;
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    if pos == ws_start {
        return None;
    }
    let tok_start = pos;
    if input[tok_start..].starts_with(REDACTED_MARKER) {
        return None;
    }
    while pos < bytes.len() && is_bearer_token_byte(bytes[pos]) {
        pos += 1;
    }
    if pos == tok_start {
        return None;
    }
    Some((pos, format!("{} {REDACTED_MARKER}", &input[i..i + 6])))
}

fn is_cookie_value_byte(b: u8) -> bool {
    !matches!(
        b,
        b';' | b','
            | b' '
            | b'\t'
            | b'\r'
            | b'\n'
            | b'"'
            | b'\''
            | b')'
            | b']'
            | b'}'
            | b'<'
            | b'>'
    )
}

fn try_redact_cookie_kv(input: &str, i: usize) -> Option<(usize, String)> {
    let bytes = input.as_bytes();
    if i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || matches!(bytes[i - 1], b'_' | b'-' | b'.'))
    {
        return None;
    }
    let rest = &input[i..];

    // JSON-quoted sensitive cookie/token fields: `"authCookie": "..."`
    if bytes[i] == b'"' {
        let json_keys = [
            "__host-console_session",
            "workoscursorsessiontoken",
            "authcookieencrypted",
            "authcookie",
            "auth_cookie",
            "console_session",
            "session_token",
            "session_id",
            "sessionkey",
            "session_key",
        ];
        for key in json_keys {
            if rest.len() > key.len() + 2
                && has_prefix_ignore_ascii_case(&rest[1..], key)
                && bytes.get(i + 1 + key.len()) == Some(&b'"')
            {
                let mut pos = i + 1 + key.len() + 1;
                while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
                    pos += 1;
                }
                if bytes.get(pos) != Some(&b':') {
                    continue;
                }
                pos += 1;
                while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
                    pos += 1;
                }
                if bytes.get(pos) != Some(&b'"') {
                    continue;
                }
                let val_start = pos + 1;
                let mut val_end = val_start;
                while val_end < bytes.len() {
                    if bytes[val_end] == b'\\' && val_end + 1 < bytes.len() {
                        val_end += 2;
                        continue;
                    }
                    if matches!(bytes[val_end], b'"' | b'\r' | b'\n') {
                        break;
                    }
                    val_end += 1;
                }
                if bytes.get(val_end) == Some(&b'"') && val_end > val_start {
                    let prefix = &input[i..val_start];
                    return Some((val_end + 1, format!("{prefix}{REDACTED_MARKER}\"")));
                }
            }
        }
        return None;
    }

    // Cookie `name=value` pairs.
    let key_end = if has_prefix_ignore_ascii_case(rest, "__host-")
        || has_prefix_ignore_ascii_case(rest, "__secure-")
    {
        let mut p = i + if has_prefix_ignore_ascii_case(rest, "__host-") {
            7
        } else {
            9
        };
        while p < bytes.len()
            && (bytes[p].is_ascii_alphanumeric() || matches!(bytes[p], b'_' | b'-' | b'.'))
        {
            p += 1;
        }
        (bytes.get(p) == Some(&b'=')).then_some(p)?
    } else {
        let named_keys = [
            "workoscursorsessiontoken",
            "console_session",
            "session_token",
            "session_id",
            "sessionid",
            "sessionkey",
            "session_key",
            "connect.sid",
            "phpsessid",
            "jsessionid",
            "auth_cookie",
            "authcookieencrypted",
            "authcookie",
            "session",
            "auth",
        ];
        let matched = named_keys.into_iter().find(|key| {
            has_prefix_ignore_ascii_case(rest, key) && bytes.get(i + key.len()) == Some(&b'=')
        })?;
        i + matched.len()
    };

    let key_slice = &input[i..key_end];
    let val_start = key_end + 1;
    if input[val_start..].starts_with(REDACTED_MARKER) {
        return None;
    }
    let mut val_end = val_start;
    while val_end < bytes.len() && is_cookie_value_byte(bytes[val_end]) {
        val_end += 1;
    }
    if val_end == val_start {
        return None;
    }
    let value = &input[val_start..val_end];
    if value == REDACTED_MARKER {
        return None;
    }

    // Avoid false positives on usage metrics like `session=12.5` or `auth=true`.
    if key_slice.eq_ignore_ascii_case("session") {
        let looks_like_token = value.len() >= 12
            && !value
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'%'));
        if !looks_like_token {
            return None;
        }
    } else if key_slice.eq_ignore_ascii_case("auth") {
        let is_trivial = value.eq_ignore_ascii_case("true")
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("none")
            || value
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'%'));
        if is_trivial {
            return None;
        }
    }

    Some((val_end, format!("{key_slice}={REDACTED_MARKER}")))
}

fn scan_profile_username(input: &str, name_start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    if name_start >= bytes.len() || input[name_start..].starts_with(REDACTED_MARKER) {
        return None;
    }

    // Support usernames with spaces (e.g. `C:\Users\John Doe\AppData`) when a
    // path separator directly follows a non-space segment before any stop char.
    let mut scan = name_start;
    let mut spaces = 0usize;
    let mut first_ws: Option<usize> = None;
    let mut sep_after_ws: Option<usize> = None;

    while scan < bytes.len() {
        let b = bytes[scan];
        if matches!(
            b,
            b'\r'
                | b'\n'
                | b'"'
                | b'\''
                | b'<'
                | b'>'
                | b'|'
                | b'*'
                | b'?'
                | b':'
                | b';'
                | b','
                | b')'
                | b']'
                | b'}'
        ) {
            break;
        }
        if b == b'\\' || b == b'/' {
            if scan > name_start && !matches!(bytes[scan - 1], b' ' | b'\t') {
                sep_after_ws = Some(scan);
            }
            break;
        }
        if matches!(b, b' ' | b'\t') {
            if first_ws.is_none() {
                first_ws = Some(scan);
            }
            spaces += 1;
            if spaces > 2 {
                break;
            }
        }
        scan += 1;
    }

    let end = match sep_after_ws {
        Some(sep) => sep,
        None => first_ws.unwrap_or(scan),
    };
    if end <= name_start {
        return None;
    }
    let segment = &input[name_start..end];
    if segment == REDACTED_MARKER {
        return None;
    }
    Some(end)
}

fn try_redact_user_path(
    input: &str,
    i: usize,
    custom_profile: Option<&str>,
) -> Option<(usize, String)> {
    let bytes = input.as_bytes();
    let rest = &input[i..];

    // Windows `<Drive>:\Users\<user>`, `<Drive>:/Users/<user>`, `<Drive>:\\Users\\<user>`
    if bytes[i].is_ascii_alphabetic()
        && bytes.get(i + 1) == Some(&b':')
        && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
    {
        for users_sep in ["\\\\Users\\\\", "\\Users\\", "/Users/"] {
            if rest.len() > 2 + users_sep.len()
                && has_prefix_ignore_ascii_case(&rest[2..], users_sep)
            {
                let name_start = i + 2 + users_sep.len();
                if let Some(end) = scan_profile_username(input, name_start) {
                    return Some((end, format!("{}{REDACTED_MARKER}", &input[i..name_start])));
                }
            }
        }
    }

    // Unix `/home/<user>` or `/Users/<user>`
    if bytes[i] == b'/' && is_boundary_before(bytes, i) {
        for prefix in ["/home/", "/Users/", "/users/"] {
            if has_prefix_ignore_ascii_case(rest, prefix) {
                let name_start = i + prefix.len();
                if let Some(end) = scan_profile_username(input, name_start) {
                    return Some((end, format!("{}{REDACTED_MARKER}", &input[i..name_start])));
                }
            }
        }
    }

    // Custom non-standard `USERPROFILE` / `HOME` prefix if configured.
    if let Some(profile) = custom_profile {
        if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && has_prefix_ignore_ascii_case(rest, profile)
        {
            let end = i + profile.len();
            let next_ok = bytes
                .get(end)
                .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_' && *b != b'-');
            if next_ok {
                if let Some(sep_idx) = profile.rfind(['\\', '/']) {
                    let prefix_end = i + sep_idx + 1;
                    return Some((end, format!("{}{REDACTED_MARKER}", &input[i..prefix_end])));
                }
                return Some((end, REDACTED_MARKER.to_owned()));
            }
        }
    }

    None
}

fn is_email_local_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'-'
        )
}

fn try_redact_email(input: &str, i: usize) -> Option<(usize, String)> {
    let bytes = input.as_bytes();
    if !is_email_local_byte(bytes[i]) || bytes[i] == b'.' {
        return None;
    }
    if i > 0 && (is_email_local_byte(bytes[i - 1]) || bytes[i - 1] == b'@') {
        return None;
    }

    let mut pos = i;
    while pos < bytes.len() && is_email_local_byte(bytes[pos]) {
        pos += 1;
    }
    if pos == i || bytes[pos - 1] == b'.' || bytes.get(pos) != Some(&b'@') {
        return None;
    }
    pos += 1;

    let domain_start = pos;
    let mut label_count = 0usize;
    let mut last_label_start = domain_start;
    let mut last_label_len = 0usize;
    loop {
        let label_start = pos;
        while pos < bytes.len() && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'-') {
            pos += 1;
        }
        let label_len = pos - label_start;
        if label_len == 0 || bytes[label_start] == b'-' || bytes[pos - 1] == b'-' {
            return None;
        }
        label_count += 1;
        last_label_start = label_start;
        last_label_len = label_len;
        if bytes.get(pos) == Some(&b'.') {
            pos += 1;
            continue;
        }
        break;
    }

    if label_count < 2
        || !(2..=24).contains(&last_label_len)
        || !bytes[last_label_start..last_label_start + last_label_len]
            .iter()
            .all(u8::is_ascii_alphabetic)
    {
        return None;
    }
    if bytes
        .get(pos)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@'))
    {
        return None;
    }

    Some((pos, REDACTED_MARKER.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_logging_returns_while_the_log_mutex_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("recovery.log");
        let state = std::sync::Arc::new(DiagnoseState::new());
        state.enable(&path, false).unwrap();
        let guard = state.file.lock().unwrap();
        let worker_state = state.clone();
        let (completed, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_state.try_write(b"busy logger\n");
            completed.send(()).unwrap();
        });
        let result = completion.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
        worker.join().unwrap();
        result.expect("recovery must not wait for the logger");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        state.try_write(b"logger available\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "logger available\n"
        );
        state.disable();
    }

    #[test]
    fn disabled_logging_skips_formatting() {
        assert!(!is_enabled());
        log_lazy(|| panic!("disabled logging must not format messages"));
    }

    #[test]
    fn log_tail_tracks_appends_and_truncation_without_partial_utf8_lines() {
        let path = std::env::temp_dir().join(format!(
            "usagehub-log-tail-{}-{}.log",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "old line\n✓ latest\n").unwrap();
        assert_eq!(read_tail(&path, 128).unwrap(), "old line\n✓ latest\n");
        assert_eq!(read_tail(&path, 12).unwrap(), "✓ latest\n");
        // Cut through the multi-byte character: discard that incomplete line.
        assert_eq!(read_tail(&path, 9).unwrap(), "");
        writeln!(
            OpenOptions::new().append(true).open(&path).unwrap(),
            "new result"
        )
        .unwrap();
        assert!(read_tail(&path, 128).unwrap().ends_with("new result\n"));
        std::fs::write(&path, "restarted\n").unwrap();
        assert_eq!(read_tail(&path, 128).unwrap(), "restarted\n");
        let mut monitor = open_log(&path, false).unwrap();
        monitor.write_all(b"monitor\n").unwrap();
        let mut dashboard = open_log(&path, true).unwrap();
        dashboard.write_all(b"dashboard\n").unwrap();
        monitor.write_all(b"poll complete\n").unwrap();
        assert_eq!(
            read_tail(&path, 128).unwrap(),
            "monitor\ndashboard\npoll complete\n"
        );
        drop(dashboard);
        drop(monitor);
        let state = DiagnoseState::new();
        assert!(!state.enabled.load(Ordering::Acquire));
        state.enable(&path, true).unwrap();
        state.write(b"recording on\n");
        state.disable();
        assert!(!state.enabled.load(Ordering::Acquire));
        assert!(
            state.file.lock().unwrap().is_none(),
            "off must close the file"
        );
        let stopped = read_tail(&path, 1024).unwrap();
        state.write(b"must not be recorded\n");
        assert_eq!(read_tail(&path, 1024).unwrap(), stopped);
        state.enable(&path, true).unwrap();
        state.write(b"recording resumed\n");
        state.disable();
        assert_eq!(
            read_tail(&path, 1024).unwrap(),
            format!("{stopped}recording resumed\n")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rotation_caps_active_log_and_moves_previous_to_prev() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usagehub.log");
        let prev = rotated_log_path(&path);
        let state = DiagnoseState::with_max_bytes(64);
        state.enable(&path, false).unwrap();

        state.write(b"first-entry-012345678901234567890123456789\n");
        assert!(!prev.exists());

        // Second write pushes total over 64 bytes -> rotates first entry to `.prev`.
        state.write(b"second-entry-01234567890123456789012345678\n");
        assert!(prev.exists());
        assert_eq!(
            std::fs::read_to_string(&prev).unwrap(),
            "first-entry-012345678901234567890123456789\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "second-entry-01234567890123456789012345678\n"
        );

        // Next rotation overwrites `.prev` cleanly.
        state.write(b"third-entry-012345678901234567890123456789\n");
        assert_eq!(
            std::fs::read_to_string(&prev).unwrap(),
            "second-entry-01234567890123456789012345678\n"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "third-entry-012345678901234567890123456789\n"
        );
        state.disable();
    }

    #[test]
    fn redaction_masks_tokens_cookies_headers_emails_and_user_paths() {
        let input = concat!(
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig\n",
            "Cookie: __Host-console_session=sess_secret_123; theme=dark\n",
            "Set-Cookie: WorkosCursorSessionToken=workos_secret; HttpOnly\n",
            "standalone bearer sk-ant-oat01-abcdef123456 and __Host-console_session=abc123==\n",
            "{\"Authorization\": \"Basic dXNlcjpwYXNz\", \"authCookie\": \"raw_cookie_val\"}\n",
            "contact alice.smith+dev@example.co.uk at C:\\Users\\Alice Smith\\AppData\\Local\\Temp\\usagehub.log\n",
            "unix /home/bob/.config/opencode-bar/opencode-go.json\n",
            "claude account work usage received: session=12.5 weekly=45.0 stale=false\n",
        );

        let redacted = redact(input);

        for secret in [
            "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            "sess_secret_123",
            "workos_secret",
            "sk-ant-oat01-abcdef123456",
            "abc123==",
            "dXNlcjpwYXNz",
            "raw_cookie_val",
            "alice.smith+dev@example.co.uk",
            "Alice Smith",
            "bob",
        ] {
            assert!(
                !redacted.contains(secret),
                "secret {secret:?} leaked in:\n{redacted}"
            );
        }

        assert!(redacted.contains("Authorization: [REDACTED]"));
        assert!(redacted.contains("Cookie: [REDACTED]"));
        assert!(redacted.contains("Set-Cookie: [REDACTED]"));
        assert!(redacted.contains("bearer [REDACTED]"));
        assert!(redacted.contains("__Host-console_session=[REDACTED]"));
        assert!(redacted.contains("\"Authorization\": \"[REDACTED]\""));
        assert!(redacted.contains("\"authCookie\": \"[REDACTED]\""));
        assert!(redacted.contains("C:\\Users\\[REDACTED]\\AppData\\Local\\Temp\\usagehub.log"));
        assert!(redacted.contains("/home/[REDACTED]/.config/opencode-bar/opencode-go.json"));
        // Normal numeric usage telemetry must remain intact.
        assert!(redacted.contains("session=12.5 weekly=45.0 stale=false"));
        // Redaction is idempotent.
        assert_eq!(redact(&redacted), redacted);
    }

    #[test]
    fn format_line_applies_automatic_redaction_and_preserves_utf8() {
        let line = format_line("✓ signed in as user@example.com with Bearer tok_12345");
        assert!(line.contains("✓ signed in as [REDACTED] with Bearer [REDACTED]"));
        assert!(!line.contains("user@example.com"));
        assert!(!line.contains("tok_12345"));
    }
}
