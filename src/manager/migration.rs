//! Legacy Second adoption and old-tool cutover (integration slice).
//!
//! Observed legacy layout (read-only until explicit cutover):
//! - install dir `D:\Soft\usageBar` holding ONLY the old monitor executable
//!   (`claude-code-usage-monitor.exe`) and `sync-claude-accounts.py`
//! - actual configuration root `%APPDATA%\ClaudeCodeUsageMonitor` holding
//!   `settings.json`, `themes/` and the plaintext `profiles/second` export
//! - scheduled task `usageBar Claude accounts sync` (Ready; runs
//!   `D:\Soft\python.exe D:\Soft\usageBar\sync-claude-accounts.py`)
//! - `HKCU\...\Run` value `ClaudeCodeUsageMonitor`
//!
//! Root ranking: candidates come from known namespaces only, and a root that
//! actually carries settings/themes/profile data always outranks the
//! binary-only install dir, so the install dir can never shadow the real
//! configuration root.
//!
//! Rules: preview/import never touch production data (read-only scans plus
//! additive theme copies). A first migration into a fork that has no settings
//! file of its own imports the legacy non-auth preferences (poll interval,
//! language, UI, provider visibility, placement) once; the legacy account
//! registry, credentials and usage cache are never copied. Cutover runs ONLY
//! via `migration cutover --confirm cutover` and follows snapshot →
//! encrypted-backup → disable/stop → remove, with a generated rollback script.
//! Plaintext token exports are deleted only after that verified encrypted
//! backup and are restorable via `backup restore`; they are never staged or
//! copied as plaintext. Adopted Second paths stay in place; live profile
//! directories are never removed. No token material is ever read by this
//! module.
//!
//! Canonical Default binding: `import` also binds an unmapped built-in Claude
//! Default to the machine's real Desktop user-data dir through the existing
//! explicit `desktop_dir` (DesktopDir) auth contract, plus its separate
//! canonical CLI home as `config_dir`. Without it an unmapped Default keeps
//! the provider-default source, which may read `~/.claude/.credentials.json`
//! (a different CLI login). Only path fields are written: id, name, display
//! metadata, preferences and customized explicit mappings stay untouched and
//! ownership stays External, so an external Default is still covered only by
//! an explicit FullReset. No canonical Desktop layout markers → the Default
//! is left as-is and the missing markers are reported; a CLI login is never
//! substituted for the Desktop source.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::process::{run_bounded, ConsoleError};
use super::{backup, lifecycle, native, paths, state, AccountContext};
use crate::accounts::{AccountOwnership, AccountProfile};
use crate::app_settings::{PlacementOverride, SettingsFile};
use crate::providers::ProviderId;

/// Exact legacy scheduled-task name observed on the production machine.
pub const LEGACY_TASK_NAME: &str = "usageBar Claude accounts sync";
/// Exact legacy Run value name observed on the production machine.
pub const LEGACY_RUN_VALUE_NAME: &str = "ClaudeCodeUsageMonitor";
/// Observed legacy install dir. Production keeps ONLY the old executable and
/// sync script there, so it is never a settings/theme source.
pub const LEGACY_OBSERVED_DIR: &str = "D:\\Soft\\usageBar";
/// Substring hints identifying legacy autostart/executable references.
pub const LEGACY_HINTS: &[&str] = &[
    "usagebar",
    "claude-code-usage-monitor",
    "claudecodeusagemonitor",
    "claudeaccounts",
    "claude-accounts",
    "claude_code_usage",
    "claude-code-usage",
    "sync-claude-accounts",
];
/// Known legacy namespaces under roaming/local app data, config realm first.
/// `ClaudeCodeUsageMonitor` is the observed configuration root (settings,
/// themes, plaintext `profiles/second`); the remaining names are older/other
/// spellings of the same realm.
const LEGACY_DIR_NAMES: &[&str] = &[
    "ClaudeCodeUsageMonitor",
    "ClaudeCodeUsage",
    "claude-code-usage",
    "ClaudeAccounts",
    "usageBar",
    "UsageBar",
];
/// Exact owned bridge directory observed on the production machine: the old
/// monitor's plaintext Second export directory. Resolved by exact path only,
/// whether or not the credential copy is still present, so it is never adopted
/// as a Desktop source.
fn bridge_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("ClaudeCodeUsageMonitor")
            .join("profiles")
            .join("second"),
    )
}

/// Exact owned bridge credential copy, when it still exists.
fn owned_bridge_export() -> Option<PathBuf> {
    bridge_dir()
        .map(|dir| dir.join(".credentials.json"))
        .filter(|path| path.is_file())
}

fn contains_hint(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    LEGACY_HINTS.iter().any(|hint| lowered.contains(hint))
}

// ---------- process helpers ----------

/// OS-query budget for every `schtasks` probe in this module (console policy:
/// 15s deadline, 8MiB capture cap). The shared runner owns hidden-console
/// setup, concurrent drain, and job-object kill; never build ad-hoc piped
/// children here.
const SCHTASKS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const SCHTASKS_MAX_CAPTURE: usize = 8 * 1024 * 1024;

/// Hidden `schtasks` probe via the shared bounded runner. `Err` is always an
/// execution unknown (timeout/spawn): callers must propagate it as unknown,
/// never as task-absent. A task that simply does not exist surfaces as `Ok`
/// with a non-zero exit status instead.
fn run_hidden(program: &str, args: &[&str]) -> Result<std::process::Output, ConsoleError> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    run_bounded(
        &mut command,
        SCHTASKS_TIMEOUT,
        SCHTASKS_MAX_CAPTURE,
        program,
    )
}

/// Display-safe unknown-state detail for a failed `schtasks` probe. The
/// runner's `Display` carries only the command label (never raw arguments),
/// so this stays prompt/token safe.
fn unknown_detail(error: &ConsoleError) -> String {
    error.to_string()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ---------- scheduled-task scan ----------

/// First CSV field of a `schtasks /FO CSV /NH` line (the `\path\name`).
fn csv_first_field(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('"') {
        return None;
    }
    let end = trimmed[1..].find("\",")?;
    Some(trimmed[1..1 + end].to_string())
}

/// Exact-task presence probe. `Ok(true)` = present, `Ok(false)` = the query
/// ran and the task is not there. `Err` is an execution unknown
/// (timeout/spawn via the bounded runner) and must abort destructive paths,
/// never read as "no task" (a hung `schtasks` must not green-light export
/// deletion while the legacy sync task may still be rewriting them).
fn task_exists_exact(name: &str) -> Result<bool, String> {
    let output = run_hidden("schtasks", &["/Query", "/TN", name, "/FO", "CSV", "/NH"])
        .map_err(|error| unknown_detail(&error))?;
    Ok(probe_present(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        name,
    ))
}

/// Pure presence bit over a finished probe (testable without spawning):
/// a non-zero exit means the query ran and the task is absent, even when the
/// name appears in stderr text. Only a finished probe reaches here; timeouts
/// and spawn failures bypass this as `Err` above.
fn probe_present(status_ok: bool, stdout: &str, name: &str) -> bool {
    status_ok && stdout.contains(name)
}

/// Full task-name list. `Err` is an execution unknown (timeout/spawn) or a
/// failed list query -- never an empty list. Callers surface it as explicit
/// unknown instead of pretending no hint-tasks exist.
fn all_task_names() -> Result<Vec<String>, String> {
    let output = run_hidden("schtasks", &["/Query", "/FO", "CSV", "/NH"])
        .map_err(|error| unknown_detail(&error))?;
    if !output.status.success() {
        return Err(format!(
            "schtasks task list failed with status {}",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(csv_first_field)
        .collect())
}

/// Verbose `Task To Run` cell for the exact legacy task (corroboration).
/// `Err` is an execution unknown (timeout/spawn) or a failed query -- never
/// an empty corroboration. Callers after the disable point route it through
/// [`abort_after_disable`]; preview surfaces it as explicit unknown.
fn task_to_run(name: &str) -> Result<String, String> {
    let output = run_hidden("schtasks", &["/Query", "/TN", name, "/V", "/FO", "CSV"])
        .map_err(|error| unknown_detail(&error))?;
    if !output.status.success() {
        return Err(format!(
            "Unable to corroborate legacy task '{name}': schtasks exited {}",
            output.status
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return Ok(String::new());
    };
    let columns: Vec<String> = header
        .split("\",\"")
        .map(|cell| {
            cell.trim_matches('"')
                .trim()
                .to_ascii_lowercase()
                .replace(' ', "")
        })
        .collect();
    let Some(index) = columns.iter().position(|cell| cell == "tasktorun") else {
        return Ok(String::new());
    };
    for line in lines {
        // Split on `","` boundaries (values are quoted by schtasks CSV).
        let cells: Vec<&str> = line.split("\",\"").collect();
        if let Some(cell) = cells.get(index) {
            let value = cell.trim_matches('"').trim().to_string();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Ok(String::new())
}

/// Current `Status` cell (`Ready`, `Running`, `Disabled`, …) for one task via
/// `schtasks /Query /TN … /FO CSV /NH`. Used to verify the legacy sync task
/// is actually disabled before any credential snapshot. `Ok(None)` = the
/// query ran and the task is absent; `Err` = execution unknown
/// (timeout/spawn) which the disable gate must treat as unverified, never
/// as stopped.
fn task_status(name: &str) -> Result<Option<String>, String> {
    let output = run_hidden("schtasks", &["/Query", "/TN", name, "/FO", "CSV", "/NH"])
        .map_err(|error| unknown_detail(&error))?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(status_cell_for(
        &String::from_utf8_lossy(&output.stdout),
        name,
    ))
}

/// Pure status-cell match over finished CSV bytes (testable without
/// spawning): exact row (backslash/case tolerant), last cell wins.
fn status_cell_for(stdout: &str, name: &str) -> Option<String> {
    let wanted = name.trim_start_matches('\\').to_ascii_lowercase();
    for line in stdout.lines() {
        let cells: Vec<&str> = line.split("\",\"").collect();
        let first = cells.first()?.trim_matches('"').trim_start_matches('\\');
        if first.eq_ignore_ascii_case(&wanted) {
            return cells
                .last()
                .map(|cell| cell.trim_matches('"').trim().to_string());
        }
    }
    None
}

/// Post-disable abort path: best-effort re-enable of the exact legacy task
/// plus the pending-rollback pointer, so a failure after the disable never
/// leaves a bare error with the task silently off. Used only before ANY
/// destructive step; partial-failure errors after destructive work keep their
/// own rollback pointer and must NOT resurrect the task.
fn abort_after_disable(snapshot: &Path, error: String) -> String {
    let re_enabled = run_hidden("schtasks", &["/Change", "/TN", LEGACY_TASK_NAME, "/ENABLE"])
        .map(|output| output.status.success())
        .unwrap_or(false);
    // Best-effort only: an unknown verification query must not fail the abort
    // itself, so it collapses to an empty status here (the caller already
    // carries the original unknown in `error`).
    let state = task_status(LEGACY_TASK_NAME)
        .ok()
        .flatten()
        .unwrap_or_default();
    format!(
        "{error} (legacy task '{LEGACY_TASK_NAME}' was disabled and has been re-enabled: {re_enabled}; task status now '{state}'; pending rollback metadata: {}/rollback-pending.json)",
        snapshot.display()
    )
}

// ---------- registry / startup-folder scan ----------

/// `(value_name, data, value_type)` of every value under HKCU Run.
fn run_key_values() -> Result<Vec<(String, String, String)>, String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegEnumValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_READ, REG_SZ,
    };

    let subkey =
        crate::native_interop::wide_str("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let mut key = windows::Win32::System::Registry::HKEY::default();
    unsafe {
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("Unable to open HKCU Run key: {status:?}"));
        }
        let mut values = Vec::new();
        let mut index = 0u32;
        loop {
            let mut name = vec![0u16; 1024];
            let mut name_len = name.len() as u32;
            let mut value_type = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
            let mut data = vec![0u16; 16384];
            let mut data_len = (data.len() * 2) as u32;
            let status = RegEnumValueW(
                key,
                index,
                Some(PWSTR::from_raw(name.as_mut_ptr())),
                &mut name_len,
                Some(std::ptr::null::<u32>()),
                Some(&mut value_type.0 as *mut u32),
                Some(data.as_mut_ptr().cast::<u8>()),
                Some(&mut data_len as *mut u32),
            );
            if status == ERROR_NO_MORE_ITEMS {
                break;
            }
            if status != ERROR_SUCCESS {
                let _ = RegCloseKey(key);
                return Err(format!("Unable to enumerate HKCU Run values: {status:?}"));
            }
            let name = String::from_utf16_lossy(&name[..name_len as usize])
                .trim_end_matches('\0')
                .to_string();
            let words = data_len as usize / 2;
            let text = String::from_utf16_lossy(&data[..words])
                .trim_end_matches('\0')
                .to_string();
            let kind = if value_type == REG_SZ {
                "REG_SZ"
            } else {
                "REG_OTHER"
            };
            values.push((name, text, kind.to_string()));
            index += 1;
        }
        let _ = RegCloseKey(key);
        Ok(values)
    }
}

fn delete_run_value(name: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };

    let subkey =
        crate::native_interop::wide_str("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
    let wide_name = crate::native_interop::wide_str(name);
    unsafe {
        let mut key = windows::Win32::System::Registry::HKEY::default();
        let status = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            Some(0),
            KEY_SET_VALUE,
            &mut key,
        );
        if status != ERROR_SUCCESS {
            return Err(format!("Unable to open HKCU Run for write: {status:?}"));
        }
        let status = RegDeleteValueW(key, PCWSTR::from_raw(wide_name.as_ptr()));
        let _ = RegCloseKey(key);
        if status != ERROR_SUCCESS {
            return Err(format!("Unable to delete Run value '{name}': {status:?}"));
        }
        Ok(())
    }
}

fn startup_dir() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup"),
    )
}

fn startup_legacy_links() -> Vec<PathBuf> {
    let Some(directory) = startup_dir() else {
        return Vec::new();
    };
    std::fs::read_dir(&directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| contains_hint(stem))
        })
        .collect()
}

// ---------- legacy directory probes ----------

/// Confidence class of one known legacy root, decided by ACTUAL content never
/// by name alone. `Config` carries the old monitor's own data (settings
/// document, themes, assets, profiles); `Binary` carries only the executable
/// and/or sync script and is startup corroboration only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LegacyRootKind {
    Config,
    Binary,
}

/// Classify one candidate directory: `None` = absent or content-free (it
/// shadows nothing). A binary-only install dir can therefore never be mistaken
/// for the settings root.
fn legacy_root_kind(directory: &Path) -> Option<LegacyRootKind> {
    if !directory.is_dir() {
        return None;
    }
    let carries_config = directory.join("settings.json").is_file()
        || directory.join("themes").is_dir()
        || directory.join("assets").is_dir()
        || directory.join("profiles").is_dir();
    if carries_config {
        return Some(LegacyRootKind::Config);
    }
    let carries_binary = dir_names(directory).iter().any(|name| contains_hint(name));
    carries_binary.then_some(LegacyRootKind::Binary)
}

/// Known-namespace candidates, config-bearing roots ranked first and deduped.
/// The observed install dir is appended LAST so a binary-only directory can
/// never outrank (and thereby shadow) the real configuration root.
fn legacy_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for variable in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            for name in LEGACY_DIR_NAMES {
                candidates.push(PathBuf::from(&base).join(name));
            }
        }
    }
    candidates.push(PathBuf::from(LEGACY_OBSERVED_DIR));
    candidates.sort_by_cached_key(|directory| match legacy_root_kind(directory) {
        Some(LegacyRootKind::Config) => 0u8,
        _ => 1,
    });
    let mut seen: Vec<PathBuf> = Vec::new();
    candidates.retain(|directory| {
        if seen.iter().any(|known| same_home(known, directory)) {
            false
        } else {
            seen.push(directory.clone());
            true
        }
    });
    candidates
}

/// Exact observed Desktop Second root on the production machine.
pub const KNOWN_SECOND_DESKTOP_DIRS: &[&str] = &["D:/Soft/claude-multi-account/accounts/Second"];

/// Desktop layout markers: a real Desktop user-data dir carries config.json
/// plus the Local State sidecar. Never inferred from a bare name.
fn has_desktop_markers(directory: &Path) -> bool {
    directory.is_dir()
        && directory.join("config.json").is_file()
        && directory.join("Local State").exists()
}

/// The old plaintext bridge dir (`.../profiles/second` holding a credential
/// copy) is never a Desktop source, even when layout markers coincide.
fn is_bridge_dir(directory: &Path) -> bool {
    bridge_dir().is_some_and(|bridge| same_home(&bridge, directory))
}

/// Exact recognized Second dirs (never a substring sweep): the known observed
/// roots plus `<legacy>/profiles/second` exact-name dirs, excluding the old
/// plaintext bridge folder. Only config-bearing legacy roots are probed: an
/// install dir holding just the executable has no profile data to adopt.
fn exact_second_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for known in KNOWN_SECOND_DESKTOP_DIRS {
        let path = PathBuf::from(known);
        if path.is_dir() {
            dirs.push(path);
        }
    }
    for root in legacy_dir_candidates()
        .into_iter()
        .filter(|directory| legacy_root_kind(directory) == Some(LegacyRootKind::Config))
    {
        let exact = root.join("profiles").join("second");
        if exact.is_dir() && !is_bridge_dir(&exact) {
            dirs.push(exact);
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

// ---------- canonical Default Desktop binding ----------
//
// The built-in Claude Default starts life with no explicit source
// (`SourceKind::ProviderDefault`), so the poller resolves the provider default
// credential file — `~/.claude/.credentials.json`, i.e. the standalone CLI
// login, which is a DIFFERENT login from the Claude desktop app. Migration
// therefore binds the canonical Default to the machine's real Desktop
// user-data dir through the existing `desktop_dir` (DesktopDir) contract: the
// poller then reads `config.json` + `Local State` there and never falls back
// to CLI credential discovery. The canonical CLI home is recorded separately
// as `config_dir`, so history/config stay where they are and only skills are
// shared. Nothing else is rewritten: id, name, preferences, display metadata
// and any customized explicit mapping survive, and ownership stays External
// (an external Default is covered only by an explicit FullReset).

/// Stable id of the built-in Claude Default profile.
const DEFAULT_ACCOUNT_ID: &str = "default";

/// Publisher directories under one root: exact `<prefix><13 lowercase or
/// digit>` name shape, the layout the poller and cleanup already recognize
/// (`claude_pzs8sxrjxfjjc`, MSIX `Claude_<publisher>`). Any other name is
/// never enumerated.
fn publisher_dirs(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix(prefix).is_some_and(|publisher| {
                    publisher.len() == 13
                        && publisher
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
            })
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// Canonical Claude Desktop user-data candidates in the poller's precedence
/// order: physical `%LOCALAPPDATA%/Claude-Data/claude_*` dirs first (the
/// migrated layout), then the `%APPDATA%/Claude` alias, then MSIX package
/// caches. A directory name alone is never trusted: [`canonical_desktop_dir`]
/// requires layout markers.
#[cfg(test)]
fn canonical_desktop_dirs() -> Vec<PathBuf> {
    // Never discover a real login or make its skills homes cutover-eligible
    // from a test. Reuse the existing thread-local canonical-root fixture.
    native::canonical_default_dir_paths(ProviderId::Claude)
}

#[cfg(not(test))]
fn canonical_desktop_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = crate::known_folders::local_app_data_dir() {
        dirs.extend(publisher_dirs(&local.join("Claude-Data"), "claude_"));
    }
    if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
        dirs.push(roaming.join("Claude"));
    }
    if let Some(local) = crate::known_folders::local_app_data_dir() {
        for package in publisher_dirs(&local.join("Packages"), "Claude_") {
            let candidate = package.join("LocalCache").join("Roaming").join("Claude");
            if candidate.is_dir() {
                dirs.push(candidate);
            }
        }
    }
    let mut seen: Vec<String> = Vec::new();
    dirs.retain(|dir| {
        let key = native::canonical_spelling(dir);
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    dirs
}

/// Strip `\\?\` / `\??\` (and their UNC forms) from a junction target, so the
/// stored mapping uses the ordinary drive spelling every other slice compares.
fn strip_nt_prefix(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    let lower = text.to_ascii_lowercase();
    const UNC_PREFIX_LEN: usize = 8;
    const DEVICE_PREFIX_LEN: usize = 4;
    if lower.starts_with("\\\\?\\unc\\") || lower.starts_with("\\??\\unc\\") {
        return PathBuf::from(format!("\\\\{}", &text[UNC_PREFIX_LEN..]));
    }
    if lower.starts_with("\\\\?\\") || lower.starts_with("\\??\\") {
        return PathBuf::from(&text[DEVICE_PREFIX_LEN..]);
    }
    path
}

/// Physical directory behind `dir`, resolving a junction/symlink at `dir`
/// itself. `None` when the link cannot be resolved or lands on another link:
/// the stored mapping must never point through a link chain (backups, cleanup
/// validation and process attribution all refuse to traverse one).
fn physical_desktop_dir(dir: &Path) -> Option<PathBuf> {
    if !paths::is_reparse_point(dir) {
        return Some(dir.to_path_buf());
    }
    let target = paths::read_link_target(dir).ok()?;
    let target = if target.is_absolute() {
        target
    } else {
        dir.parent()?.join(target)
    };
    if paths::is_reparse_point(&target) {
        return None;
    }
    Some(strip_nt_prefix(target))
}

/// First canonical Desktop dir carrying real layout markers, as a physical
/// path. `None` = no usable Desktop layout on this machine.
fn canonical_desktop_dir() -> Option<PathBuf> {
    canonical_desktop_dirs().into_iter().find_map(|dir| {
        if !has_desktop_markers(&dir) {
            return None;
        }
        physical_desktop_dir(&dir).filter(|physical| has_desktop_markers(physical))
    })
}

/// Read-only outcome of the canonical Default Desktop binding.
enum DefaultBindingPlan {
    /// The Default already carries an explicit source: a customized fork
    /// mapping (or a previous binding). Never rewritten.
    AlreadyMapped,
    /// Not the canonical external Default (absent, or Managed).
    Ineligible { reason: String },
    /// No canonical Desktop dir carries readable layout markers.
    MissingDesktop { searched: Vec<PathBuf> },
    /// Binding would overlap another registered account's source root.
    Overlap { path: PathBuf, other: String },
    /// Safe to bind `desktop_dir` (DesktopDir auth) plus its separate CLI home.
    Bind {
        desktop_dir: PathBuf,
        config_dir: PathBuf,
    },
}

impl DefaultBindingPlan {
    fn report(&self) -> Value {
        match self {
            Self::AlreadyMapped => json!({
                "state": "already_mapped",
                "reason": "Default already carries an explicit mapping; customized fork mappings are preserved",
            }),
            Self::Ineligible { reason } => json!({"state": "skipped", "reason": reason}),
            Self::MissingDesktop { searched } => json!({
                "state": "missing_desktop",
                "markers": "config.json + Local State",
                "searched": searched
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>(),
                "reason": "no canonical Claude Desktop data dir carries readable layout markers; no Desktop binding was applied",
            }),
            Self::Overlap { path, other } => json!({
                "state": "refused",
                "path": path.display().to_string(),
                "other": other,
                "reason": "the canonical Default source overlaps another registered account's root",
            }),
            Self::Bind {
                desktop_dir,
                config_dir,
            } => json!({
                "state": "would_bind",
                "desktop_dir": desktop_dir.display().to_string(),
                "config_dir": config_dir.display().to_string(),
                "ownership": "external",
            }),
        }
    }
}

/// Plan the canonical Default binding without writing anything. Only the
/// built-in External Default with NO explicit source field at all is
/// eligible: any explicit field is a user/fork mapping and is preserved
/// verbatim.
fn default_binding_plan(settings: &SettingsFile) -> DefaultBindingPlan {
    let accounts = &settings.accounts.claude;
    let Some(default) = accounts
        .profiles
        .iter()
        .find(|profile| profile.id.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ID))
    else {
        return DefaultBindingPlan::Ineligible {
            reason: "no canonical Default profile is registered".to_string(),
        };
    };
    if default.ownership != AccountOwnership::External {
        return DefaultBindingPlan::Ineligible {
            reason: "Default is Managed; its own mapping is preserved".to_string(),
        };
    }
    if [
        &default.desktop_dir,
        &default.config_dir,
        &default.credentials_path,
    ]
    .iter()
    .any(|field| !field.trim().is_empty())
    {
        return DefaultBindingPlan::AlreadyMapped;
    }
    let Some(desktop_dir) = canonical_desktop_dir() else {
        return DefaultBindingPlan::MissingDesktop {
            searched: canonical_desktop_dirs(),
        };
    };
    // The CLI home stays separate: the Desktop mapping must never move the
    // canonical CLI login/history into the desktop tree.
    let Some(config_dir) = canonical_default_base() else {
        return DefaultBindingPlan::Ineligible {
            reason: "no canonical Claude CLI home could be resolved".to_string(),
        };
    };
    // Single ownership: the Default may never share a root with a sibling.
    let desktop_spelling = native::canonical_spelling(&desktop_dir);
    let cli_spelling = native::canonical_spelling(&config_dir);
    for profile in accounts
        .profiles
        .iter()
        .filter(|profile| !profile.id.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ID))
    {
        for field in [
            &profile.desktop_dir,
            &profile.config_dir,
            &profile.credentials_path,
        ] {
            let trimmed = field.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(existing) = paths::require_absolute(Path::new(trimmed)) else {
                continue;
            };
            let spelling = native::canonical_spelling(&existing);
            if native::overlaps(&desktop_spelling, &spelling)
                || native::overlaps(&cli_spelling, &spelling)
            {
                return DefaultBindingPlan::Overlap {
                    path: existing,
                    other: format!("{} ({})", profile.name, profile.id),
                };
            }
        }
    }
    DefaultBindingPlan::Bind {
        desktop_dir,
        config_dir,
    }
}

/// Bind an unmapped canonical Claude Default to the real Desktop data dir via
/// the existing explicit DesktopDir auth contract. Never fails the adoption:
/// every outcome is reported. Id, name, display metadata, preferences and
/// ownership are untouched; the canonical CLI home is recorded separately so
/// only skills are shared.
fn bind_canonical_default_desktop() -> Value {
    match default_binding_plan(&state::load_fresh()) {
        DefaultBindingPlan::Bind { .. } => {}
        other => return other.report(),
    }
    // Serialize with poll/cleanup/open on the canonical Default, then commit
    // through the transactional settings writer. A busy account is reported,
    // never forced.
    let _guard = match state::AccountOperationGuard::acquire(
        ProviderId::Claude,
        DEFAULT_ACCOUNT_ID,
        lifecycle::ACCOUNT_OP_TIMEOUT_MS,
    ) {
        Ok(guard) => guard,
        Err(error) => {
            return json!({
                "state": "busy",
                "reason": format!("canonical Default is busy; re-run `migration import` ({error})"),
            })
        }
    };
    match state::mutate_settings(|settings| {
        // Re-plan on the committed view: another writer may have mapped (or
        // cleared) the Default while this call waited for the guard.
        match default_binding_plan(settings) {
            DefaultBindingPlan::Bind {
                desktop_dir,
                config_dir,
            } => {
                let profile = settings
                    .accounts
                    .claude
                    .profiles
                    .iter_mut()
                    .find(|profile| profile.id.eq_ignore_ascii_case(DEFAULT_ACCOUNT_ID))
                    .ok_or_else(|| "no canonical Default profile is registered".to_string())?;
                let desktop_text = desktop_dir.display().to_string();
                let config_text = config_dir.display().to_string();
                profile.desktop_dir = desktop_text.clone();
                profile.config_dir = config_text.clone();
                // Reject a mapping the poller cannot serve before saving.
                AccountContext::from_profile(ProviderId::Claude, profile)?;
                Ok(json!({
                    "state": "bound",
                    "desktop_dir": desktop_text,
                    "config_dir": config_text,
                    "ownership": "external",
                }))
            }
            other => Ok(other.report()),
        }
    }) {
        Ok(value) => value,
        Err(error) => json!({"state": "refused", "reason": error}),
    }
}

fn theme_files_in(directory: &Path) -> Vec<String> {
    let themes = directory.join("themes");
    std::fs::read_dir(&themes)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        })
        .filter_map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .collect()
}

/// Absolute `.json` paths mentioned in a legacy settings document (any shape):
/// every string value that names an absolute file. Pure string extraction;
/// existence and ownership filtering happen in [`exact_token_exports`].
fn json_export_paths(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        serde_json::Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.len() > 8
                && trimmed.to_ascii_lowercase().ends_with(".json")
                && Path::new(trimmed).is_absolute()
            {
                out.push(trimmed.to_string());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                out.extend(json_export_paths(item));
            }
        }
        serde_json::Value::Object(map) => {
            for (_, nested) in map {
                out.extend(json_export_paths(nested));
            }
        }
        _ => {}
    }
    out
}

/// Absolute `.json` paths referenced by the legacy sync script (its export
/// targets). Pure text scan for drive-letter absolutes ending in `.json`.
fn script_export_paths(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index + 3 < bytes.len() {
        let drive = bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && (bytes[index + 2] == b'\\' || bytes[index + 2] == b'/');
        if !drive {
            index += 1;
            continue;
        }
        let mut end = index + 3;
        while end < bytes.len()
            && !matches!(bytes[end], b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'\'')
        {
            end += 1;
        }
        let candidate = text[index..end]
            .trim_end_matches([',', ';', ')', ']', '}'])
            .to_string();
        if candidate.len() > 8 && candidate.to_ascii_lowercase().ends_with(".json") {
            out.push(candidate);
        }
        index = end;
    }
    out.sort();
    out.dedup();
    out
}

/// Credential-shape predicate for export enumeration: a credentials file
/// (`.credentials.json` / `credentials*.json`) inside Claude Second scope
/// (some ancestor dir named exactly `second`), and never inside
/// themes/assets/provider-config locations.
fn is_credential_export(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let shape =
        name == ".credentials.json" || (name.starts_with("credentials") && name.ends_with(".json"));
    if !shape {
        return false;
    }
    let second_scope = path.ancestors().any(|ancestor| {
        ancestor
            .file_name()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.eq_ignore_ascii_case("second"))
    });
    if !second_scope {
        return false;
    }
    let banned = path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        text == "themes"
            || text == "assets"
            || text == "claude-config"
            || text == ".codex"
            || text == "codex"
            || text == "settings.json"
    });
    !banned
}

/// Exact plaintext-export enumeration. ONLY: the owned bridge path plus
/// credential-shaped Claude Second outputs confirmed by the old settings
/// document or the sync script that (a) exist as files and (b) live under a
/// confirmed legacy root (or are the bridge itself). Never arbitrary filename
/// hints, so themes/assets/settings/Codex configs can never be swept in.
fn exact_token_exports() -> Vec<(PathBuf, PathBuf)> {
    // Scope proof: only known-namespace roots that actually contain legacy
    // content may scope an export. Anything else (other providers, unrelated
    // app-data dirs) is never a root.
    let mut roots: Vec<PathBuf> = legacy_dir_candidates()
        .into_iter()
        .filter(|directory| legacy_root_kind(directory).is_some())
        .collect();
    let mut hits: Vec<(PathBuf, PathBuf)> = Vec::new();
    // 1. Owned bridge export (credential shape by construction). The bridge
    //    namespace root is the config root that owns it.
    if let Some(bridge) = owned_bridge_export() {
        let root = bridge_dir().and_then(|dir| {
            dir.parent()
                .and_then(|parent| parent.parent())
                .map(Path::to_path_buf)
        });
        if let Some(root) = root {
            if !roots.iter().any(|known| same_home(known, &root)) {
                roots.push(root.clone());
            }
            if is_credential_export(&bridge) {
                hits.push((bridge, root));
            }
        }
    }
    // 2. Old settings documents and the sync script inside confirmed roots.
    let mut candidates = Vec::new();
    for root in &roots {
        if let Ok(text) = std::fs::read_to_string(root.join("settings.json")) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                candidates.extend(json_export_paths(&value));
            }
        }
        if let Ok(text) = std::fs::read_to_string(root.join("sync-claude-accounts.py")) {
            candidates.extend(script_export_paths(&text));
        }
    }
    for candidate in candidates {
        let path = PathBuf::from(&candidate);
        if !path.is_absolute() || !path.is_file() || !is_credential_export(&path) {
            continue;
        }
        if hits.iter().any(|(known, _)| same_home(known, &path)) {
            continue;
        }
        if let Some(root) = roots.iter().find(|root| paths::is_within(root, &path)) {
            hits.push((path, root.clone()));
        }
    }
    hits.sort();
    hits.dedup();
    hits
}

// ---------- public surface ----------

/// Read-only migration preview. Never fails on a fresh user: absent legacy
/// dirs/tasks simply yield empty findings.
pub fn preview() -> Result<Value, String> {
    // Tri-state probe: `Ok(true)` present, `Ok(false)` absent, `Err` unknown
    // (timeout/spawn). Unknown is surfaced explicitly (`present: null` plus
    // `unknown`) and never collapses to "no task".
    let task_probe = task_exists_exact(LEGACY_TASK_NAME);
    let task_unknown: Option<String> = task_probe.as_ref().err().cloned();
    let exact_task_present = matches!(task_probe, Ok(true));
    let task_present_json = match task_unknown {
        Some(_) => Value::Null,
        None => json!(exact_task_present),
    };
    let run_command = if exact_task_present {
        task_to_run(LEGACY_TASK_NAME).unwrap_or_default()
    } else {
        String::new()
    };
    let hint_probe = all_task_names();
    let hint_unknown: Option<String> = hint_probe.as_ref().err().cloned();
    let other_tasks: Vec<String> = hint_probe
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            !name.eq_ignore_ascii_case(LEGACY_TASK_NAME)
                && !name
                    .trim_start_matches('\\')
                    .eq_ignore_ascii_case(LEGACY_TASK_NAME)
                && contains_hint(name)
        })
        .collect();
    let run_values = run_key_values().unwrap_or_default();
    let legacy_run: Vec<Value> = run_values
        .iter()
        .filter(|(name, data, _)| name == LEGACY_RUN_VALUE_NAME || contains_hint(data))
        .map(|(name, data, kind)| {
            json!({
                "name": name,
                "data": data,
                "type": kind,
                "exact": name == LEGACY_RUN_VALUE_NAME,
            })
        })
        .collect();
    let startup_links: Vec<String> = startup_legacy_links()
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    // Fork settings are read ONCE so the settings-import probe below can run on
    // a clone (preview stays read-only).
    let settings = state::load_fresh();
    let confirmed_dir = confirmed_legacy_dir();
    let mut legacy_dirs = Vec::new();
    for directory in legacy_dir_candidates() {
        let Some(kind) = legacy_root_kind(&directory) else {
            continue;
        };
        let kind_label = match kind {
            LegacyRootKind::Config => "config",
            LegacyRootKind::Binary => "binary",
        };
        let themes = theme_files_in(&directory);
        legacy_dirs.push(json!({
            "dir": directory.display().to_string(),
            "kind": kind_label,
            "confirmed": confirmed_dir
                .as_ref()
                .is_some_and(|confirmed| same_home(confirmed, &directory)),
            "settings": directory.join("settings.json").is_file(),
            "themes": themes,
            "assets": directory.join("assets").is_dir(),
        }));
    }
    // Legacy settings coverage: the non-auth preference keys a first import
    // would take. Read-only probe on a clone; nothing is written here.
    let legacy_settings = confirmed_dir
        .as_ref()
        .map(|root| root.join("settings.json"))
        .filter(|document| document.is_file())
        .map(|document| {
            let mut probe = settings.clone();
            let keys = std::fs::read_to_string(&document)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .map(|value| apply_legacy_preferences(&value, &mut probe))
                .unwrap_or_default();
            json!({
                "path": document.display().to_string(),
                "fork_fresh": !crate::app_settings::settings_path().exists(),
                "importable_keys": keys,
            })
        })
        .unwrap_or(Value::Null);
    // Exact recognized Second dirs (never a substring sweep) with layout
    // markers and bridge exclusion for the adoption decision below.
    let second_dirs: Vec<Value> = exact_second_dirs()
        .into_iter()
        .map(|path| {
            json!({
                "path": path.display().to_string(),
                "desktop_markers": has_desktop_markers(&path),
                "bridge": is_bridge_dir(&path),
            })
        })
        .collect();
    // Exact export enumeration (owned bridge + settings/sync-script confirmed
    // paths under legacy roots only); each entry verified as an existing file.
    let token_exports: Vec<Value> = exact_token_exports()
        .into_iter()
        .map(|(path, root)| {
            json!({
                "path": path.display().to_string(),
                "root": root.display().to_string(),
                "bytes": path.metadata().map(|meta| meta.len()).unwrap_or(0),
            })
        })
        .collect();
    let mut registry = Vec::new();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        if let Some(accounts) = settings.accounts.get(provider) {
            for profile in &accounts.profiles {
                registry.push(json!({
                    "provider": provider.descriptor().key,
                    "id": profile.id,
                    "name": profile.name,
                    "ownership": serde_json::to_value(profile.ownership).unwrap_or(Value::Null),
                    "desktop_dir": profile.desktop_dir,
                    "config_dir": profile.config_dir,
                }));
            }
        }
    }
    // Canonical Default Desktop binding preview (read-only plan: nothing is
    // written here, the plan is re-evaluated transactionally by `import`).
    let default_binding = default_binding_plan(&settings).report();
    let mut notes: Vec<String> = Vec::new();
    if default_binding["state"] == "would_bind" {
        notes.push(format!(
            "canonical Default will be bound to the real Desktop data dir '{}' (explicit DesktopDir source; separate CLI home '{}'; External ownership, id/name/preferences and customized mappings unchanged)",
            default_binding["desktop_dir"].as_str().unwrap_or_default(),
            default_binding["config_dir"].as_str().unwrap_or_default(),
        ));
    } else if default_binding["state"] == "missing_desktop" {
        notes.push(
            "canonical Default is unmapped but no Claude Desktop data dir carries layout markers (config.json + Local State); import leaves the provider-default source as-is and never substitutes the CLI login"
                .to_string(),
        );
    }
    if legacy_settings["fork_fresh"].as_bool() == Some(true) {
        let count = legacy_settings["importable_keys"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
        if count > 0 {
            notes.push(format!(
                "first migration imports {count} legacy preference(s) from the legacy settings document (accounts, credentials and usage cache excluded)"
            ));
        }
    }
    if let Some(unknown) = task_unknown.as_ref() {
        notes.push(format!(
            "legacy sync task state is UNKNOWN ({unknown}); cutover aborts before any backup or deletion until schtasks answers"
        ));
    } else if exact_task_present {
        notes.push(
            "legacy sync task is present; cutover disables+stops it before any export removal"
                .to_string(),
        );
    }
    if let Some(unknown) = hint_unknown.as_ref() {
        notes.push(format!(
            "task hint sweep is UNKNOWN ({unknown}); other_hint_tasks is incomplete, not empty"
        ));
    }
    if !legacy_run.is_empty() {
        notes.push(
            "legacy autostart Run values will be moved to the cutover snapshot with rollback"
                .to_string(),
        );
    }
    if registry.iter().any(|entry| {
        entry["id"] == "second" || entry["name"] == "Second" && entry["provider"] == "claude"
    }) {
        notes.push("a Second account is already registered; import adopts it in place".to_string());
    } else {
        notes.push("no Second account registered yet; import adopts the legacy second export with stable id 'second'".to_string());
    }
    let skills_preview = skills_preview_json();
    if skills_preview["skills_conflicts"].as_u64().unwrap_or(0) > 0 {
        notes.push(
            "shared-skills conflicts need manual resolution before cutover junctions them"
                .to_string(),
        );
    }
    notes.push(
        "cutover requires explicit `--confirm cutover`; it snapshots and encrypts a backup before removing any legacy autostart entry or plaintext export"
            .to_string(),
    );
    Ok(json!({
        "legacy_task": {
            "name": LEGACY_TASK_NAME,
            "present": task_present_json,
            "unknown": task_unknown,
            "task_to_run": run_command,
        },
        "other_hint_tasks": other_tasks,
        "other_hint_tasks_unknown": hint_unknown,
        "run_values": legacy_run,
        "startup_links": startup_links,
        "legacy_dirs": legacy_dirs,
        "legacy_settings": legacy_settings,
        "second_dirs": second_dirs,
        "default_binding": default_binding,
        "token_exports": token_exports,
        "skills": skills_preview,
        "registry": registry,
        "notes": notes,
    }))
}

/// Highest-confidence confirmed configuration root among the known candidates.
/// Only a root carrying settings/themes/assets/profiles qualifies, so the
/// binary-only install dir can never be selected as the settings/theme source.
fn confirmed_root(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|directory| legacy_root_kind(directory) == Some(LegacyRootKind::Config))
}

fn confirmed_legacy_dir() -> Option<PathBuf> {
    confirmed_root(legacy_dir_candidates())
}

// ---------- legacy settings import ----------
//
// The legacy monitor's `settings.json` shares the fork schema, but only its
// non-auth preferences may cross over. Accounts (the plaintext bridge Second
// points `config_dir` at `.../profiles/second`), `credentials_path`,
// `selected`/`used_ids`, the update-check stamp and the separate usage cache
// (which also holds manual reset state) are NEVER imported: account adoption
// stays with the validated Desktop flow.

/// Apply the allowlisted legacy preference keys onto `settings` and return the
/// key names actually taken. Pure in-memory mapping: no filesystem access
/// beyond resolving the fork themes dir for the active-theme transfer, so the
/// same function can probe and apply. `normalize()` on save still owns range
/// validation (poll interval, dashboard dimensions, opacity clamp).
fn apply_legacy_preferences(legacy: &Value, settings: &mut SettingsFile) -> Vec<String> {
    let Some(object) = legacy.as_object() else {
        return Vec::new();
    };
    let mut imported: Vec<String> = Vec::new();
    if let Some(interval) = object
        .get("poll_interval_ms")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    {
        settings.poll_interval_ms = interval;
        imported.push("poll_interval_ms".into());
    }
    if let Some(code) = object
        .get("language")
        .and_then(Value::as_str)
        .and_then(crate::localization::LanguageId::from_code)
    {
        settings.language = Some(code.code().to_string());
        imported.push("language".into());
    }
    // Theme SELECTION only, and only onto an already-copied fork theme: a
    // legacy absolute path is never carried into the fork settings.
    if let Some(raw) = object.get("active_theme_path").and_then(Value::as_str) {
        let legacy_path = Path::new(raw);
        if legacy_path.is_absolute() {
            if let Some(name) = legacy_path.file_name() {
                let fork_file = crate::theme_engine::themes_directory().join(name);
                if fork_file.is_file() {
                    settings.active_theme_path = Some(fork_file.display().to_string());
                    imported.push("active_theme_path".into());
                }
            }
        }
    }
    if let Some(enabled) = object.get("usage_countdown").and_then(Value::as_bool) {
        settings.usage_countdown = enabled;
        imported.push("usage_countdown".into());
    }
    if let Some(enabled) = object.get("lock_taskbar").and_then(Value::as_bool) {
        settings.lock_taskbar = enabled;
        imported.push("lock_taskbar".into());
    }
    if let Some(width) = object.get("dashboard_width").and_then(Value::as_f64) {
        settings.dashboard_width = Some(width as f32);
        imported.push("dashboard_width".into());
    }
    if let Some(height) = object.get("dashboard_height").and_then(Value::as_f64) {
        settings.dashboard_height = Some(height as f32);
        imported.push("dashboard_height".into());
    }
    if let Some(opacity) = object
        .get("floating_card_opacity")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
    {
        settings.floating_card_opacity = Some(opacity);
        imported.push("floating_card_opacity".into());
    }
    // Placement: pre-theme offsets stay pending so the startup migration can
    // transfer them exactly like a legacy fork settings file would.
    if let Some(offset) = object
        .get("tray_offset")
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
    {
        settings.tray_offset = offset;
        settings.legacy_placement_pending = true;
        imported.push("tray_offset".into());
    }
    if let Some(index) = object
        .get("taskbar_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    {
        settings.taskbar_index = index;
        settings.legacy_placement_pending = true;
        imported.push("taskbar_index".into());
    }
    if let Some(placement) = object
        .get("placement_override")
        .and_then(|value| serde_json::from_value::<PlacementOverride>(value.clone()).ok())
    {
        settings.placement_override = Some(placement);
        imported.push("placement_override".into());
    }
    if let Some(visible) = object.get("widget_visible").and_then(Value::as_bool) {
        settings.widget_visible = visible;
        settings.legacy_visibility_pending = true;
        imported.push("widget_visible".into());
    }
    // Provider visibility switches.
    for (key, provider) in [
        ("show_claude_code", ProviderId::Claude),
        ("show_codex", ProviderId::Codex),
        ("show_opencode", ProviderId::OpenCode),
        ("show_cursor", ProviderId::Cursor),
        ("show_grok", ProviderId::Grok),
        ("show_copilot", ProviderId::Copilot),
    ] {
        if let Some(enabled) = object.get(key).and_then(Value::as_bool) {
            settings.set_provider_enabled(provider, enabled);
            imported.push(key.to_string());
        }
    }
    if let Some(enabled) = object.get("show_antigravity").and_then(Value::as_bool) {
        settings.set_provider_enabled(ProviderId::Antigravity, enabled);
        imported.push("show_antigravity".into());
    }
    if let Some(enabled) = object
        .get("show_antigravity_gemini")
        .and_then(Value::as_bool)
    {
        settings.set_antigravity_gemini_enabled(enabled);
        imported.push("show_antigravity_gemini".into());
    }
    if let Some(enabled) = object
        .get("show_antigravity_external")
        .and_then(Value::as_bool)
    {
        settings.set_antigravity_external_enabled(enabled);
        imported.push("show_antigravity_external".into());
    }
    imported
}

/// One-shot import of the legacy non-auth preferences from the confirmed
/// configuration root. `fresh_fork` must be captured BEFORE any adoption
/// mutation: only a fork that had no settings file of its own is populated, so
/// a repeat import can never overwrite preferences the user already edited.
fn import_legacy_preferences(fresh_fork: bool) -> Result<Value, String> {
    if !fresh_fork {
        return Ok(json!({
            "imported": false,
            "reason": "fork settings already exist; user preferences preserved",
        }));
    }
    let Some(root) = confirmed_legacy_dir() else {
        return Ok(json!({"imported": false, "reason": "no confirmed legacy configuration root"}));
    };
    let document = root.join("settings.json");
    let Ok(text) = std::fs::read_to_string(&document) else {
        return Ok(json!({"imported": false, "reason": "legacy settings document not readable"}));
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(json!({"imported": false, "reason": "legacy settings document is not JSON"}));
    };
    let mut probe = state::load_fresh();
    let keys = apply_legacy_preferences(&value, &mut probe);
    if keys.is_empty() {
        return Ok(json!({
            "imported": false,
            "source": document.display().to_string(),
            "reason": "legacy settings document has no importable preferences",
        }));
    }
    state::mutate_settings(|settings| {
        apply_legacy_preferences(&value, settings);
        Ok(())
    })?;
    Ok(json!({
        "imported": true,
        "source": document.display().to_string(),
        "keys": keys,
    }))
}

/// Adopt Second in place: existing registry Second becomes Managed, or the
/// legacy second export is registered under the stable id `second` (never
/// reused afterwards). Copies legacy themes/assets into the fork namespace,
/// skipping existing files, and rewrites a stale `active_theme_path` that
/// still points into a legacy dir. On a FIRST migration (no fork settings file
/// yet) the legacy non-auth preferences (poll interval, language, UI, provider
/// visibility, placement) are imported exactly once; repeat runs preserve the
/// fork's own edits. The legacy account registry and credentials are never
/// copied. Never reads credential files.
///
/// Also binds an unmapped canonical Claude Default to the real Desktop
/// user-data dir ([`bind_canonical_default_desktop`]). That step is
/// independent of the Second plan (a Desktop-only machine has no Second to
/// adopt) and is reported, never forced: a busy/missing/overlapping Default
/// leaves the adoption result intact with an explicit state.
pub fn adopt() -> Result<Value, String> {
    // Fresh-fork detection BEFORE any adoption mutation: a fork with no
    // settings file of its own receives the legacy preferences exactly once.
    let fork_was_fresh = !crate::app_settings::settings_path().exists();
    // Decide the registry plan from fresh state BEFORE any filesystem change.
    let settings = state::load_fresh();
    let existing_second = [ProviderId::Claude, ProviderId::Codex]
        .into_iter()
        .find_map(|provider| {
            settings.accounts.get(provider).and_then(|accounts| {
                accounts
                    .profiles
                    .iter()
                    .find(|profile| {
                        profile.id.eq_ignore_ascii_case("second")
                            || profile.name.eq_ignore_ascii_case("second")
                    })
                    .map(|profile| (provider, profile.id.clone()))
            })
        });
    // Exact recognized Second only (never a substring sweep, never the
    // plaintext bridge as a Desktop). Markers + prospective ownership
    // validation decide below; anything else is refused with guidance.
    let legacy_export = exact_second_dirs().into_iter().next();
    // Canonical Default Desktop binding runs first and independently of the
    // Second plan: a Desktop-only machine has no Second to adopt, yet its
    // unmapped Default must stop falling back to the CLI login. Never
    // returns Err — every outcome is reported.
    let default_binding = bind_canonical_default_desktop();
    // Theme/asset copies next: purely additive, skip-existing, never overwrite.
    let (themes_copied, themes_skipped, assets_copied) = copy_legacy_themes()?;
    enum Plan {
        AlreadyManaged {
            provider: ProviderId,
            id: String,
        },
        AdoptExisting {
            provider: ProviderId,
            id: String,
        },
        InsertSecond {
            desktop_dir: String,
            config_dir: String,
        },
        Refused {
            reason: String,
        },
        Nothing,
    }
    let plan = match (existing_second, legacy_export) {
        (Some((provider, id)), _) => {
            let managed = settings
                .accounts
                .get(provider)
                .and_then(|accounts| accounts.find(&id))
                .is_some_and(|profile| profile.ownership == AccountOwnership::Managed);
            if managed {
                Plan::AlreadyManaged { provider, id }
            } else {
                Plan::AdoptExisting { provider, id }
            }
        }
        // `exact_second_dirs` only ever yields recognized directories, so the
        // markers/ownership checks below are the only refusals that can fire.
        (None, Some(export)) => {
            // Observed layout: the Second dir itself is the Desktop
            // user-data dir, and its existing `claude-config` child is the
            // CLI config dir. Both stay in place, validated absolute-only,
            // never moved on rename.
            if !has_desktop_markers(&export) {
                Plan::Refused {
                    reason: format!(
                        "Recognized Second dir '{}' has no Desktop layout markers (config.json + Local State); import it explicitly instead",
                        export.display()
                    ),
                }
            } else {
                let claude_config = export.join("claude-config");
                let config_dir = if claude_config.is_dir() {
                    claude_config.display().to_string()
                } else {
                    String::new()
                };
                // Prospective ownership validation through the shared
                // lifecycle gate: the same checks cleanup enforces before any
                // deletion (markers, broad/canonical/sibling overlap).
                let prospective = AccountProfile {
                    id: "second".to_string(),
                    name: "Second".to_string(),
                    config_dir: config_dir.clone(),
                    desktop_dir: export.display().to_string(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                };
                match AccountContext::from_profile(ProviderId::Claude, &prospective)
                    .and_then(|context| lifecycle::owned_deletion_root(&context).map(|_| ()))
                {
                    Ok(()) => Plan::InsertSecond {
                        desktop_dir: export.display().to_string(),
                        config_dir,
                    },
                    Err(error) => Plan::Refused {
                        reason: format!(
                            "Prospective Second root '{}' fails ownership validation: {error}",
                            export.display()
                        ),
                    },
                }
            }
        }
        (None, None) => Plan::Nothing,
    };
    let adopted = match plan {
        Plan::AlreadyManaged { provider, id } => {
            json!({"provider": provider.descriptor().key, "id": id, "state": "already_managed"})
        }
        Plan::AdoptExisting { provider, id } => {
            state::mutate_settings(|settings| {
                let accounts = match provider {
                    ProviderId::Claude => &mut settings.accounts.claude,
                    ProviderId::Codex => &mut settings.accounts.codex,
                    _ => return Err("Unsupported provider".to_string()),
                };
                let profile = accounts
                    .find_mut(&id)
                    .ok_or_else(|| format!("Unknown account '{id}'"))?;
                profile.ownership = AccountOwnership::Managed;
                Ok(())
            })?;
            json!({"provider": provider.descriptor().key, "id": id, "state": "adopted"})
        }
        Plan::InsertSecond {
            desktop_dir,
            config_dir,
        } => {
            if desktop_dir.is_empty() && config_dir.is_empty() {
                return Err("Legacy second export has no usable directory".into());
            }
            state::mutate_settings(|settings| {
                let accounts = &mut settings.accounts.claude;
                // Stable id `second`: refused when already retired/used so a
                // new profile can never retarget old theme bindings.
                if accounts.used_ids.contains("second") || accounts.find("second").is_some() {
                    return Err(
                        "Id 'second' is already used or retired; import the export explicitly instead"
                            .to_string(),
                    );
                }
                for candidate in [&desktop_dir, &config_dir] {
                    if !candidate.is_empty() {
                        paths::require_absolute(Path::new(candidate))?;
                    }
                }
                accounts.profiles.push(AccountProfile {
                    id: "second".to_string(),
                    name: "Second".to_string(),
                    config_dir,
                    desktop_dir,
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                });
                accounts.used_ids.insert("second".to_string());
                Ok(())
            })?;
            json!({"provider": "claude", "id": "second", "state": "adopted"})
        }
        Plan::Refused { reason } => {
            return Err(reason);
        }
        Plan::Nothing => {
            return Err(
                "No Second source found: register it first with `--manage create` or `--manage import`"
                    .to_string(),
            );
        }
    };
    // Non-auth legacy preferences (poll/language/UI/provider/placement) land
    // before the stale-theme rewrite so the imported theme selection is already
    // in fork form.
    let preferences = import_legacy_preferences(fork_was_fresh)?;
    let active_theme = fix_stale_active_theme()?;
    // Non-destructive shared-skills import: existing real homes are copied
    // into the library with replace=false; conflicts reported, never applied.
    let skills_import = skills_import_nondestructive();
    // First-migration pin initialization: Default + adopted Second pinned
    // when no actual user pin prefs exist yet.
    let pins = ensure_default_pins()?;
    Ok(json!({
        "adopted": adopted,
        "default_binding": default_binding,
        "themes_copied": themes_copied,
        "themes_skipped": themes_skipped,
        "assets_copied": assets_copied,
        "active_theme": active_theme,
        "preferences": preferences,
        "skills_import": skills_import,
        "pins": pins,
    }))
}

/// Pin initialization for first migration: when a provider registry carries
/// no actual user pin yet, pin its enabled Default and Second accounts (and
/// nothing else). Pre-existing pins and all display metadata (names, order,
/// colors, labels) are preserved untouched.
fn ensure_default_pins() -> Result<Value, String> {
    let mut providers_json = Vec::new();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let outcome = state::mutate_settings(|settings| {
            let accounts = match provider {
                ProviderId::Claude => &mut settings.accounts.claude,
                ProviderId::Codex => &mut settings.accounts.codex,
                _ => return Err("Unsupported provider".to_string()),
            };
            if accounts.profiles.iter().any(|profile| profile.pinned) {
                return Ok(json!({"initialized": false, "reason": "existing pin prefs preserved"}));
            }
            let mut pinned = Vec::new();
            for profile in accounts.profiles.iter_mut() {
                if !profile.enabled {
                    continue;
                }
                let is_default = profile.id.eq_ignore_ascii_case("default");
                let is_second = profile.id.eq_ignore_ascii_case("second")
                    || profile.name.eq_ignore_ascii_case("second");
                if is_default || is_second {
                    profile.pinned = true;
                    pinned.push(profile.id.clone());
                }
            }
            Ok(json!({"initialized": true, "pinned": pinned}))
        })?;
        providers_json.push(json!({
            "provider": provider.descriptor().key,
            "pins": outcome,
        }));
    }
    Ok(Value::Array(providers_json))
}

/// Copies themes/assets from the single confirmed CONFIGURATION root only
/// (skip-existing, never overwrite). The binary-only install dir is never a
/// source, so it cannot shadow the real themed settings root.
fn copy_legacy_themes() -> Result<(u64, u64, u64), String> {
    let Some(legacy) = confirmed_legacy_dir() else {
        return Ok((0, 0, 0));
    };
    let themes_dir = crate::theme_engine::themes_directory();
    let assets_dir = crate::theme_engine::assets_directory();
    let mut copied = 0u64;
    let mut skipped = 0u64;
    let mut assets = 0u64;
    let legacy_themes = legacy.join("themes");
    if legacy_themes.is_dir() {
        paths::ensure_dir(&themes_dir)?;
        let Ok(entries) = std::fs::read_dir(&legacy_themes) else {
            return Ok((copied, skipped, assets));
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            let is_json = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
            if !is_json || !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let destination = themes_dir.join(name);
            if destination.exists() {
                skipped += 1;
                continue;
            }
            let bytes = std::fs::copy(&path, &destination)
                .map_err(|error| format!("Unable to copy theme '{}': {error}", path.display()))?;
            if destination.metadata().map(|meta| meta.len()).unwrap_or(0) != bytes {
                let _ = std::fs::remove_file(&destination);
                return Err(format!("Short copy of theme '{}'", path.display()));
            }
            copied += 1;
        }
    }
    let legacy_assets = legacy.join("assets");
    if legacy_assets.is_dir() {
        paths::ensure_dir(&assets_dir)?;
        let Ok(entries) = std::fs::read_dir(&legacy_assets) else {
            return Ok((copied, skipped, assets));
        };
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            if !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let destination = assets_dir.join(name);
            if destination.exists() {
                skipped += 1;
                continue;
            }
            std::fs::copy(&path, &destination)
                .map_err(|error| format!("Unable to copy asset '{}': {error}", path.display()))?;
            assets += 1;
        }
    }
    Ok((copied, skipped, assets))
}

/// Rewrite `active_theme_path` only when it still points into a legacy dir
/// and the same file name now exists in the fork themes dir.
fn fix_stale_active_theme() -> Result<Value, String> {
    let settings = state::load_fresh();
    let Some(active) = settings.active_theme_path.clone() else {
        return Ok(json!({"rewrote": false, "reason": "no active theme set"}));
    };
    let active_path = PathBuf::from(&active);
    let in_legacy = legacy_dir_candidates().into_iter().any(|directory| {
        legacy_root_kind(&directory).is_some() && paths::is_within(&directory, &active_path)
    });
    if !in_legacy {
        return Ok(json!({"rewrote": false, "reason": "active theme already in fork namespace"}));
    }
    let Some(name) = active_path.file_name() else {
        return Ok(json!({"rewrote": false, "reason": "active theme has no file name"}));
    };
    let fork_file = crate::theme_engine::themes_directory().join(name);
    if !fork_file.is_file() {
        return Ok(json!({"rewrote": false, "reason": "theme not imported; kept as-is"}));
    }
    let replacement = fork_file.display().to_string();
    state::mutate_settings(|settings| {
        settings.active_theme_path = Some(replacement.clone());
        Ok(())
    })?;
    Ok(json!({"rewrote": true, "active_theme_path": replacement}))
}

// ---------- cutover ----------

fn snapshot_root() -> Result<PathBuf, String> {
    let local = paths::fork_local_dir().ok_or("Unable to resolve the fork local root")?;
    Ok(local
        .join("migration")
        .join("snapshots")
        .join(now_unix().to_string()))
}

/// Final cutover. Order: snapshot task XML → disable+stop exact task (verified;
/// abort when it cannot be stopped or its state is unknown) → scoped encrypted
/// backup of ONLY the exact owned exports (merge-missing restore, so rollback
/// actually succeeds) → manifest + restore-based rollback → delete Run
/// values/launchers + DELETE plaintext exports (never moved or copied as
/// plaintext) + skills junctioning. Aborts before ANY destructive step when
/// the disable, the task state, or the encrypted backup is unknown/failed.
/// Never touches live profile directories. Rollback
/// restores deleted exports from the encrypted backup via `backup restore`,
/// never from staged plaintext.
pub fn cutover() -> Result<Value, String> {
    // Unknown task state (timeout/spawn) aborts BEFORE any snapshot, backup,
    // or deletion: a hung `schtasks` must never read as "no task" while the
    // legacy sync task may still be rewriting the exports.
    let exact_present = task_exists_exact(LEGACY_TASK_NAME).map_err(|unknown| {
        format!("Unable to determine legacy task '{LEGACY_TASK_NAME}' state ({unknown}); aborting cutover before any backup or deletion")
    })?;
    let run_values = run_key_values().unwrap_or_default();
    let legacy_run: Vec<(String, String)> = run_values
        .into_iter()
        .filter(|(name, data, _)| name == LEGACY_RUN_VALUE_NAME || contains_hint(data))
        .map(|(name, data, _)| (name, data))
        .collect();
    let startup_links = startup_legacy_links();
    // Exact owned exports with their own confirmed roots (path, root) pairs;
    // never filename-hint sweeps.
    let token_exports: Vec<(PathBuf, PathBuf)> = exact_token_exports();
    let skills_pending = skills_cutover_pending();
    if !exact_present
        && legacy_run.is_empty()
        && startup_links.is_empty()
        && token_exports.is_empty()
        && !skills_pending
    {
        return Ok(json!({
            "cutover": "noop",
            "reason": "no legacy task, autostart entries, token exports or pending skills homes found",
        }));
    }
    let snapshot = snapshot_root()?;
    for child in ["tasks", "startup", "skills"] {
        paths::ensure_dir(&snapshot.join(child))?;
    }
    // Initial rollback metadata BEFORE the task is touched: whatever fails
    // afterwards can point at this file.
    std::fs::write(
        snapshot.join("rollback-pending.json"),
        serde_json::to_vec_pretty(&json!({
            "task": LEGACY_TASK_NAME,
            "phase": "pre-disable",
            "created_unix": now_unix(),
            "re_enable": format!("schtasks /Change /TN \"{LEGACY_TASK_NAME}\" /ENABLE"),
        }))
        .unwrap_or_default(),
    )
    .map_err(|error| format!("Unable to write pending rollback metadata: {error}"))?;
    // 1. Snapshot the exact task definition (plaintext XML: command lines
    //    only, no secrets) for rollback.
    let mut manifest_tasks = Vec::new();
    if exact_present {
        let output = run_hidden("schtasks", &["/Query", "/TN", LEGACY_TASK_NAME, "/XML"]).map_err(
            |error| {
                format!(
                    "Unable to export legacy task: {}; aborting before any destructive step",
                    unknown_detail(&error)
                )
            },
        )?;
        if !output.status.success() {
            return Err(
                "Unable to export legacy task definition; aborting before any destructive step"
                    .into(),
            );
        }
        std::fs::write(
            snapshot.join("tasks").join("legacy-task.xml"),
            &output.stdout,
        )
        .map_err(|error| format!("Unable to write task snapshot: {error}"))?;
        manifest_tasks.push(json!({
            "name": LEGACY_TASK_NAME,
            "xml": "tasks/legacy-task.xml",
            "task_to_run": task_to_run(LEGACY_TASK_NAME).map_err(|unknown| {
                abort_after_disable(
                    &snapshot,
                    format!("Unable to corroborate legacy task before deletion: {unknown}"),
                )
            })?,
        }));
    }
    // 2. Disable + stop the exact legacy task FIRST (before any credential
    // snapshot, so the task cannot rewrite exports mid-backup). Verified via
    // re-query; abort when it cannot be stopped. Pre-disable state is
    // reversible and nothing has been deleted at this point. A timeout in
    // either the disable or the verification query is an unknown, never a
    // success: abort via the re-enabling path with the rollback pointer.
    let mut completed = Vec::new();
    let mut failures = Vec::new();
    if exact_present {
        // /End fails when the task is not running; that is fine (best-effort).
        let _ = run_hidden("schtasks", &["/End", "/TN", LEGACY_TASK_NAME]);
        let disabled = run_hidden(
            "schtasks",
            &["/Change", "/TN", LEGACY_TASK_NAME, "/DISABLE"],
        )
        .map(|output| output.status.success())
        .unwrap_or(false);
        let verified = task_status(LEGACY_TASK_NAME)
            .map_err(|unknown| {
                abort_after_disable(
                    &snapshot,
                    format!("Unable to verify legacy task is disabled ({unknown}); aborting before any backup or deletion"),
                )
            })?
            .is_some_and(|status| status.eq_ignore_ascii_case("disabled"));
        if !disabled || !verified {
            return Err(abort_after_disable(
                &snapshot,
                format!("Unable to disable+stop task '{LEGACY_TASK_NAME}' (nothing deleted yet). Aborting cutover before any backup or deletion."),
            ));
        }
        completed
            .push("disabled+stopped task 'usageBar Claude accounts sync' (verified)".to_string());
    }
    // 3. ONE encrypted targeted archive PER export, each with its own
    // confirmed root. Merge-missing restore recreates an absent export
    // without touching still-occupied settings/themes, so rollback actually
    // succeeds per file. Originals are deleted only once their archive
    // verifies. Any abort from here re-enables the task (nothing destructive
    // has run yet).
    let mut export_backups: Vec<(PathBuf, PathBuf, String)> = Vec::new();
    for (export, root) in &token_exports {
        let account_id = format!("legacy_{}", sanitize_id("usagebar"));
        let info = backup::import_legacy_paths(
            root,
            std::slice::from_ref(export),
            ProviderId::Claude,
            &account_id,
        )
        .map_err(|error| {
            abort_after_disable(
                &snapshot,
                format!(
                    "Encrypted legacy backup failed for '{}': {error}",
                    export.display()
                ),
            )
        })?;
        backup::verify_backup(&info.backup_id).map_err(|error| {
            abort_after_disable(
                &snapshot,
                format!(
                    "Encrypted legacy backup verify failed for '{}': {error}",
                    export.display()
                ),
            )
        })?;
        export_backups.push((export.clone(), root.clone(), info.backup_id));
    }
    // 2b. Shared-skills phase: non-destructive library import per home, then
    // an encrypted backup per real dir. Homes with conflicts, external
    // ownership, quoted paths, or failed backups are refused, never replaced.
    let skills_plan =
        skills_cutover_prepare().map_err(|error| abort_after_disable(&snapshot, error))?;
    let skills_restores: Vec<(PathBuf, Option<String>)> = skills_plan
        .iter()
        .filter(|work| work.action == "replace" || work.action == "create")
        .map(|work| {
            let staged = if work.action == "replace" {
                Some(format!("skills\\{}", work.tag))
            } else {
                None
            };
            (work.home.clone(), staged)
        })
        .collect();
    // 3b. Rollback script + manifest BEFORE destructive steps. Plaintext
    // exports are never staged: rollback restores each missing export from
    // its own encrypted archive via `backup restore`, then restores launchers
    // after data recovery.
    let export_restores: Vec<(PathBuf, String)> = export_backups
        .iter()
        .map(|(export, _, backup_id)| (export.clone(), backup_id.clone()))
        .collect();
    let rollback_cmd = rollback_script(
        &snapshot,
        exact_present,
        &legacy_run,
        &startup_links,
        &skills_restores,
        &export_restores,
    )
    .map_err(|error| abort_after_disable(&snapshot, error))?;
    let manifest = json!({
        "created_unix": now_unix(),
        "tasks": manifest_tasks,
        "run_values": legacy_run.iter().map(|(name, data)| json!({"name": name, "data": data})).collect::<Vec<_>>(),
        "startup_links": startup_links.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "token_exports": export_backups.iter().map(|(export, root, backup_id)| json!({
            "path": export.display().to_string(),
            "root": root.display().to_string(),
            "encrypted_backup_id": backup_id,
        })).collect::<Vec<_>>(),
        "legacy_backups": export_backups.iter().map(|(_, root, backup_id)| json!({
            "root": root.display().to_string(),
            "backup_id": backup_id,
        })).collect::<Vec<_>>(),
        "skills": skills_plan_json(&skills_plan),
        "rollback": rollback_cmd,
        "rollback_data_restores": export_restores.iter().map(|(export, backup_id)| {
            format!("if not exist \"{}\" usagehub.exe --manage backup restore --backup {backup_id} --confirm {backup_id}", export.display())
        }).collect::<Vec<_>>(),
    });
    std::fs::write(
        snapshot.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|error| {
        abort_after_disable(
            &snapshot,
            format!("Unable to write cutover manifest: {error}"),
        )
    })?;
    // 4. Destructive phase. Plaintext token exports are DELETED (never moved
    // or copied as plaintext) only after the verified encrypted backup above;
    // rollback restores them via `backup restore`, never from staged copies.
    for (name, _) in &legacy_run {
        match delete_run_value(name) {
            Ok(()) => completed.push(format!("removed Run value '{name}'")),
            Err(error) => failures.push(error),
        }
    }
    for link in &startup_links {
        let target = snapshot.join("startup").join(
            link.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("link.lnk"),
        );
        match move_file(link, &target) {
            Ok(()) => completed.push(format!("moved launcher '{}'", link.display())),
            Err(error) => failures.push(error),
        }
    }
    for (export, _, _) in &export_backups {
        match std::fs::remove_file(export) {
            Ok(()) => completed.push(format!(
                "deleted plaintext export '{}' (encrypted backup verified)",
                export.display()
            )),
            Err(error) => failures.push(format!(
                "Unable to delete plaintext export '{}': {error}",
                export.display()
            )),
        }
    }
    // 6. Shared-skills homes: junction `create`/`replace` plans; `already`
    // homes are verified shared, `refused` homes are reported, never touched.
    let mut skills_outcomes = Vec::new();
    for work in &skills_plan {
        if work.action != "create" && work.action != "replace" {
            skills_outcomes.push(json!({
                "kind": work.kind,
                "home": work.home.display().to_string(),
                "action": work.action,
                "reason": work.reason,
            }));
            continue;
        }
        match execute_skills_item(&snapshot, work) {
            Ok(detail) => {
                completed.push(format!(
                    "skills {} junctioned '{}'",
                    work.kind,
                    work.home.display()
                ));
                skills_outcomes.push(json!({
                    "kind": work.kind,
                    "home": work.home.display().to_string(),
                    "action": work.action,
                    "backup_id": work.backup_id,
                    "detail": detail,
                }));
            }
            Err(error) => failures.push(format!(
                "skills {} '{}': {error}",
                work.kind,
                work.home.display()
            )),
        }
    }
    if failures.is_empty() {
        Ok(json!({
            "cutover": "done",
            "snapshot": snapshot.display().to_string(),
            "rollback": rollback_cmd,
            "legacy_backups": export_backups.iter().map(|(export, root, backup_id)| json!({
                "path": export.display().to_string(),
                "root": root.display().to_string(),
                "backup_id": backup_id,
            })).collect::<Vec<_>>(),
            "skills": skills_outcomes,
            "completed": completed,
        }))
    } else {
        Err(format!(
            "Cutover partially failed; snapshot at '{}' holds rollback ({}). Done: {}. Failed: {}.",
            snapshot.display(),
            rollback_cmd,
            completed.join("; "),
            failures.join("; "),
        ))
    }
}

fn move_file(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Cross-volume fallback: copy, verify size, then delete the original.
    let bytes = std::fs::copy(from, to)
        .map_err(|error| format!("Unable to move '{}': {error}", from.display()))?;
    let actual = to
        .metadata()
        .map(|meta| meta.len())
        .map_err(|error| format!("Unable to verify '{}': {error}", to.display()))?;
    if actual != bytes {
        let _ = std::fs::remove_file(to);
        return Err(format!("Short copy moving '{}'", from.display()));
    }
    std::fs::remove_file(from)
        .map_err(|error| format!("Unable to remove original '{}': {error}", from.display()))?;
    Ok(())
}

/// Generates `rollback.cmd` + `rollback.reg` inside the snapshot. Restores the
/// task from its XML, the Run values from the .reg, launchers from staged
/// copies, and unjunctions skills homes (moving staged real dirs back).
/// Deleted plaintext exports are NEVER staged: rollback restores them from
/// the verified encrypted backup via `backup restore` (conflict refusal
/// included), then restores launchers after data recovery. Snapshot holds
/// metadata only (task XML, Run values, .lnk files, manifest); no secrets.
/// `skills` holds `(home, staged_relative)` for every created/replaced
/// junction: `rmdir` removes the junction inode only (never the library),
/// then a staged original is moved back.
fn rollback_script(
    snapshot: &Path,
    task: bool,
    run_values: &[(String, String)],
    startup_links: &[PathBuf],
    skills: &[(PathBuf, Option<String>)],
    export_restores: &[(PathBuf, String)],
) -> Result<String, String> {
    for (name, data) in run_values {
        if name.contains(['\r', '\n']) || data.contains(['\r', '\n']) {
            return Err(format!(
                "Refusing to script rollback for Run value '{name}' containing a newline"
            ));
        }
    }
    if task && LEGACY_TASK_NAME.contains('"') {
        return Err("Refusing to script rollback for quoted task name".into());
    }
    let mut reg = String::from("Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n");
    for (name, data) in run_values {
        reg.push_str(&format!(
            "\"{}\"=\"{}\"\r\n",
            name.replace('\\', "\\\\").replace('"', "\\\""),
            data.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    // REGEDIT5 files use UTF-16LE with a BOM so non-ASCII values survive
    // registry import independently of the system's active ANSI code page.
    let mut reg_bytes = vec![0xff, 0xfe];
    for unit in reg.encode_utf16() {
        reg_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(snapshot.join("rollback.reg"), reg_bytes)
        .map_err(|error| format!("Unable to write rollback.reg: {error}"))?;
    // Batch files are interpreted under the active console code page. Switch
    // to UTF-8 before any generated non-ASCII text, and prevent expansion of
    // literal percent signs or exclamation marks in generated arguments.
    let batch_arg = |value: &str| -> Result<String, String> {
        if value.contains(['"', '\r', '\n']) {
            return Err("Unable to safely quote a rollback command argument".into());
        }
        Ok(value.replace('%', "%%"))
    };
    let mut cmd = String::from("@echo off\r\nsetlocal DisableDelayedExpansion\r\nchcp 65001 >nul\r\nif errorlevel 1 exit /b 1\r\nrem UsageHub migration rollback: restore data before legacy launchers.\r\nset \"SNAP=%~dp0\"\r\n");
    if !export_restores.is_empty() {
        let executable = std::env::current_exe().map_err(|error| {
            format!("Unable to locate UsageHub executable for rollback: {error}")
        })?;
        let executable = executable
            .to_str()
            .ok_or("UsageHub executable path is not valid Unicode")?;
        let executable = batch_arg(executable)?;
        for (export, backup_id) in export_restores {
            let export = export.to_str().ok_or("Export path is not valid Unicode")?;
            let export = batch_arg(export)?;
            let backup_id = batch_arg(backup_id)?;
            cmd.push_str(&format!(
                "if not exist \"{export}\" (\r\n  \"{executable}\" --manage backup restore --backup \"{backup_id}\" --confirm \"{backup_id}\"\r\n  if errorlevel 1 goto restore_failed\r\n  if not exist \"{export}\" goto restore_failed\r\n)\r\n"
            ));
        }
    }
    for (home, staged) in skills {
        // Quoted skills paths are refused in the plan phase, so this script
        // never quotes them unsafely. `rmdir` on a junction removes the link
        // inode only, never the shared-library target.
        let home = home
            .to_str()
            .ok_or("Skills home path is not valid Unicode")?;
        let home = batch_arg(home)?;
        cmd.push_str(&format!(
            "rmdir \"{home}\"\r\nif errorlevel 1 goto skills_failed\r\n"
        ));
        if let Some(relative) = staged {
            let relative = batch_arg(relative)?;
            cmd.push_str(&format!(
                "move \"%SNAP%{relative}\" \"{home}\" >nul\r\nif errorlevel 1 goto skills_failed\r\n"
            ));
        }
    }
    if task {
        cmd.push_str(&format!(
            "schtasks /Create /TN \"{LEGACY_TASK_NAME}\" /XML \"%SNAP%tasks\\legacy-task.xml\" /F\r\nif errorlevel 1 goto launcher_failed\r\n"
        ));
    }
    if !run_values.is_empty() {
        cmd.push_str(
            "reg.exe import \"%SNAP%rollback.reg\"\r\nif errorlevel 1 goto launcher_failed\r\n",
        );
    }
    if !startup_links.is_empty() {
        cmd.push_str("copy /Y \"%SNAP%startup\\*.lnk\" \"%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\Startup\\\" >nul\r\nif errorlevel 1 goto launcher_failed\r\n");
    }
    cmd.push_str("echo Rollback complete.\r\nexit /b 0\r\n:restore_failed\r\necho DATA RESTORE FAILED - legacy launchers were not restored; some data may already have been restored. 1>&2\r\nexit /b 1\r\n:skills_failed\r\necho SKILLS RESTORE FAILED - legacy launchers were not restored; some data or skills may already have been restored. 1>&2\r\nexit /b 1\r\n:launcher_failed\r\necho LAUNCHER RESTORE FAILED - data and skills were restored, and some legacy launchers may already have been restored. 1>&2\r\nexit /b 1\r\n");
    let path = snapshot.join("rollback.cmd");
    std::fs::write(&path, cmd).map_err(|error| format!("Unable to write rollback.cmd: {error}"))?;
    Ok(path.display().to_string())
}

// ---------- explicit legacy backups ----------

/// ASCII alnum/underscore id fragment (fork path rules: no hyphens/spaces).
fn sanitize_id(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() {
            out.push(byte.to_ascii_lowercase() as char);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "backup".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

/// Explicit user-directed handling of one legacy plaintext directory:
/// `encrypt` (managed encrypted snapshot via backup, restorable to the
/// recorded legacy path only), `purge` (verified recursive delete after an
/// explicit `--confirm`), or `leave` (acknowledged, untouched).
pub fn legacy_backup(path: &Path, action: &str) -> Result<Value, String> {
    let absolute = paths::require_absolute(path)?;
    if !absolute.is_dir() {
        return Err(format!("Not a directory: '{}'", absolute.display()));
    }
    match action {
        "encrypt" => {
            recognized_legacy_backup_dir(&absolute)?;
            let stem = absolute
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("legacy");
            let account_id = format!("legacy_{}", sanitize_id(stem));
            let info = backup::import_legacy_backup(&absolute, ProviderId::Claude, &account_id)?;
            backup::verify_backup(&info.backup_id).map_err(|error| {
                format!("Encrypted legacy backup verify failed; plaintext left untouched: {error}")
            })?;
            // Approved encrypt semantics: the verified encrypted copy REPLACES
            // the plaintext generation (7-day encrypted undo). Partial purge
            // failure reports the retained backup id + remaining path.
            let (files, bytes) = purge_tree(&absolute).map_err(|error| {
                format!(
                    "{error} (verified encrypted backup {} retained; plaintext remains at '{}')",
                    info.backup_id,
                    absolute.display()
                )
            })?;
            Ok(json!({
                "action": "encrypt",
                "path": absolute.display().to_string(),
                "backup_id": info.backup_id,
                "account_id": info.account_id,
                "files_removed": files,
                "bytes_removed": bytes,
                "note": "plaintext generation replaced by the verified encrypted snapshot (7-day undo); restorable via backup restore to the recorded legacy directory only",
            }))
        }
        "purge" => {
            recognized_legacy_backup_dir(&absolute)?;
            let (files, bytes) = purge_tree(&absolute)?;
            Ok(json!({
                "action": "purge",
                "path": absolute.display().to_string(),
                "files_removed": files,
                "bytes_removed": bytes,
            }))
        }
        "leave" => Ok(json!({
            "action": "leave",
            "path": absolute.display().to_string(),
            "acknowledged": true,
            "note": "left untouched; re-run with encrypt or purge to change this",
        })),
        other => Err(format!("Unknown legacy-backup action '{other}'")),
    }
}

/// Recognition gate for the mutating legacy-backup actions. Only recognized
/// Claude Cleaner backup generations (`.claude_backup_*` basenames) may be
/// encrypted-replaced or purged. Broad roots (drive/home), active account
/// dirs, canonical provider homes, the shared library, and fork/managed roots
/// are refused in both directions (the target inside a protected root, or a
/// protected root inside the target). Any reparse point on the path or its
/// ancestors is refused. `leave` stays a read-only acknowledgement and does
/// not go through this gate.
fn recognized_legacy_backup_dir(path: &Path) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !name.starts_with(".claude_backup_") {
        return Err(format!(
            "Refusing legacy-backup on unrecognized '{}': mutating actions are limited to recognized Claude Cleaner generations (`.claude_backup_*`)",
            path.display()
        ));
    }
    if let Some(reparse) = paths::first_reparse_point(path) {
        return Err(format!(
            "Refusing legacy-backup with a reparse point at '{}' (target '{}')",
            reparse.display(),
            path.display()
        ));
    }
    let mut protected: Vec<Option<PathBuf>> = Vec::new();
    protected.extend([
        paths::fork_config_dir(),
        paths::fork_local_dir(),
        paths::profiles_root(),
    ]);
    protected.extend([
        crate::known_folders::home_dir(),
        crate::accounts::environment_directory(ProviderId::Claude),
        crate::accounts::environment_directory(ProviderId::Codex),
    ]);
    if let Some(home) = crate::known_folders::home_dir() {
        protected.push(Some(home.join(".claude")));
        protected.push(Some(home.join(".codex")));
    }
    if let Some(library) = super::skills::shared_skills_dir() {
        protected.push(Some(library));
    }
    protected.push(Some(PathBuf::from("C:\\")));
    for root in protected.into_iter().flatten() {
        if same_home(&root, path) || paths::is_within(&root, path) || paths::is_within(path, &root)
        {
            return Err(format!(
                "Refusing legacy-backup overlapping protected root '{}'",
                root.display()
            ));
        }
    }
    Ok(())
}

/// Recursive delete that refuses to traverse links: any symlink/junction
/// inside aborts the purge with an explicit list.
fn purge_tree(root: &Path) -> Result<(u64, u64), String> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    let mut links = Vec::new();
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
            let path = entry.path();
            let file_type = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("Unable to stat '{}': {error}", path.display()))?
                .file_type();
            if file_type.is_symlink() {
                links.push(path.display().to_string());
            } else if file_type.is_dir() {
                stack.push(path);
            } else {
                files += 1;
                bytes += path.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    if !links.is_empty() {
        return Err(format!(
            "Refusing purge: links inside '{}': {}",
            root.display(),
            links.join(", ")
        ));
    }
    std::fs::remove_dir_all(root)
        .map_err(|error| format!("Unable to purge '{}': {error}", root.display()))?;
    if root.exists() {
        return Err(format!(
            "Purge incomplete: '{}' still exists",
            root.display()
        ));
    }
    Ok((files, bytes))
}

// ---------- shared-skills adoption ----------
//
// Approved skills are shared, never per-account copies: the canonical Default
// home (`$CLAUDE_CONFIG_DIR/skills`, else `<home>/.claude/skills`) and the
// adopted Second skills dir both end up as protected junctions into the fork
// shared library. Existing real dirs are NEVER silently replaced: import
// copies their skills into the library non-destructively (`replace = false`),
// cutover junctions a home only after a conflict-free import plus a verified
// encrypted backup, and the moved original stays restorable through the
// cutover snapshot + rollback script.

/// One known skills home: the canonical Default location or the adopted
/// Second skills dir resolved from the registry.
struct SkillsHome {
    kind: &'static str,
    base: PathBuf,
    home: PathBuf,
    account_id: String,
    /// False for External accounts: reported only — never created, replaced,
    /// or junctioned (read-only). The one exception is the canonical CLI home
    /// of an explicitly canonical External Default (the migration binding),
    /// which is the approved `.claude/skills` backup+junction target.
    managed: bool,
}

fn same_home(left: &Path, right: &Path) -> bool {
    fn squash(path: &Path) -> String {
        path.display()
            .to_string()
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    squash(left) == squash(right)
}

fn canonical_default_base() -> Option<PathBuf> {
    crate::accounts::environment_directory(ProviderId::Claude)
        .or_else(|| crate::known_folders::home_dir().map(|home| home.join(".claude")))
}

fn push_skills_home(
    homes: &mut Vec<SkillsHome>,
    kind: &'static str,
    base: PathBuf,
    account_id: String,
    managed: bool,
) {
    let home = base.join("skills");
    if !homes
        .iter()
        .any(|known: &SkillsHome| same_home(&known.home, &home))
    {
        homes.push(SkillsHome {
            kind,
            base,
            home,
            account_id,
            managed,
        });
    }
}

/// Canonical Claude roots: CLI home, credential file, `%APPDATA%/Claude`,
/// MSIX caches (shared with the lifecycle gates) plus the physical
/// `Claude-Data/claude_*` Desktop layouts the migration binding records.
fn canonical_claude_roots() -> Vec<String> {
    let mut roots = lifecycle::canonical_default_roots(ProviderId::Claude);
    roots.extend(
        canonical_desktop_dirs()
            .iter()
            .map(|dir| native::canonical_spelling(dir)),
    );
    roots.sort();
    roots.dedup();
    roots
}

/// True when every explicit source field of the canonical External Default
/// names a canonical Claude root: the migration binding, or an equivalent
/// hand-entered canonical mapping. Only such a Default keeps the canonical
/// CLI skills home eligible for the approved backup+junction cutover; any
/// arbitrary mapped External root stays report-only.
fn default_mapping_is_canonical(profile: &AccountProfile) -> bool {
    if profile.ownership != AccountOwnership::External {
        return false;
    }
    let roots = canonical_claude_roots();
    let mut explicit = false;
    for field in [
        &profile.desktop_dir,
        &profile.config_dir,
        &profile.credentials_path,
    ] {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }
        explicit = true;
        let path = Path::new(trimmed);
        if !path.is_absolute() {
            return false;
        }
        let spelling = native::canonical_spelling(path);
        let inside = roots.iter().any(|root| {
            spelling == *root
                || spelling
                    .strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with('\\'))
        });
        if !inside {
            return false;
        }
    }
    explicit
}

/// Known skills homes derived from the registry: the Default profile and any
/// Second profile (exact id or name), each with BOTH its desktop and CLI
/// config bases (the CLI reads `<config>/skills`, the desktop `<desktop>/
/// skills`). Managed profiles junction at cutover. External bases are
/// report-only, with ONE approved exception: an explicitly canonical External
/// Default (the `import` binding to the real Desktop dir, or an equivalent
/// canonical mapping) keeps its separate canonical CLI home junction-eligible,
/// so the approved `.claude/skills` backup+junction cutover still works while
/// the external Desktop tree itself stays untouched. When the Default has no
/// explicit mapping (external default or no registry entry), fall back to the
/// canonical provider-default home so a config-less Default still shares the
/// approved library.
fn known_skills_homes() -> Vec<SkillsHome> {
    let mut homes = Vec::new();
    let settings = state::load_fresh();
    let mut default_mapped = false;
    if let Some(accounts) = settings.accounts.get(ProviderId::Claude) {
        if let Some(default) = accounts
            .profiles
            .iter()
            .find(|profile| profile.id.eq_ignore_ascii_case("default"))
        {
            let managed = default.ownership == AccountOwnership::Managed;
            let canonical = default_mapping_is_canonical(default);
            let mut bases: Vec<(&'static str, PathBuf)> = Vec::new();
            if !default.desktop_dir.trim().is_empty() {
                bases.push(("desktop", PathBuf::from(&default.desktop_dir)));
            }
            if !default.config_dir.trim().is_empty() {
                bases.push(("config", PathBuf::from(&default.config_dir)));
            }
            if bases.is_empty() {
                if let Some(base) = canonical_default_base() {
                    push_skills_home(&mut homes, "default", base, default.id.clone(), true);
                }
            } else {
                // The Desktop tree of an External Default is never junctioned;
                // only its canonical CLI home is, and only for the canonical
                // mapping (otherwise a customized External mapping stays
                // report-only).
                let cli_home_implicit = default.config_dir.trim().is_empty();
                for (field, base) in bases {
                    let junctionable = managed || (canonical && field == "config");
                    push_skills_home(
                        &mut homes,
                        "default",
                        base,
                        default.id.clone(),
                        junctionable,
                    );
                }
                // A canonical mapping that left the CLI home implicit still
                // keeps the canonical `.claude/skills` home in the approved
                // cutover set.
                if canonical && cli_home_implicit {
                    if let Some(base) = canonical_default_base() {
                        push_skills_home(&mut homes, "default", base, default.id.clone(), true);
                    }
                }
            }
            default_mapped = true;
        }
        for profile in accounts.profiles.iter().filter(|profile| {
            profile.id.eq_ignore_ascii_case("second") || profile.name.eq_ignore_ascii_case("second")
        }) {
            let managed = profile.ownership == AccountOwnership::Managed;
            if !profile.desktop_dir.trim().is_empty() {
                push_skills_home(
                    &mut homes,
                    "second",
                    PathBuf::from(&profile.desktop_dir),
                    profile.id.clone(),
                    managed,
                );
            }
            if !profile.config_dir.trim().is_empty() {
                push_skills_home(
                    &mut homes,
                    "second",
                    PathBuf::from(&profile.config_dir),
                    profile.id.clone(),
                    managed,
                );
            }
        }
    }
    if !default_mapped {
        if let Some(base) = canonical_default_base() {
            push_skills_home(&mut homes, "default", base, "default".to_string(), true);
        }
    }
    homes
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HomeState {
    Missing,
    Link,
    RealDir,
}

fn home_state(home: &Path) -> HomeState {
    match std::fs::symlink_metadata(home) {
        Err(_) => HomeState::Missing,
        Ok(meta) if meta.file_type().is_symlink() => HomeState::Link,
        Ok(_) => {
            if paths::first_reparse_point(home).as_deref() == Some(home) {
                HomeState::Link
            } else {
                HomeState::RealDir
            }
        }
    }
}

fn top_level_count(home: &Path) -> u64 {
    std::fs::read_dir(home)
        .map(|entries| entries.filter_map(Result::ok).count() as u64)
        .unwrap_or(0)
}

fn dir_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Read-only skills preview for `migration preview`. Ensures the (possibly
/// empty) shared library exists so conflicts can be computed; copies nothing.
fn skills_preview_json() -> Value {
    use super::skills;
    let library = skills::shared_skills_dir().filter(|dir| dir.is_dir());
    // Computing entry previews below ensures the empty library dir; that is
    // the only filesystem effect of this read-only scan.
    let library_skills: Vec<String> = library
        .as_ref()
        .map(|dir| dir_names(dir))
        .unwrap_or_default();
    let mut homes_json = Vec::new();
    let mut conflicts_total = 0u64;
    for home in known_skills_homes() {
        let quoted = home.home.display().to_string().contains('"');
        let (exists, is_link, link_valid, skill_count, conflicts, preview_error) =
            match home_state(&home.home) {
                HomeState::Missing => (false, false, Value::Null, 0, Vec::new(), None),
                HomeState::Link => {
                    let valid = library.as_ref().is_some_and(|dir| {
                        skills::validate_skill_link(&home.home, std::slice::from_ref(dir)).is_ok()
                    });
                    (true, true, Value::Bool(valid), 0, Vec::new(), None)
                }
                HomeState::RealDir => match skills::preview_skills_import(&home.home) {
                    Ok(entries) => {
                        let conflicts: Vec<String> = entries
                            .iter()
                            .filter(|entry| entry.action == skills::SkillImportAction::Conflict)
                            .map(|entry| entry.skill_name.clone())
                            .collect();
                        // Report-only homes never gate the cutover.
                        if home.managed {
                            conflicts_total += conflicts.len() as u64;
                        }
                        (
                            true,
                            false,
                            Value::Null,
                            top_level_count(&home.home),
                            conflicts,
                            None,
                        )
                    }
                    Err(error) => (
                        true,
                        false,
                        Value::Null,
                        top_level_count(&home.home),
                        Vec::new(),
                        Some(error),
                    ),
                },
            };
        homes_json.push(json!({
            "kind": home.kind,
            "home": home.home.display().to_string(),
            "account_id": home.account_id,
            "managed": home.managed,
            "quoted_path": quoted,
            "exists": exists,
            "is_link": is_link,
            "link_valid": link_valid,
            "skill_count": skill_count,
            "conflicts": conflicts,
            "preview_error": preview_error,
        }));
    }
    json!({
        "library": library.as_ref().map(|dir| dir.display().to_string()),
        "library_skills": library_skills,
        "homes": homes_json,
        "skills_conflicts": conflicts_total,
    })
}

/// Non-destructive library import for `migration import`: every existing real
/// home is copied into the shared library with `replace = false`. Conflicts
/// are reported, never overwritten. This helper never fails the adoption: it
/// records per-home outcomes (including errors) and reports `ok: false` when
/// any home failed.
fn skills_import_nondestructive() -> Value {
    use super::skills;
    let mut homes_json = Vec::new();
    let mut ok = true;
    for home in known_skills_homes() {
        let path = home.home.display().to_string();
        if !matches!(home_state(&home.home), HomeState::RealDir) {
            homes_json.push(json!({
                "kind": home.kind, "home": path,
                "action": "skipped", "reason": "no real skills dir to import",
            }));
            continue;
        }
        match skills::import_skills_from(&home.home, false) {
            Ok(report) => homes_json.push(json!({
                "kind": home.kind, "home": path, "action": "imported",
                "imported": report.imported,
                "skipped_unchanged": report.skipped_unchanged,
                "skipped_conflicts": report.skipped_conflicts,
            })),
            Err(error) => {
                ok = false;
                homes_json.push(json!({
                    "kind": home.kind, "home": path, "action": "failed", "error": error,
                }));
            }
        }
    }
    json!({"homes": homes_json, "ok": ok})
}

/// Planned cutover step for one skills home.
struct SkillsWork {
    kind: &'static str,
    home: PathBuf,
    base: PathBuf,
    #[allow(dead_code)]
    account_id: String,
    /// `create` (missing home), `replace` (staged real dir), `already`
    /// (validated junction), `refused` (conflict/external/quote/backup — see
    /// `reason`; never junctioned).
    action: String,
    reason: Option<String>,
    backup_id: Option<String>,
    tag: String,
    /// Pre-move folder metadata captured in the manifest for rollback.
    files: u64,
    bytes: u64,
}

/// Stable per-home staging slot: semantic desktop/config suffix, unique per
/// (kind, base shape). Deterministic across runs for identical input.
fn skills_tag(kind: &str, base: &Path) -> String {
    let slot = base
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("claude-config"));
    format!("{}_{}", kind, if slot { "config" } else { "desktop" })
}

fn skills_work(kind: &'static str, home: &SkillsHome, action: &str) -> SkillsWork {
    let (files, bytes) = count_tree(&home.home).unwrap_or((0, 0));
    SkillsWork {
        kind,
        home: home.home.clone(),
        base: home.base.clone(),
        account_id: home.account_id.clone(),
        action: action.to_string(),
        reason: None,
        backup_id: None,
        tag: skills_tag(kind, &home.base),
        files,
        bytes,
    }
}

/// True when any junction-eligible home still needs cutover work (missing
/// canonical home, real dir, or invalid link). External homes are report-only
/// and never create cutover work. Cheap stats only; no imports or mutations.
fn skills_cutover_pending() -> bool {
    use super::skills;
    let library = skills::shared_skills_dir();
    for home in known_skills_homes().into_iter().filter(|home| home.managed) {
        match home_state(&home.home) {
            HomeState::Missing => return true,
            HomeState::RealDir => return true,
            HomeState::Link => {
                let valid = library.as_ref().is_some_and(|dir| {
                    skills::validate_skill_link(&home.home, std::slice::from_ref(dir)).is_ok()
                });
                if !valid {
                    return true;
                }
            }
        }
    }
    false
}

/// Import + encrypted-backup phase (runs before the manifest). Per-home
/// refusals (conflicts, external ownership, quoted paths, import/backup
/// failures) are recorded, never junctioned.
fn skills_cutover_prepare() -> Result<Vec<SkillsWork>, String> {
    use super::skills;
    let library = skills::shared_skills_dir().ok_or("Unable to resolve the skills library")?;
    let mut plan = Vec::new();
    for home in known_skills_homes() {
        if home.home.display().to_string().contains('"') {
            let mut work = skills_work(home.kind, &home, "refused");
            work.reason = Some("quoted path cannot be scripted safely".to_string());
            plan.push(work);
            continue;
        }
        match home_state(&home.home) {
            HomeState::Missing => {
                if home.managed {
                    plan.push(skills_work(home.kind, &home, "create"));
                } else {
                    // External roots are report-only: never create a junction
                    // inside another application's data tree.
                    let mut work = skills_work(home.kind, &home, "refused");
                    work.reason = Some("external account is read-only; adopt it first".to_string());
                    plan.push(work);
                }
            }
            HomeState::Link => {
                match skills::validate_skill_link(&home.home, std::slice::from_ref(&library)) {
                    Ok(_) => plan.push(skills_work(home.kind, &home, "already")),
                    Err(_) => {
                        let mut work = skills_work(home.kind, &home, "refused");
                        work.reason = Some(
                            "link points outside the shared library; fix it manually".to_string(),
                        );
                        plan.push(work);
                    }
                }
            }
            HomeState::RealDir => {
                if !home.managed {
                    let mut work = skills_work(home.kind, &home, "refused");
                    work.reason = Some("external account is read-only; adopt it first".to_string());
                    plan.push(work);
                    continue;
                }
                let report = match skills::import_skills_from(&home.home, false) {
                    Ok(report) => report,
                    Err(error) => {
                        let mut work = skills_work(home.kind, &home, "refused");
                        work.reason = Some(format!("import failed: {error}"));
                        plan.push(work);
                        continue;
                    }
                };
                if !report.skipped_conflicts.is_empty() {
                    let mut work = skills_work(home.kind, &home, "refused");
                    work.reason = Some(format!(
                        "unresolved conflicts ({}); resolve manually and re-run",
                        report.skipped_conflicts.join(", ")
                    ));
                    plan.push(work);
                    continue;
                }
                let info = match backup::import_legacy_backup(
                    &home.home,
                    ProviderId::Claude,
                    &home.account_id,
                ) {
                    Ok(info) => info,
                    Err(error) => {
                        let mut work = skills_work(home.kind, &home, "refused");
                        work.reason = Some(format!("encrypted backup failed: {error}"));
                        plan.push(work);
                        continue;
                    }
                };
                if let Err(error) = backup::verify_backup(&info.backup_id) {
                    let mut work = skills_work(home.kind, &home, "refused");
                    work.reason = Some(format!("backup verify failed: {error}"));
                    plan.push(work);
                    continue;
                }
                let mut work = skills_work(home.kind, &home, "replace");
                work.backup_id = Some(info.backup_id);
                plan.push(work);
            }
        }
    }
    // Unique staging slots even when two homes share a kind+slot shape
    // (e.g. a custom config dir beside a desktop base): deterministic suffix.
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for work in &mut plan {
        let count = seen.entry(work.tag.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            work.tag = format!("{}_{}", work.tag, count);
        }
    }
    Ok(plan)
}

fn skills_plan_json(plan: &[SkillsWork]) -> Value {
    Value::Array(
        plan.iter()
            .map(|work| {
                json!({
                    "kind": work.kind,
                    "home": work.home.display().to_string(),
                    "action": work.action,
                    "reason": work.reason,
                    "backup_id": work.backup_id,
                    "files": work.files,
                    "bytes": work.bytes,
                    "staged": if work.action == "replace" {
                        Some(format!("skills\\{}", work.tag))
                    } else {
                        None
                    },
                })
            })
            .collect(),
    )
}

/// Executes one planned `create`/`replace` step. `replace` moves the real dir
/// into the snapshot first and junctions only afterwards; on junction failure
/// the dir is moved back so no half state remains.
fn execute_skills_item(snapshot: &Path, work: &SkillsWork) -> Result<Value, String> {
    use super::skills;
    let library = skills::shared_skills_dir().ok_or("Unable to resolve the skills library")?;
    match work.action.as_str() {
        "create" => {
            skills::ensure_account_skills_junction(&work.base)?;
            skills::validate_skill_link(&work.home, std::slice::from_ref(&library)).map_err(
                |error| {
                    format!(
                        "Junction '{}' failed validation: {error}",
                        work.home.display()
                    )
                },
            )?;
            Ok(json!({"junction": work.home.display().to_string()}))
        }
        "replace" => {
            let staged = snapshot.join("skills").join(&work.tag);
            if staged.exists() {
                return Err(format!(
                    "Snapshot skills slot '{}' is occupied",
                    staged.display()
                ));
            }
            let (files, bytes) = move_dir_all(&work.home, &staged)?;
            if let Err(error) = skills::ensure_account_skills_junction(&work.base) {
                let back = move_dir_all(&staged, &work.home);
                return Err(format!(
                    "Junction failed, rolled the dir back ({back:?}): {error}"
                ));
            }
            if let Err(error) =
                skills::validate_skill_link(&work.home, std::slice::from_ref(&library))
            {
                let _ = skills::remove_link_only(&work.home);
                let _ = move_dir_all(&staged, &work.home);
                return Err(format!("Junction failed validation: {error}"));
            }
            Ok(json!({
                "moved_to": staged.display().to_string(),
                "files": files,
                "bytes": bytes,
            }))
        }
        other => Err(format!("Skills item '{other}' is not executable")),
    }
}

/// Move a directory without following links: `rename`, else recursive copy
/// (links refused) + fingerprint compare + source removal.
fn move_dir_all(from: &Path, to: &Path) -> Result<(u64, u64), String> {
    if to.exists() {
        return Err(format!(
            "Refusing to stage into existing '{}'",
            to.display()
        ));
    }
    if let Some(parent) = to.parent() {
        paths::ensure_dir(parent)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return count_tree(to);
    }
    copy_tree_nolinks(from, to)?;
    if tree_fingerprint(from)? != tree_fingerprint(to)? {
        let _ = std::fs::remove_dir_all(to);
        return Err(format!(
            "Copy verification failed moving '{}'",
            from.display()
        ));
    }
    std::fs::remove_dir_all(from).map_err(|error| {
        format!(
            "Copied but unable to remove source '{}': {error}",
            from.display()
        )
    })?;
    count_tree(to)
}

fn copy_tree_nolinks(from: &Path, to: &Path) -> Result<(), String> {
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((source, destination)) = stack.pop() {
        std::fs::create_dir_all(&destination)
            .map_err(|error| format!("Unable to create '{}': {error}", destination.display()))?;
        let entries = std::fs::read_dir(&source)
            .map_err(|error| format!("Unable to read '{}': {error}", source.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("Unable to read '{}': {error}", source.display()))?;
            let source_path = entry.path();
            let file_type = std::fs::symlink_metadata(&source_path)
                .map_err(|error| format!("Unable to stat '{}': {error}", source_path.display()))?
                .file_type();
            if file_type.is_symlink() {
                return Err(format!("Refusing to move link '{}'", source_path.display()));
            }
            let destination_path = destination.join(entry.file_name());
            if file_type.is_dir() {
                stack.push((source_path, destination_path));
            } else if file_type.is_file() {
                std::fs::copy(&source_path, &destination_path).map_err(|error| {
                    format!("Unable to copy '{}': {error}", source_path.display())
                })?;
            }
        }
    }
    Ok(())
}

fn tree_fingerprint(root: &Path) -> Result<Vec<(String, u64, bool)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
            let path = entry.path();
            let file_type = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("Unable to stat '{}': {error}", path.display()))?
                .file_type();
            if file_type.is_symlink() {
                return Err(format!("Refusing to fingerprint link '{}'", path.display()));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("Unable to relativize '{}': {error}", path.display()))?
                .display()
                .to_string();
            if file_type.is_dir() {
                out.push((relative, 0, true));
                stack.push(path);
            } else {
                out.push((
                    relative,
                    path.metadata().map(|meta| meta.len()).unwrap_or(0),
                    false,
                ));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn count_tree(root: &Path) -> Result<(u64, u64), String> {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("Unable to read '{}': {error}", directory.display()))?;
            let path = entry.path();
            let file_type = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("Unable to stat '{}': {error}", path.display()))?
                .file_type();
            if file_type.is_dir() && !file_type.is_symlink() {
                stack.push(path);
            } else if file_type.is_file() {
                files += 1;
                bytes += path.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schtasks_first_field_parses_quoted_names() {
        assert_eq!(
            csv_first_field("\"\\usageBar Claude accounts sync\",\"Ready\""),
            Some("\\usageBar Claude accounts sync".to_string())
        );
        assert_eq!(csv_first_field("not-quoted"), None);
        assert_eq!(csv_first_field("\"unterminated"), None);
    }

    #[test]
    fn legacy_hints_match_observed_entries() {
        assert!(contains_hint(
            "D:\\Soft\\usageBar\\claude-code-usage-monitor.exe --diagnose"
        ));
        assert!(contains_hint("usageBar Claude accounts sync"));
        assert!(contains_hint(
            "D:\\Soft\\python.exe D:\\Soft\\usageBar\\sync-claude-accounts.py"
        ));
        assert!(!contains_hint("Claude"));
        // The observed CONFIGURATION namespace is a known candidate...
        assert!(LEGACY_DIR_NAMES.contains(&"ClaudeCodeUsageMonitor"));
        // ...and classification follows content, never the directory name, so
        // the binary-only install dir can never be confirmed as the root.
        let root = tempfile::tempdir().expect("temp dir");
        let config = root.path().join("ClaudeCodeUsageMonitor");
        let binary = root.path().join("usageBar");
        std::fs::create_dir_all(config.join("themes")).unwrap();
        std::fs::write(config.join("settings.json"), "{}").unwrap();
        std::fs::create_dir_all(&binary).unwrap();
        std::fs::write(binary.join("claude-code-usage-monitor.exe"), "").unwrap();
        assert_eq!(legacy_root_kind(&config), Some(LegacyRootKind::Config));
        assert_eq!(legacy_root_kind(&binary), Some(LegacyRootKind::Binary));
        assert_eq!(legacy_root_kind(&root.path().join("absent")), None);
        assert_eq!(
            confirmed_root([binary.clone(), config.clone()]),
            Some(config)
        );
        assert_eq!(confirmed_root([binary]), None);
    }

    #[test]
    fn legacy_preferences_import_skips_accounts_and_credentials() {
        let themes = crate::theme_engine::themes_directory();
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("left-dock-usage.json"), "{}").unwrap();
        let legacy = serde_json::json!({
            "accounts": {
                "claude": {
                    "profiles": [{
                        "config_dir": "C:/Users/x/AppData/Roaming/ClaudeCodeUsageMonitor/profiles/second",
                        "credentials_path": "C:/secret/.credentials.json",
                        "enabled": true,
                        "id": "second",
                        "name": "Second"
                    }],
                    "selected": "second",
                    "used_ids": ["default", "second"]
                }
            },
            "credentials_path": "C:/secret/.credentials.json",
            "active_theme_path": "C:/Users/x/AppData/Roaming/ClaudeCodeUsageMonitor/themes/left-dock-usage.json",
            "last_update_check_unix": 1792954658,
            "language": "en",
            "poll_interval_ms": 300000,
            "usage_countdown": true,
            "lock_taskbar": true,
            "dashboard_width": 1100.0,
            "dashboard_height": 600.0,
            "floating_card_opacity": 100,
            "show_codex": true,
            "show_antigravity": true,
            "tray_offset": 12,
            "taskbar_index": 3,
            "widget_visible": false,
        });
        let mut settings = SettingsFile::default();
        let original_accounts = settings.accounts.clone();
        let keys = apply_legacy_preferences(&legacy, &mut settings);
        assert_eq!(settings.poll_interval_ms, 300_000);
        assert_eq!(settings.language.as_deref(), Some("en"));
        assert!(settings.usage_countdown && settings.lock_taskbar);
        assert_eq!(settings.dashboard_width, Some(1100.0));
        assert_eq!(settings.dashboard_height, Some(600.0));
        assert_eq!(settings.floating_card_opacity, Some(100));
        assert!(settings.provider_enabled(ProviderId::Codex));
        assert!(settings.provider_enabled(ProviderId::Antigravity));
        assert!(settings.legacy_placement_pending);
        assert_eq!((settings.tray_offset, settings.taskbar_index), (12, 3));
        assert!(settings.legacy_visibility_pending && !settings.widget_visible);
        // Theme SELECTION lands on the fork copy, never the legacy absolute path.
        let fork_theme = themes.join("left-dock-usage.json").display().to_string();
        assert_eq!(
            settings.active_theme_path.as_deref(),
            Some(fork_theme.as_str())
        );
        // Accounts, credentials and the update stamp never cross over.
        assert_eq!(settings.accounts, original_accounts);
        assert!(settings.last_update_check_unix.is_none());
        assert!(!keys
            .iter()
            .any(|key| key.contains("account") || key.contains("credential")));
        // Unknown language codes are ignored instead of poisoning the fork.
        let mut other = SettingsFile::default();
        apply_legacy_preferences(&serde_json::json!({"language": "xx"}), &mut other);
        assert!(other.language.is_none());
    }

    #[test]
    fn legacy_ids_are_path_valid() {
        assert_eq!(sanitize_id("usageBar"), "usagebar");
        assert_eq!(sanitize_id("Second Export (2)"), "second_export_2");
        assert_eq!(sanitize_id("---"), "backup");
        for id in [sanitize_id("usageBar"), sanitize_id("a/b c")] {
            assert!(id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            assert!(!id.contains('-'));
        }
    }

    #[test]
    fn export_paths_come_only_from_settings_or_script() {
        let value = serde_json::json!({
            "profiles": {"second": "D:\\Soft\\usageBar\\profiles\\second\\creds.json"},
            "nested": ["C:\\x\\y.JSON", 1, "profiles\\relative.json"],
        });
        let mut paths = json_export_paths(&value);
        paths.sort();
        assert_eq!(
            paths,
            [
                "C:\\x\\y.JSON",
                "D:\\Soft\\usageBar\\profiles\\second\\creds.json"
            ]
        );
        let script = "EXPORT = \"D:\\\\Soft\\\\usageBar\\\\sync-out.json\"\nOTHER='C:/data/tokens.json',\nignore = \"relative/x.json\"\n";
        let mut script_paths = script_export_paths(script);
        script_paths.sort();
        assert_eq!(
            script_paths,
            [
                "C:/data/tokens.json",
                "D:\\\\Soft\\\\usageBar\\\\sync-out.json"
            ]
        );
    }

    #[test]
    fn fork_paths_are_never_legacy_backups() {
        // Fixture roots (cfg(test)) stand in for the real fork dirs; no FS touch.
        let local = paths::fork_local_dir().expect("fixture local root");
        let inside = local.join("backups").join("claude");
        assert!(paths::is_within(&local, &inside));
        assert!(legacy_backup(&inside, "leave").is_err());
        assert!(legacy_backup(&local, "leave").is_err());
    }

    #[test]
    fn legacy_mutations_require_recognized_generations() {
        let root = tempfile::tempdir().expect("temp dir");
        // Unrecognized names are refused for mutating actions, untouched.
        let other = root.path().join("random-backup");
        std::fs::create_dir_all(&other).unwrap();
        assert!(legacy_backup(&other, "purge").is_err());
        assert!(legacy_backup(&other, "encrypt").is_err());
        assert!(other.is_dir());
        // ...but leave only acknowledges.
        assert!(legacy_backup(&other, "leave").is_ok());
        // Recognized empty generation purges cleanly.
        let generation = root.path().join(".claude_backup_2026");
        std::fs::create_dir_all(&generation).unwrap();
        let value = legacy_backup(&generation, "purge").expect("recognized purge");
        assert_eq!(value["files_removed"], 0);
        assert!(!generation.exists());
        // Recognized name inside a protected root is still refused.
        let local = paths::fork_local_dir().expect("fixture local root");
        assert!(legacy_backup(&local.join(".claude_backup_2026"), "purge").is_err());
    }

    #[test]
    fn home_paths_compare_slash_and_case_insensitively() {
        assert!(same_home(
            Path::new("C:/Users/x/.claude/skills"),
            Path::new("c:\\users\\X\\.claude\\skills"),
        ));
        assert!(!same_home(
            Path::new("C:/Users/x/.claude/skills"),
            Path::new("C:/Users/x/.claude/other"),
        ));
    }

    #[test]
    fn dir_moves_verify_and_refuse_occupied_slots_and_links() {
        let root = tempfile::tempdir().expect("temp dir");
        let source = root.path().join("skills");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("a.md"), "# a").unwrap();
        std::fs::write(source.join("nested").join("b.md"), "# b").unwrap();
        // Occupied destination is refused, source untouched.
        let occupied = root.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        assert!(move_dir_all(&source, &occupied).is_err());
        assert!(source.is_dir());
        // Clean move preserves the tree fingerprint.
        let before = tree_fingerprint(&source).expect("fingerprint");
        let staged = root.path().join("staged").join("default");
        let (files, bytes) = move_dir_all(&source, &staged).expect("move");
        assert_eq!((files, bytes), (2, 6));
        assert_eq!(tree_fingerprint(&staged).expect("staged"), before);
        assert!(!source.exists());
        // Links inside a moved tree are refused by the verifying copy path
        // (fingerprint/copy walk), never followed.
        let linked = root.path().join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join("real.md"), "# r").unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(linked.join("real.md"), linked.join("alias.md"))
            .is_ok()
        {
            assert!(tree_fingerprint(&linked).is_err());
            assert!(copy_tree_nolinks(&linked, &root.path().join("out")).is_err());
            assert!(!root.path().join("out").join("alias.md").exists());
            assert!(linked.join("real.md").is_file());
        }
    }

    #[test]
    fn credential_exports_require_shape_and_second_scope() {
        // Owned bridge: credential shape inside a `second` scope.
        assert!(is_credential_export(Path::new(
            "C:/Users/x/AppData/Roaming/ClaudeCodeUsageMonitor/profiles/second/.credentials.json"
        )));
        assert!(is_credential_export(Path::new(
            "D:/Soft/usageBar/profiles/second/credentials-backup.json"
        )));
        // Wrong shape: themes, settings, and non-credential files never match.
        assert!(!is_credential_export(Path::new(
            "D:/Soft/usageBar/themes/classic-usage-widget.json"
        )));
        assert!(!is_credential_export(Path::new(
            "D:/Soft/usageBar/profiles/second/settings.json"
        )));
        assert!(!is_credential_export(Path::new(
            "D:/Soft/usageBar/profiles/second/notes.md"
        )));
        // Right shape but outside Second scope or inside banned dirs.
        assert!(!is_credential_export(Path::new(
            "D:/Soft/usageBar/credentials.json"
        )));
        assert!(!is_credential_export(Path::new("C:/Users/x/.claude.json")));
        assert!(!is_credential_export(Path::new(
            "D:/Soft/usageBar/profiles/second/claude-config/credentials.json"
        )));
        assert!(!is_credential_export(Path::new(
            "C:/tools/.codex/second/credentials.json"
        )));
    }

    #[test]
    fn skills_tags_are_stable_and_unique_per_base_shape() {
        assert_eq!(
            skills_tag("second", Path::new("D:/x/Second")),
            "second_desktop"
        );
        assert_eq!(
            skills_tag("second", Path::new("D:/x/Second/claude-config")),
            "second_config"
        );
        assert_eq!(
            skills_tag("default", Path::new("C:/Users/x/.claude")),
            "default_desktop"
        );
    }

    #[test]
    fn task_presence_bit_needs_finished_success_and_name() {
        // Present only when the probe finished AND the name is in stdout.
        assert!(probe_present(
            true,
            "\"\\usageBar Claude accounts sync\",\"Ready\"",
            LEGACY_TASK_NAME
        ));
        // Non-zero exit (query ran, task absent) is absent even with output.
        assert!(!probe_present(
            false,
            "\"\\usageBar Claude accounts sync\",\"Ready\"",
            LEGACY_TASK_NAME
        ));
        assert!(!probe_present(
            true,
            "\"\\Other Task\",\"Ready\"",
            LEGACY_TASK_NAME
        ));
        assert!(!probe_present(true, "", LEGACY_TASK_NAME));
        // Timeouts/spawn failures never reach this bit: they surface as the
        // `Err` (unknown) arm of `task_exists_exact`, never as `false`.
    }

    #[test]
    fn task_status_cell_matches_exact_row_only() {
        let stdout = "\"\\usageBar Claude accounts sync\",\"Ready\"\n\"\\Other\",\"Disabled\"";
        assert_eq!(
            status_cell_for(stdout, LEGACY_TASK_NAME).as_deref(),
            Some("Ready")
        );
        assert_eq!(
            status_cell_for("\"\\Other\",\"Disabled\"", LEGACY_TASK_NAME),
            None
        );
        // Backslash-tolerant, case-insensitive exact match.
        assert_eq!(
            status_cell_for(stdout, "usagebar claude accounts sync").as_deref(),
            Some("Ready")
        );
        // No row at all: absent (Ok(None) at the caller), not unknown.
        assert_eq!(status_cell_for("", LEGACY_TASK_NAME), None);
    }
}
