//! Native Windows integration for managed accounts: official-app discovery,
//! per-account launches with explicit child environments, PID/window mapping,
//! graceful/force stop, and taskbar identities (shortcuts + AppUserModelID).
//!
//! Rules enforced here:
//! - One native EXE per provider (the officially installed app); UsageHub
//!   never redistributes or embeds it, it only launches it.
//! - Child processes get an explicit environment (`CLAUDE_CONFIG_DIR` /
//!   `CODEX_HOME`, `TEMP`/`TMP`, nonessential-traffic opt-outs). The parent
//!   process environment is never mutated.
//! - Stop/focus/status attribute managed processes by account marker and
//!   canonical defaults by installed image and default data roots; never kill globally.
//! - A hidden window (tray-minimized, `X`-to-tray) is reported as hidden, not
//!   as stopped. `running` is derived from live PIDs, never from visibility.
//! - Process attribution runs through [`live_process_snapshot`]: ONE native
//!   Toolhelp snapshot shared by every account, with command lines filled
//!   from an in-memory `(pid, creation-time)` cache. A single batched
//!   `Get-CimInstance` query (a Windows built-in utility) runs only for cache
//!   misses — never per account and never per timer tick in steady state.
//!   There is no stable documented Win32 API for another process's command
//!   line (`NtQueryInformationProcess` is undocumented), so the shell-out is
//!   the fallback only. Any command-line failure degrades to an empty
//!   attribution set, never to guessing another account's processes.

use std::collections::{BTreeMap, BTreeSet};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows::core::{Interface as _, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HWND, LPARAM, PROPERTYKEY, RECT, WPARAM,
};
use windows::Win32::Storage::EnhancedStorage::{
    PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand,
    PKEY_AppUserModel_RelaunchDisplayNameResource, PKEY_AppUserModel_RelaunchIconResource,
};
use windows::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::GetModuleFileNameExW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_READ,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};
use windows::Win32::System::Variant::{VT_EMPTY, VT_LPWSTR};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IPropertyStore, SHGetPropertyStoreForWindow, SHGetPropertyStoreFromParsingName, GPS_READWRITE,
};
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, IApplicationActivationManager, IShellLinkW, SHStrDupW, ShellLink,
    AO_NONE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW, SetForegroundWindow,
    ShowWindowAsync, GWL_EXSTYLE, GWL_STYLE, GW_OWNER, SW_RESTORE, WM_CLOSE, WS_CAPTION, WS_CHILD,
    WS_DISABLED, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_THICKFRAME,
};

use crate::accounts::AccountProfile;
use crate::manager::{paths, state, AccountContext, ManagerResult};
use crate::native_interop::wide_str;
use crate::providers::ProviderId;

/// Opt-out sent to every managed child so background helpers stay quiet.
pub const DISABLE_NONESSENTIAL_ENV: &[(&str, &str)] =
    &[("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")];

/// Stable per-account taskbar identity root.
pub const APP_ID_PREFIX: &str = "CodeZeno.UsageHub";

/// Absolute-override env var for the shortcuts root (smoke/portable use).
/// When set it wins over the Start Menu; must be absolute. In `cfg(test)`
/// the per-test-thread fixture wins and env is ignored.
pub const SHORTCUTS_ROOT_ENV: &str = "USAGEHUB_SHORTCUTS_ROOT";

/// Graceful-stop wait budget before the caller reports `verified_idle: false`.
pub const GRACEFUL_STOP_WAIT_MS: u64 = 8_000;

/// Command-line query budget; on expiry the miss set stays empty.
pub const COMMAND_LINE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Image names owned by the managed Claude/Codex launches.
fn wanted_image_names(provider: ProviderId) -> &'static [&'static str] {
    match provider {
        ProviderId::Claude => &["claude.exe"],
        ProviderId::Codex => &["codex.exe", "chatgpt.exe"],
        _ => &[],
    }
}

fn all_wanted_image_names() -> &'static [&'static str] {
    &["claude.exe", "codex.exe", "chatgpt.exe"]
}

/// Map a process image name to its provider. Unknown images yield `None`
/// so foreign processes are never attributed (unknown is never idle).
fn provider_for_image(image: &str) -> Option<ProviderId> {
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        if wanted_image_names(provider)
            .iter()
            .any(|wanted| image.eq_ignore_ascii_case(wanted))
        {
            return Some(provider);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Official app discovery
// ---------------------------------------------------------------------------

/// Locate the installed desktop app, independently of CLI PATH resolution.
/// The Codex package registers ChatGPT.exe as its entry point; its window
/// may instead be hosted by Codex.exe from that same installed package.
pub fn discover_official_exe(provider: ProviderId) -> ManagerResult<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    match provider {
        ProviderId::Claude => {
            candidates.extend(msix_claude_exes());
            if let Some(local) = crate::known_folders::local_app_data_dir() {
                candidates.push(
                    local
                        .join("Microsoft")
                        .join("WindowsApps")
                        .join("claude.exe"),
                );
                candidates.push(local.join("Programs").join("Claude").join("Claude.exe"));
                candidates.push(local.join("Claude").join("Claude.exe"));
            }
            if let Ok(path) = app_paths_lookup("claude.exe") {
                candidates.push(path);
            }
            candidates.push(PathBuf::from("C:\\Program Files\\Claude\\Claude.exe"));
        }
        ProviderId::Codex => candidates.extend(
            registered_package_roots("OpenAI.Codex_2p2nqsd0c76g0")
                .into_iter()
                .map(|root| root.join("app").join("ChatGPT.exe")),
        ),
        _ => return Err("This provider does not support account profiles".into()),
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| match provider {
            ProviderId::Claude => {
                "Official Claude desktop app not found (registered MSIX package, WindowsApps alias, App Paths, or desktop install directory)"
                    .into()
            }
            _ => "Official Codex desktop package not found".into(),
        })
}

/// Use the vendor's real icon at every DPI, not a hand-encoded account ICO.
/// Cache only icon discovery; normal launch discovery remains fresh. A removed
/// package path is rediscovered without holding the cache lock during discovery.
fn official_app_icon(provider: ProviderId) -> ManagerResult<PathBuf> {
    if let Ok(cache) = OFFICIAL_ICON_CACHE.lock() {
        if let Some(path) = cache.get(&provider).filter(|path| path.is_file()) {
            return Ok(path.clone());
        }
    }
    let path = discover_official_exe(provider)?;
    if let Ok(mut cache) = OFFICIAL_ICON_CACHE.lock() {
        cache.insert(provider, path.clone());
    }
    Ok(path)
}

static OFFICIAL_ICON_CACHE: Mutex<BTreeMap<ProviderId, PathBuf>> = Mutex::new(BTreeMap::new());

/// Read `...\\App Paths\\<exe>` default value from HKCU, then HKLM.
fn app_paths_lookup(exe: &str) -> ManagerResult<PathBuf> {
    let subkey = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe}");
    for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
        if let Some(path) = app_paths_in_hive(hive, &subkey) {
            return Ok(path);
        }
    }
    Err(format!("No App Paths entry for {exe}"))
}

fn app_paths_in_hive(hive: HKEY, subkey: &str) -> Option<PathBuf> {
    unsafe {
        let subkey_w = wide_str(subkey);
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            hive,
            PCWSTR::from_raw(subkey_w.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return None;
        }
        let mut len = 0u32;
        let empty_w = wide_str("");
        let _ = RegQueryValueExW(
            key,
            PCWSTR::from_raw(empty_w.as_ptr()),
            None,
            None,
            None,
            Some(&mut len),
        );
        if len == 0 || len > 32_768 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u16; (len as usize + 1) / 2];
        let ok = RegQueryValueExW(
            key,
            PCWSTR::from_raw(empty_w.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        )
        .is_ok();
        let _ = RegCloseKey(key);
        if !ok {
            return None;
        }
        let end = buf.iter().position(|unit| *unit == 0).unwrap_or(buf.len());
        let text = String::from_utf16_lossy(&buf[..end]).trim().to_string();
        if text.is_empty() {
            return None;
        }
        // App Paths stores an executable path, not a command line. Unquoted
        // paths containing spaces are valid registry values.
        let path_text = text.trim_matches('"');
        let path = PathBuf::from(path_text);
        path.is_absolute().then_some(path)
    }
}

/// Resolve the installed MSIX binary through the current user's package
/// registrations. LocalAppData\Packages contains data, not installed binaries.
fn msix_claude_exes() -> Vec<PathBuf> {
    const PREFERRED: &str = "Claude_pzs8sxrjxfjjc";
    let mut families = vec![PREFERRED.to_string()];
    if let Some(local) = crate::known_folders::local_app_data_dir() {
        for entry in std::fs::read_dir(local.join("Packages"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(publisher) = name.strip_prefix("Claude_") {
                if publisher.len() == 13
                    && publisher
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                {
                    families.push(name);
                }
            }
        }
    }
    families.sort_by(|a, b| (a != PREFERRED, a).cmp(&(b != PREFERRED, b)));
    families.dedup();
    families
        .iter()
        .flat_map(|family| registered_package_roots(family))
        .flat_map(|root| [root.join("app").join("Claude.exe"), root.join("Claude.exe")])
        .collect()
}

fn registered_package_roots(family: &str) -> Vec<PathBuf> {
    let family = wide_str(family);
    let mut count = 0;
    let mut buffer_len = 0;
    unsafe {
        if GetPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            &mut count,
            None,
            &mut buffer_len,
            None,
        ) != ERROR_INSUFFICIENT_BUFFER
            || !(1..=64).contains(&count)
            || !(1..=65_536).contains(&buffer_len)
        {
            return Vec::new();
        }
        let mut names = vec![PWSTR::null(); count as usize];
        let mut buffer = vec![0u16; buffer_len as usize];
        if GetPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            &mut count,
            Some(names.as_mut_ptr()),
            &mut buffer_len,
            Some(PWSTR(buffer.as_mut_ptr())),
        ) != ERROR_SUCCESS
        {
            return Vec::new();
        }
        let mut roots = Vec::new();
        for name in names.into_iter().take(count as usize) {
            let mut length = 0;
            if GetPackagePathByFullName(PCWSTR(name.0), &mut length, None)
                != ERROR_INSUFFICIENT_BUFFER
                || !(2..=32_768).contains(&length)
            {
                continue;
            }
            let mut path = vec![0u16; length as usize];
            if GetPackagePathByFullName(PCWSTR(name.0), &mut length, Some(PWSTR(path.as_mut_ptr())))
                == ERROR_SUCCESS
            {
                let end = path
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(path.len());
                roots.push(PathBuf::from(String::from_utf16_lossy(&path[..end])));
            }
        }
        roots
    }
}

fn is_packaged_codex_image(path: &Path, roots: &[PathBuf]) -> bool {
    let image = canonical_spelling(path);
    roots.iter().any(|root| {
        ["ChatGPT.exe", "Codex.exe"]
            .iter()
            .any(|name| image == canonical_spelling(&root.join("app").join(name)))
    })
}

/// Only an external Default bound to a real provider-default root can use
/// native vendor activation and identity. Custom mappings remain isolated.
fn is_canonical_default(ctx: &AccountContext) -> bool {
    if ctx.is_managed() || !ctx.profile.id.eq_ignore_ascii_case("default") {
        return false;
    }
    if ctx.source == crate::accounts::SourceKind::ProviderDefault {
        return ctx.desktop_dir.is_none()
            && ctx.config_dir.is_none()
            && ctx.credential_file.is_none();
    }
    let defaults = canonical_default_dirs(ctx.provider);
    let is_default = |path: &PathBuf| defaults.contains(&canonical_spelling(path));
    ctx.source == crate::accounts::SourceKind::DesktopDir
        && ctx.desktop_dir.as_ref().is_some_and(is_default)
        && ctx.config_dir.as_ref().is_none_or(is_default)
}

fn packaged_app_id(provider: ProviderId) -> Option<&'static str> {
    match provider {
        ProviderId::Claude => Some("Claude_pzs8sxrjxfjjc!Claude"),
        ProviderId::Codex => Some("OpenAI.Codex_2p2nqsd0c76g0!App"),
        _ => None,
    }
}

fn activate_packaged_default(provider: ProviderId) -> ManagerResult<u32> {
    let app_id = packaged_app_id(provider)
        .ok_or_else(|| "Provider has no packaged desktop app".to_string())?;
    // A fresh thread gives activation its own known COM apartment, regardless
    // of the caller's existing apartment model.
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("usagehub-package-activation".into())
        .spawn(move || {
            let result = (|| unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|error| format!("Unable to initialize COM for {app_id}: {error}"))?;
                let result = (|| {
                    let manager: IApplicationActivationManager =
                        CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                            .map_err(|error| {
                                format!("Unable to create activation manager for {app_id}: {error}")
                            })?;
                    let id = wide_str(app_id);
                    let pid = manager
                        .ActivateApplication(PCWSTR(id.as_ptr()), PCWSTR::null(), AO_NONE)
                        .map_err(|error| format!("Unable to activate {app_id}: {error}"))?;
                    if pid == 0 {
                        return Err(format!("Activation of {app_id} returned no process ID"));
                    }
                    Ok(pid)
                })();
                CoUninitialize();
                result
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| format!("Unable to start activation for {app_id}: {error}"))?;
    // A stalled shell broker must not hold the caller's account guard forever
    // and starve quota polling. The COM apartment owns no account/auth locks.
    receiver
        .recv_timeout(Duration::from_secs(15))
        .map_err(|error| format!("Activation of {app_id} did not complete: {error}"))?
}

// ---------------------------------------------------------------------------
// Per-account paths / identity
// ---------------------------------------------------------------------------

/// Account data root: explicit desktop dir wins, then explicit config dir,
/// then the owned managed root. External default profiles have neither and
/// resolve to `None` so launches inherit provider-default discovery.
pub fn account_base_dir(ctx: &AccountContext) -> ManagerResult<Option<PathBuf>> {
    if let Some(dir) = &ctx.desktop_dir {
        return Ok(Some(paths::require_absolute(dir)?));
    }
    if let Some(dir) = &ctx.config_dir {
        return Ok(Some(paths::require_absolute(dir)?));
    }
    match ctx.source {
        crate::accounts::SourceKind::ProviderDefault => Ok(None),
        _ => Ok(Some(paths::managed_account_dir(
            ctx.provider,
            &ctx.profile.id,
        )?)),
    }
}

/// Per-account scratch dir for explicit child `TEMP`/`TMP`.
pub fn account_tmp_dir(ctx: &AccountContext) -> ManagerResult<Option<PathBuf>> {
    if !ctx.is_managed() {
        return Ok(None);
    }
    Ok(account_base_dir(ctx)?.map(|base| base.join("tmp")))
}

/// Stable taskbar identity: id-keyed so renames never retarget it.
pub fn account_app_user_model_id(provider: ProviderId, id: &str) -> String {
    let provider_key = match provider {
        ProviderId::Claude => "Claude",
        ProviderId::Codex => "Codex",
        _ => "Account",
    };
    let safe: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("{APP_ID_PREFIX}.{provider_key}.{safe}")
}

/// Marker carried on the child command line (`--user-data-dir`) and used to
/// attribute live processes to exactly one account. Lowercased for matching.
pub fn account_command_marker(ctx: &AccountContext) -> ManagerResult<Option<String>> {
    Ok(account_base_dir(ctx)?.map(|base| base.to_string_lossy().to_lowercase()))
}

/// True when `command_line` belongs to `marker`.
///
/// Boundary-aware: the character after the marker must be a quote, a path
/// separator, whitespace, or end-of-string, so `...\account_1` never matches
/// a sibling `...\account_10` process.
pub fn command_line_owns(command_line: &str, marker: &str) -> bool {
    let haystack = command_line.to_lowercase();
    let mut search_from = 0;
    while let Some(hit) = haystack[search_from..].find(marker) {
        let end = search_from + hit + marker.len();
        let boundary = haystack[end..]
            .chars()
            .next()
            .is_none_or(|ch| matches!(ch, '"' | '\'' | '\\' | '/' | ' ' | '\t'));
        if boundary {
            return true;
        }
        search_from = end;
    }
    false
}

/// Explicit child environment for an account launch or CLI run.
///
/// Dir roots only — never tokens, credential paths, or auth material.
/// A canonical Default honors its explicit CLI config directory, if present;
/// otherwise it inherits provider discovery. Isolated Desktop accounts use
/// their explicit config dir or `<desktop>/claude-config`.
pub fn account_child_env(ctx: &AccountContext) -> ManagerResult<Vec<(String, String)>> {
    let mut env: Vec<(String, String)> = DISABLE_NONESSENTIAL_ENV
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    let base = account_base_dir(ctx)?;
    let cli_root = if let Some(dir) = &ctx.config_dir {
        Some(paths::require_absolute(dir)?)
    } else if is_canonical_default(ctx) {
        None
    } else if let Some(desktop) = &ctx.desktop_dir {
        Some(paths::require_absolute(desktop)?.join("claude-config"))
    } else {
        base.clone()
    };
    if let Some(root) = cli_root {
        let text = root.to_string_lossy().to_string();
        match ctx.provider {
            ProviderId::Claude => env.push(("CLAUDE_CONFIG_DIR".into(), text)),
            ProviderId::Codex => env.push(("CODEX_HOME".into(), text)),
            _ => return Err("This provider does not support account profiles".into()),
        }
        if let Some(tmp) = account_tmp_dir(ctx)? {
            // This constructor owns the child env: create the tmp dir here
            // so import/CLI paths (which never run the launch ensure) do
            // not hand children a TEMP that does not exist.
            paths::ensure_dir(&tmp)?;
            let tmp_text = tmp.to_string_lossy().to_string();
            env.push(("TEMP".into(), tmp_text.clone()));
            env.push(("TMP".into(), tmp_text));
        }
    }
    Ok(env)
}

// ---------------------------------------------------------------------------
// Shared live snapshot (one native pass for ALL accounts)
// ---------------------------------------------------------------------------

/// One live OS process considered for account attribution.
#[derive(Clone, Debug)]
pub struct LiveProcess {
    pub pid: u32,
    pub provider: ProviderId,
    pub image: String,
    /// Codex image belongs to the registered official package, not a CLI.
    pub packaged_codex: bool,
    pub command_line: Option<String>,
    /// Creation time as Unix seconds, when readable.
    pub created_unix: Option<u64>,
    /// Owning process id from the Toolhelp snapshot (child-process walk).
    pub parent_pid: u32,
}

/// One native Toolhelp pass shared by every account, plus whether command
/// lines are complete. `attribution_complete == false` means at least one
/// wanted process still has no command line (CIM unavailable/slow): that
/// is UNKNOWN, never "not running". Consumers must fail closed on it.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub processes: Vec<LiveProcess>,
    pub attribution_complete: bool,
}

/// One live top-level window attributed to an account's processes.
#[derive(Clone, Debug)]
pub struct LiveWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub visible: bool,
    pub title: String,
}

/// `cfg(test)` process-table injector: when set, `live_process_snapshot`
/// returns exactly these processes and never touches the real Toolhelp
/// snapshot or PowerShell — no test can close a real user app. Unset means
/// an empty table (also hermetic). Production builds always read the OS.
thread_local! {
    static TEST_SNAPSHOT: std::cell::RefCell<Option<Vec<LiveProcess>>> =
        std::cell::RefCell::new(None);
    static TEST_ALLOW_SIGNALS: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static TEST_DEFAULT_ROOTS: std::cell::RefCell<Option<Vec<PathBuf>>> =
        std::cell::RefCell::new(None);
}

/// Install an exact fake process table for the current test thread.
/// Signaling/focus stays refused until [`allow_test_signals`] opts in.
#[cfg(test)]
pub fn set_test_snapshot(processes: Vec<LiveProcess>) {
    TEST_SNAPSHOT.with(|slot| *slot.borrow_mut() = Some(processes));
    TEST_ALLOW_SIGNALS.with(|flag| flag.set(false));
}

/// Opt this test thread into real signaling/focus of injector-supplied
/// PIDs (test-only; production never calls this).
#[cfg(test)]
pub fn allow_test_signals() {
    TEST_ALLOW_SIGNALS.with(|flag| flag.set(true));
}

/// Override the canonical Default data roots for the current test thread
/// (shared with the cleanup slice's fake-Default fixtures). Unset means an
/// empty set — never the real home/AppData.
#[cfg(test)]
pub fn set_test_default_roots(roots: Vec<PathBuf>) {
    TEST_DEFAULT_ROOTS.with(|slot| *slot.borrow_mut() = Some(roots));
}

#[cfg(test)]
fn test_signals_allowed() -> bool {
    TEST_ALLOW_SIGNALS.with(|flag| flag.get())
}

/// Command-line cache keyed by PID with the creation time it was read at.
/// PID reuse is detected by comparing birthtimes, so a recycled PID never
/// inherits the previous owner's command line.
static CMDLINE_CACHE: Mutex<BTreeMap<u32, (Option<u64>, String)>> = Mutex::new(BTreeMap::new());

/// Negative cache: after a CIM failure, skip the probe for a bounded
/// cooldown instead of spawning PowerShell on every tick.
static CMDLINE_FAILED_UNTIL: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Cooldown after a failed command-line probe (R5).
const CMDLINE_FAIL_COOLDOWN: Duration = Duration::from_secs(60);

/// Single shared snapshot for every account: one native Toolhelp pass, then
/// command lines from the birthtime cache with ONE batched CIM query for
/// misses only. Steady-state timer ticks spawn nothing; a failed probe is
/// negatively cached for a bounded cooldown instead of re-spawning, and
/// the timed-out child is always killed and reaped.
pub fn live_process_snapshot() -> Snapshot {
    #[cfg(test)]
    {
        // Hermetic by default: an unset injector means an empty table, so
        // no test ever reads the real process table or spawns PowerShell.
        if let Some(processes) = TEST_SNAPSHOT.with(|slot| slot.borrow().clone()) {
            let complete = processes.iter().all(|p| p.command_line.is_some());
            return Snapshot {
                processes,
                attribution_complete: complete,
            };
        }
        return Snapshot {
            processes: Vec::new(),
            attribution_complete: true,
        };
    }
    #[cfg(not(test))]
    {
        live_process_snapshot_inner()
    }
}

#[cfg(not(test))]
fn live_process_snapshot_inner() -> Snapshot {
    let (mut processes, images_complete) = snapshot_processes(all_wanted_image_names());
    if processes.is_empty() {
        if let Ok(mut cache) = CMDLINE_CACHE.lock() {
            cache.clear();
        }
        return Snapshot {
            processes,
            attribution_complete: images_complete,
        };
    }
    // Collect the miss set under the lock, then release before spawning.
    let misses: Vec<u32> = {
        let cache = CMDLINE_CACHE.lock();
        let Ok(cache) = cache else {
            return Snapshot {
                processes,
                attribution_complete: false,
            };
        };
        processes
            .iter()
            .filter(|process| {
                cache
                    .get(&process.pid)
                    .is_none_or(|(created, _)| *created != process.created_unix)
            })
            .map(|process| process.pid)
            .collect()
    };
    let mut complete = images_complete;
    if !misses.is_empty() {
        let (fresh, probe_ok) = query_command_lines_once();
        complete &= probe_ok;
        if let Ok(mut cache) = CMDLINE_CACHE.lock() {
            let live: BTreeSet<u32> = processes.iter().map(|p| p.pid).collect();
            cache.retain(|pid, _| live.contains(pid));
            for (pid, line) in fresh {
                if live.contains(&pid) {
                    let created = processes
                        .iter()
                        .find(|p| p.pid == pid)
                        .and_then(|p| p.created_unix);
                    cache.insert(pid, (created, line));
                }
            }
        }
    } else if let Ok(mut cache) = CMDLINE_CACHE.lock() {
        let live: BTreeSet<u32> = processes.iter().map(|p| p.pid).collect();
        cache.retain(|pid, _| live.contains(pid));
    }
    if let Ok(cache) = CMDLINE_CACHE.lock() {
        for process in &mut processes {
            process.command_line = cache.get(&process.pid).map(|(_, line)| line.clone());
        }
    }
    if complete {
        complete = processes.iter().all(|p| p.command_line.is_some());
    }
    Snapshot {
        processes,
        attribution_complete: complete,
    }
}

/// Canonical spelling for root comparisons: backslashes, lowercase,
/// Win32/NT prefixes stripped. Shared by attribution, overlap checks, and
/// the lifecycle slice (which reuses this instead of a third copy).
pub fn canonical_spelling(path: &Path) -> String {
    let mut spelling = path.to_string_lossy().replace('/', "\\").to_lowercase();
    for prefix in ["\\\\?\\unc\\", "\\??\\unc\\", "\\\\?\\", "\\??\\"] {
        if let Some(rest) = spelling.strip_prefix(prefix) {
            spelling = if prefix.ends_with("unc\\") {
                format!("\\\\{rest}")
            } else {
                rest.to_string()
            };
            break;
        }
    }
    spelling.trim_end_matches('\\').to_string()
}

/// Overlap check over canonical spellings: true when the roots are equal
/// or one contains the other. Segment-boundary aware — a shared string
/// prefix is not enough (`C:\p\account_1` does NOT overlap
/// `C:\p\account_10`). Shared by the lifecycle collision gates, which
/// compare [`canonical_spelling`] outputs.
pub fn overlaps(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('\\'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('\\'))
}

/// Attribute snapshot processes to `ctx` via its command-line marker,
/// plus transitive Electron/Chromium children (renderers, GPU, utility)
/// that carry no `--user-data-dir` themselves. The walk is depth-capped
/// and birthtime-guarded against PID reuse: a child must be born at or
/// after its attributed parent when both birthtimes are known.
pub fn attribute_snapshot(ctx: &AccountContext, snapshot: &[LiveProcess]) -> Vec<LiveProcess> {
    let marker = account_command_marker(ctx).unwrap_or(None);
    let Some(marker) = marker else {
        return Vec::new();
    };
    let mut owned: BTreeMap<u32, &LiveProcess> = snapshot
        .iter()
        .filter(|process| {
            process.provider == ctx.provider
                && process
                    .command_line
                    .as_deref()
                    .is_some_and(|line| command_line_owns(line, &marker))
        })
        .map(|process| (process.pid, process))
        .collect();
    for _ in 0..8 {
        let mut grown = false;
        for process in snapshot.iter().filter(|p| p.provider == ctx.provider) {
            if owned.contains_key(&process.pid) {
                continue;
            }
            let Some(parent) = owned.get(&process.parent_pid) else {
                continue;
            };
            let ordered = match (process.created_unix, parent.created_unix) {
                (Some(child), Some(birth)) => child >= birth,
                _ => true,
            };
            if ordered {
                owned.insert(process.pid, process);
                grown = true;
            }
        }
        if !grown {
            break;
        }
    }
    owned.into_values().cloned().collect()
}

/// Attribute one account against a shared snapshot, including the external
/// Default arm. Managed accounts use marker (+children) attribution.
/// The external Default (`ownership == External`, id `default`,
/// case-insensitive) owns provider-default launches: processes whose
/// command line has NO `--user-data-dir` switch, or whose value
/// canonicalizes inside a canonical default root — but never a process
/// carrying another account's marker, and never a process (or child)
/// already attributed to a managed account. No marker and not the
/// external Default attributes nothing rather than guessing.
pub fn attribute_account_in(ctx: &AccountContext, snapshot: &Snapshot) -> Vec<LiveProcess> {
    let marker = account_command_marker(ctx).unwrap_or(None);
    let external_default = ctx.ownership == crate::accounts::AccountOwnership::External
        && ctx.profile.id.eq_ignore_ascii_case("default");
    if !external_default {
        return if marker.is_some() {
            attribute_snapshot(ctx, &snapshot.processes)
        } else {
            Vec::new()
        };
    }
    let defaults = canonical_default_dirs(ctx.provider);
    // A migrated canonical Desktop Default must still recognize the vendor's
    // already-running, marker-less launch. Custom mapped External profiles
    // retain strict marker-only attribution.
    let canonical_mapping = is_canonical_default(ctx);
    if marker.is_some() && !canonical_mapping {
        return attribute_snapshot(ctx, &snapshot.processes);
    }
    let managed = managed_attributed_pids(ctx.provider, snapshot);
    let foreign = registered_markers(ctx.provider);
    snapshot
        .processes
        .iter()
        .filter(|process| {
            process.provider == ctx.provider
                && (ctx.provider != ProviderId::Codex || process.packaged_codex)
                && !managed.contains(&process.pid)
                && process.command_line.as_deref().is_some_and(|line| {
                    let lower = line.to_lowercase();
                    if foreign
                        .iter()
                        .any(|marker| command_line_owns(&lower, marker))
                    {
                        return false;
                    }
                    match user_data_dir_value(&lower) {
                        None => true,
                        Some(value) => defaults
                            .iter()
                            .any(|root| value == *root || value.starts_with(&format!("{root}\\"))),
                    }
                })
        })
        .cloned()
        .collect()
}

/// PIDs already owned by managed accounts (direct marker + transitive
/// children): the Default arm must never claim these subtrees.
fn managed_attributed_pids(provider: ProviderId, snapshot: &Snapshot) -> BTreeSet<u32> {
    let settings = state::load_fresh();
    let mut out = BTreeSet::new();
    let Some(accounts) = settings.accounts.get(provider) else {
        return out;
    };
    for profile in &accounts.profiles {
        if profile.id.eq_ignore_ascii_case("default") {
            continue;
        }
        let Ok(ctx) = AccountContext::from_profile(provider, profile) else {
            continue;
        };
        if account_command_marker(&ctx).unwrap_or(None).is_none() {
            continue;
        }
        out.extend(
            attribute_snapshot(&ctx, &snapshot.processes)
                .iter()
                .map(|p| p.pid),
        );
    }
    out
}

/// Every explicit desktop/config root of every registered account
/// (lowercased full paths): the Default arm excludes any command line
/// carrying one of these, so a managed window is never misattributed.
fn registered_markers(provider: ProviderId) -> Vec<String> {
    let settings = state::load_fresh();
    let mut out = Vec::new();
    for probe in [ProviderId::Claude, ProviderId::Codex] {
        if probe != provider {
            continue;
        }
        let Some(accounts) = settings.accounts.get(probe) else {
            continue;
        };
        for profile in &accounts.profiles {
            if profile.id.eq_ignore_ascii_case("default") {
                continue;
            }
            for field in [&profile.desktop_dir, &profile.config_dir] {
                if !field.trim().is_empty() {
                    out.push(field.to_lowercase());
                }
            }
        }
    }
    out
}

/// Extract and canonicalize a `--user-data-dir=` value, if present.
fn user_data_dir_value(command_line: &str) -> Option<String> {
    let key = "--user-data-dir=";
    let at = command_line.find(key)?;
    let mut value = command_line[at + key.len()..].trim_start().to_string();
    if let Some(stripped) = value.strip_prefix('"') {
        value = stripped.split('"').next().unwrap_or("").to_string();
    } else {
        value = value.split_whitespace().next().unwrap_or("").to_string();
    }
    if value.is_empty() {
        return None;
    }
    Some(canonical_spelling(Path::new(&value)))
}

/// Canonical provider-Default data roots as real paths (for probing).
/// In `cfg(test)` this NEVER touches the real home/AppData: unset means an
/// empty set (a fake Default with no locks reads `Ready`), and fixtures
/// that need lock coverage inject their own roots with
/// [`set_test_default_roots`] (shared with the cleanup slice's fake-Default
/// tests). Real user Default activity can therefore never make tests flaky
/// or expose real lock state.
pub(crate) fn canonical_default_dir_paths(provider: ProviderId) -> Vec<PathBuf> {
    #[cfg(test)]
    {
        if let Some(roots) = TEST_DEFAULT_ROOTS.with(|slot| slot.borrow().clone()) {
            return roots;
        }
        let _ = provider;
        return Vec::new();
    }
    #[cfg(not(test))]
    {
        canonical_default_dir_paths_inner(provider)
    }
}

#[cfg(not(test))]
fn canonical_default_dir_paths_inner(provider: ProviderId) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config) = crate::accounts::environment_directory(provider) {
        roots.push(config);
    }
    if let Some(home) = crate::known_folders::home_dir() {
        let folder = match provider {
            ProviderId::Claude => ".claude",
            _ => ".codex",
        };
        roots.push(home.join(folder));
    }
    if provider == ProviderId::Claude {
        if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
            roots.push(roaming.join("Claude"));
        }
        if let Some(local) = crate::known_folders::local_app_data_dir() {
            if let Ok(entries) = std::fs::read_dir(local.join("Claude-Data")) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let publisher = name.strip_prefix("claude_").unwrap_or_default();
                    if publisher.len() == 13
                        && publisher
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    {
                        roots.push(entry.path());
                    }
                }
            }
            if let Ok(entries) = std::fs::read_dir(local.join("Packages")) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let publisher = name.strip_prefix("Claude_").unwrap_or_default();
                    if publisher.len() == 13
                        && publisher
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    {
                        roots.push(
                            entry
                                .path()
                                .join("LocalCache")
                                .join("Roaming")
                                .join("Claude"),
                        );
                    }
                }
            }
        }
    }
    roots
}

/// Canonical provider-Default data roots as compare spellings.
fn canonical_default_dirs(provider: ProviderId) -> Vec<String> {
    canonical_default_dir_paths(provider)
        .iter()
        .map(|root| canonical_spelling(root))
        .collect()
}

/// Attribute live processes to `ctx` (one shared snapshot internally).
pub fn account_live_processes(ctx: &AccountContext) -> Vec<LiveProcess> {
    attribute_account_in(ctx, &live_process_snapshot())
}

/// Window lookup over an already-taken shared snapshot.
pub fn account_live_windows_in(ctx: &AccountContext, snapshot: &Snapshot) -> Vec<LiveWindow> {
    let pids: BTreeSet<u32> = attribute_account_in(ctx, snapshot)
        .into_iter()
        .map(|process| process.pid)
        .collect();
    if pids.is_empty() {
        return Vec::new();
    }
    windows_for_pids(&pids)
}

fn snapshot_processes(image_names: &[&str]) -> (Vec<LiveProcess>, bool) {
    let mut out = Vec::new();
    let mut complete = true;
    let mut desktop_paths: BTreeMap<ProviderId, Option<PathBuf>> = BTreeMap::new();
    let codex_roots = registered_package_roots("OpenAI.Codex_2p2nqsd0c76g0");
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return (out, false);
        };
        if snapshot.is_invalid() {
            return (out, false);
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let image = String::from_utf16_lossy(
                &entry.szExeFile[..entry
                    .szExeFile
                    .iter()
                    .position(|unit| *unit == 0)
                    .unwrap_or(entry.szExeFile.len())],
            );
            if let Some(provider) = provider_for_image(&image) {
                if image_names
                    .iter()
                    .any(|wanted| image.eq_ignore_ascii_case(wanted))
                {
                    let mut packaged_codex = false;
                    if provider == ProviderId::Codex {
                        let image_path = process_image_path(entry.th32ProcessID);
                        if image_path.is_none() {
                            complete = false;
                        }
                        packaged_codex = image_path
                            .as_ref()
                            .is_some_and(|path| is_packaged_codex_image(path, &codex_roots));
                        if !packaged_codex {
                            more = Process32NextW(snapshot, &mut entry).is_ok();
                            continue;
                        }
                    } else if image.eq_ignore_ascii_case("claude.exe") {
                        let official = desktop_paths
                            .entry(provider)
                            .or_insert_with(|| discover_official_exe(provider).ok());
                        let image_path = process_image_path(entry.th32ProcessID);
                        if official.is_none() || image_path.is_none() {
                            complete = false;
                        }
                        if !official
                            .as_ref()
                            .zip(image_path.as_ref())
                            .is_some_and(|(a, b)| canonical_spelling(a) == canonical_spelling(b))
                        {
                            more = Process32NextW(snapshot, &mut entry).is_ok();
                            continue;
                        }
                    }
                    out.push(LiveProcess {
                        pid: entry.th32ProcessID,
                        provider,
                        image,
                        packaged_codex,
                        command_line: None,
                        created_unix: process_created_unix(entry.th32ProcessID),
                        parent_pid: entry.th32ParentProcessID,
                    });
                }
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }
    (out, complete)
}

fn process_created_unix(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut created: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut exited: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut kernel: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut user: windows::Win32::Foundation::FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user).is_ok();
        if !ok {
            return None;
        }
        let ticks = ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64;
        ticks
            .checked_sub(11_644_473_600 * 10_000_000)
            .map(|rest| rest / 10_000_000)
    }
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let len = GetModuleFileNameExW(Some(handle), None, &mut buf);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if len == 0 {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }
}

/// One batched CIM query for every wanted image at once. Returns the map
/// plus whether the probe itself succeeded. On any failure the map is empty
/// AND a bounded negative cache suppresses re-spawns for a cooldown; the
/// timed-out child is always killed and reaped, never abandoned.
fn query_command_lines_once() -> (BTreeMap<u32, String>, bool) {
    if let Ok(guard) = CMDLINE_FAILED_UNTIL.lock() {
        if guard.is_some_and(|until| std::time::Instant::now() < until) {
            return (BTreeMap::new(), false);
        }
    }
    let failed = |empty: BTreeMap<u32, String>| {
        if let Ok(mut guard) = CMDLINE_FAILED_UNTIL.lock() {
            *guard = Some(std::time::Instant::now() + CMDLINE_FAIL_COOLDOWN);
        }
        (empty, false)
    };
    let filter = all_wanted_image_names()
        .iter()
        .map(|name| format!("Name = '{name}'"))
        .collect::<Vec<_>>()
        .join(" OR ");
    // Explicit UTF-8 so the JSON pipe decodes deterministically.
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; Get-CimInstance Win32_Process -Filter \"{filter}\" | Select-Object ProcessId,CommandLine | ConvertTo-Json -Compress"
    );
    let mut command = std::process::Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    // Bounded capture on the OS-query budget, drained concurrently by the
    // runner: no post-exit pipe read, no ad-hoc wait loop. Timeout, exit
    // failure, and truncation all report Unknown with a 60s negative cache.
    const COMMAND_LINE_CAPTURE_CAP: usize = 8 * 1024 * 1024;
    let output = match crate::manager::process::run_bounded(
        &mut command,
        COMMAND_LINE_QUERY_TIMEOUT,
        COMMAND_LINE_CAPTURE_CAP,
        "native.command-lines",
    ) {
        Ok(output) => output,
        Err(_) => return failed(BTreeMap::new()),
    };
    if !output.status.success() {
        return failed(BTreeMap::new());
    }
    if output.stdout.len() > COMMAND_LINE_CAPTURE_CAP {
        // Truncated capture would attribute only part of the table: report
        // Unknown and cool down instead of caching a half-filled map.
        return failed(BTreeMap::new());
    }
    let map = parse_command_lines(&String::from_utf8_lossy(&output.stdout));
    (map, true)
}

fn parse_command_lines(json: &str) -> BTreeMap<u32, String> {
    let trimmed = json.trim();
    if trimmed.is_empty() {
        return BTreeMap::new();
    }
    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return BTreeMap::new(),
    };
    let entries: Vec<&serde_json::Value> = match &value {
        serde_json::Value::Array(items) => items.iter().collect(),
        single @ serde_json::Value::Object(_) => vec![single],
        _ => Vec::new(),
    };
    let mut map = BTreeMap::new();
    for entry in entries {
        let pid = entry
            .get("ProcessId")
            .and_then(|id| id.as_u64())
            .and_then(|id| u32::try_from(id).ok());
        let line = entry
            .get("CommandLine")
            .and_then(|line| line.as_str())
            .map(str::to_string);
        if let (Some(pid), Some(line)) = (pid, line) {
            map.insert(pid, line);
        }
    }
    map
}

struct EnumState {
    pids: BTreeSet<u32>,
    windows: Vec<LiveWindow>,
}

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, param: LPARAM) -> BOOL {
    let state = &mut *(param.0 as *mut EnumState);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if !state.pids.contains(&pid) {
        return true.into();
    }
    let mut text = vec![0u16; 512];
    let len = GetWindowTextW(hwnd, &mut text);
    let title = if len > 0 {
        String::from_utf16_lossy(&text[..len as usize])
    } else {
        String::new()
    };
    state.windows.push(LiveWindow {
        hwnd: hwnd.0 as isize,
        pid,
        visible: IsWindowVisible(hwnd).as_bool(),
        title,
    });
    true.into()
}

fn windows_for_pids(pids: &BTreeSet<u32>) -> Vec<LiveWindow> {
    let mut state = EnumState {
        pids: pids.clone(),
        windows: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_cb),
            LPARAM(&mut state as *mut EnumState as isize),
        );
    }
    state.windows
}

#[derive(Clone, Copy)]
struct WindowCandidate {
    hwnd: isize,
    pid: u32,
    owner: isize,
    visible: bool,
    style: u32,
    ex_style: u32,
    has_area: bool,
}

fn primary_window(windows: &[WindowCandidate]) -> Option<&WindowCandidate> {
    let eligible = |w: &&WindowCandidate| {
        w.has_area
            && w.style & WS_CHILD.0 == 0
            && w.ex_style & (WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0) == 0
    };
    let primary = windows
        .iter()
        .filter(eligible)
        .filter(|w| {
            (w.owner == 0 || w.ex_style & WS_EX_APPWINDOW.0 != 0)
                && (w.style & WS_CAPTION.0 == WS_CAPTION.0
                    || w.style & WS_THICKFRAME.0 != 0
                    || w.ex_style & WS_EX_APPWINDOW.0 != 0)
        })
        .min_by_key(|w| !w.visible)?;
    // Every link in a modal chain must be an attributed, visible, eligible
    // window. Bound traversal by the candidate count so stale/cyclic owner
    // relationships cannot hang selection.
    if primary.style & WS_DISABLED.0 != 0 {
        return windows.iter().filter(eligible).find(|dialog| {
            if !dialog.visible || dialog.style & WS_DISABLED.0 != 0 {
                return false;
            }
            let mut owner = dialog.owner;
            for _ in 0..windows.len() {
                if owner == primary.hwnd {
                    return true;
                }
                let Some(parent) = windows
                    .iter()
                    .filter(eligible)
                    .find(|w| w.hwnd == owner && w.visible && w.style & WS_DISABLED.0 != 0)
                else {
                    return false;
                };
                owner = parent.owner;
            }
            false
        });
    }
    Some(primary)
}

fn window_candidate(window: &LiveWindow) -> Option<WindowCandidate> {
    unsafe {
        let hwnd = HWND(window.hwnd as *mut _);
        let mut pid = 0;
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != window.pid {
            return None;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return None;
        }
        Some(WindowCandidate {
            hwnd: window.hwnd,
            pid,
            owner: GetWindow(hwnd, GW_OWNER).unwrap_or_default().0 as isize,
            visible: IsWindowVisible(hwnd).as_bool(),
            style: GetWindowLongW(hwnd, GWL_STYLE) as u32,
            ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
            has_area: rect.right > rect.left && rect.bottom > rect.top,
        })
    }
}

fn account_primary_window(windows: &[LiveWindow]) -> Option<WindowCandidate> {
    let candidates: Vec<_> = windows.iter().filter_map(window_candidate).collect();
    primary_window(&candidates).copied()
}

// ---------------------------------------------------------------------------
// Launch / focus / stop
// ---------------------------------------------------------------------------

pub(crate) fn ensure_managed_launch_roots(ctx: &AccountContext) -> ManagerResult<()> {
    if ctx.is_managed() {
        if let Some(base) = account_base_dir(ctx)? {
            paths::ensure_dir(&base)?;
            paths::ensure_dir(&base.join("tmp"))?;
            if ctx.provider == ProviderId::Claude && ctx.desktop_dir.is_some() {
                paths::ensure_dir(&base.join("claude-config"))?;
            }
        }
        if let Some(config) = &ctx.config_dir {
            paths::ensure_dir(&paths::require_absolute(config)?)?;
        }
    }
    Ok(())
}

/// Build the official-app command for `ctx`: explicit child environment,
/// per-account `--user-data-dir`, managed roots + tmp (+ nested Desktop
/// CLI config) ensured first. External default profiles launch without
/// overrides.
fn prepare_launch_command(
    ctx: &AccountContext,
    extra_args: &[String],
) -> ManagerResult<(String, std::process::Command)> {
    let exe = discover_official_exe(ctx.provider)?;
    let base = if is_canonical_default(ctx) {
        None
    } else {
        account_base_dir(ctx)?
    };
    ensure_managed_launch_roots(ctx)?;
    let mut command = std::process::Command::new(&exe);
    match ctx.provider {
        ProviderId::Claude => {
            if let Some(base) = &base {
                command.arg(format!("--user-data-dir={}", base.display()));
            }
        }
        ProviderId::Codex => return Err(
            "Codex desktop does not support isolated account launches; use the provider CLI for this profile".into()
        ),
        _ => return Err("This provider does not support account profiles".into()),
    }
    for arg in extra_args {
        command.arg(arg);
    }
    for (key, value) in account_child_env(ctx)? {
        command.env(key, value);
    }
    if let Some(base) = &base {
        command.current_dir(base);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0000_0008); // DETACHED_PROCESS
    }
    // Long-lived official GUI spawn: never routed through the bounded console
    // runner. Null stdio so GUI descendants cannot inherit the manager's
    // response pipes.
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    Ok((exe.display().to_string(), command))
}

/// Spawn helper that keeps the `Child` handle so the caller can observe
/// early exit (forward-to-existing-instance) instead of stalling.
/// Dropping the handle never kills the child.
fn spawn_detached(
    mut command: std::process::Command,
    exe_display: &str,
) -> ManagerResult<(u32, std::process::Child)> {
    // Shared spawn policy: clear inherited STD handle flags before any child
    // is created. The official app is launched with stdio NUL, but Rust's
    // Windows launcher still passes every other inheritable handle in this
    // process; if this manager inherited response pipes, the long-lived GUI
    // would keep them open after the manager exits and stall the caller.
    crate::manager::process::clear_inherited_standard_handles();
    let mut child = command
        .spawn()
        .map_err(|error| format!("Unable to launch {exe_display}: {error}"))?;
    let pid = child.id();
    // Reap promptly if it already exited (forward-to-existing-instance):
    // `try_wait` both reports and reaps without blocking.
    let _ = child.try_wait();
    Ok((pid, child))
}

/// Open an interactive sign-in console for an isolated Codex CLI profile.
/// The console owns the login flow; UsageHub only prepares the same
/// `CODEX_HOME` the poller reads. Nothing is captured or terminated here.
pub fn open_codex_sign_in(ctx: &AccountContext) -> ManagerResult<()> {
    if ctx.provider != ProviderId::Codex {
        return Err("Only Codex CLI profiles need a separate sign-in console".into());
    }
    if is_canonical_default(ctx) {
        return Err(
            "Default uses the official app or the default CODEX_HOME; no isolated sign-in is needed"
                .into(),
        );
    }
    ensure_managed_launch_roots(ctx)?;
    let mut command = std::process::Command::new("cmd.exe");
    command.args(["/k", "codex login"]);
    for (key, value) in account_child_env(ctx)? {
        command.env(key, value);
    }
    if let Some(base) = account_base_dir(ctx)? {
        command.current_dir(base);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_CONSOLE: the login prompts need a real console.
        command.creation_flags(0x0000_0010);
    }
    command
        .spawn()
        .map_err(|error| format!("Unable to open the Codex sign-in console: {error}"))?;
    Ok(())
}

/// Focus this account's existing window instead of opening a duplicate.
/// Also (re)stamps every live window of the account with its identity +
/// relaunch metadata, so grouping survives restarts. Returns `true` when an
/// existing window was restored — even when the foreground grant itself is
/// denied (a routine foreground-lock outcome that must NOT read as "not
/// focused" and trigger a duplicate launch). In `cfg(test)` signaling and
/// focus are refused unless the test injector explicitly opts in, so no
/// test can touch a real window.
pub fn focus_account_window(ctx: &AccountContext) -> ManagerResult<bool> {
    #[cfg(test)]
    if !test_signals_allowed() {
        return Err("Refusing to focus a window in tests".into());
    }
    let snapshot = live_process_snapshot();
    stamp_account_windows_in(ctx, &snapshot);
    let windows = account_live_windows_in(ctx, &snapshot);
    let Some(target) = account_primary_window(&windows) else {
        // A tray/helper process is not an existing primary window. Let the
        // normal official-app activation create or restore the primary.
        return Ok(false);
    };
    // An HWND can be destroyed and recycled between enumeration and focus.
    // Re-read its ownership and selection and, when available, process birth.
    let birth = snapshot
        .processes
        .iter()
        .find(|p| p.pid == target.pid)
        .and_then(|p| p.created_unix);
    if birth.is_some() && process_created_unix(target.pid) != birth {
        return Err("Account window process changed before focus".into());
    }
    let current = account_live_windows_in(ctx, &snapshot);
    if !account_primary_window(&current)
        .is_some_and(|window| window.hwnd == target.hwnd && window.pid == target.pid)
    {
        return Err("Account window changed before focus".into());
    }
    unsafe {
        let hwnd = HWND(target.hwnd as *mut _);
        let mut pid = 0;
        if !IsWindow(Some(hwnd)).as_bool()
            || GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0
            || pid != target.pid
        {
            return Err("Account window changed before focus".into());
        }
        // Never join the vendor's input queue: a stalled target can otherwise
        // block this worker while it owns the account-operation guard, starving
        // quota polls. Restore asynchronously and respect foreground-lock rules.
        let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
        Ok(true)
    }
}

/// Gracefully close this account's windows and wait for its processes to
/// exit. Only matched PIDs are signaled. Hidden (tray) windows are signaled
/// too — hiding is not stopping. In `cfg(test)` signaling is refused
/// unless the test injector explicitly opts in.
pub fn stop_account_graceful(ctx: &AccountContext) -> ManagerResult<StopReport> {
    #[cfg(test)]
    if !test_signals_allowed() {
        return Err("Refusing to signal processes in tests".into());
    }
    let snapshot = live_process_snapshot();
    let unknown = !snapshot.attribution_complete;
    let windows = account_live_windows_in(ctx, &snapshot);
    let mut signaled = 0usize;
    unsafe {
        for window in &windows {
            let hwnd = HWND(window.hwnd as *mut _);
            if IsWindow(Some(hwnd)).as_bool()
                && PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)).is_ok()
            {
                signaled += 1;
            }
        }
    }
    let stopped = wait_for_exit(ctx, GRACEFUL_STOP_WAIT_MS);
    let remaining = account_live_processes(ctx);
    let idle = !unknown && wait_verified(ctx);
    let mut notes = String::new();
    if unknown {
        notes = "Process attribution was incomplete (command-line probe unavailable); quiesce is unverified".into();
    }
    Ok(StopReport {
        provider: ctx.provider,
        account_id: ctx.profile.id.clone(),
        force: false,
        signaled_windows: signaled,
        stopped_pids: stopped,
        remaining_pids: remaining.into_iter().map(|p| p.pid).collect(),
        verified_idle: idle,
        attribution_unknown: unknown,
        notes,
    })
}

/// Forced stop. Explicit user action only: terminates only matched PIDs
/// (leaves first, so the browser cannot orphan children) without a
/// graceful close. Never called implicitly by cleanup or the controller.
/// In `cfg(test)` termination is refused unless the injector opts in.
pub fn stop_account_force(ctx: &AccountContext) -> ManagerResult<StopReport> {
    #[cfg(test)]
    if !test_signals_allowed() {
        return Err("Refusing to terminate processes in tests".into());
    }
    let snapshot = live_process_snapshot();
    let unknown = !snapshot.attribution_complete;
    let processes = attribute_account_in(ctx, &snapshot);
    // Leaves first (reverse topological order): children before parents.
    let mut ordered = processes.clone();
    let owned: BTreeSet<u32> = processes.iter().map(|p| p.pid).collect();
    ordered.sort_by_key(|process| {
        let mut depth = 0u32;
        let mut cursor = process.parent_pid;
        while owned.contains(&cursor) && depth < 8 {
            depth += 1;
            cursor = processes
                .iter()
                .find(|p| p.pid == cursor)
                .map(|p| p.parent_pid)
                .unwrap_or(0);
        }
        std::cmp::Reverse(depth);
    });
    let mut stopped = Vec::new();
    unsafe {
        for process in &ordered {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, process.pid) {
                if TerminateProcess(handle, 1).is_ok() {
                    stopped.push(process.pid);
                }
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
    wait_for_exit(ctx, 5_000);
    let remaining = account_live_processes(ctx);
    let idle = !unknown && wait_verified(ctx);
    let mut notes = "Forced termination requested explicitly".to_string();
    if unknown {
        notes.push_str("; attribution was incomplete, quiesce is unverified");
    }
    Ok(StopReport {
        provider: ctx.provider,
        account_id: ctx.profile.id.clone(),
        force: true,
        signaled_windows: 0,
        stopped_pids: stopped,
        remaining_pids: remaining.into_iter().map(|p| p.pid).collect(),
        verified_idle: idle,
        attribution_unknown: unknown,
        notes,
    })
}

fn wait_for_exit(ctx: &AccountContext, budget_ms: u64) -> Vec<u32> {
    let before: BTreeSet<u32> = account_live_processes(ctx)
        .into_iter()
        .map(|p| p.pid)
        .collect();
    if before.is_empty() {
        return Vec::new();
    }
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < budget_ms as u128 {
        // Birthtime cache keeps these re-snapshots spawn-free in steady state.
        let now: BTreeSet<u32> = account_live_processes(ctx)
            .into_iter()
            .map(|p| p.pid)
            .collect();
        if now.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let after: BTreeSet<u32> = account_live_processes(ctx)
        .into_iter()
        .map(|p| p.pid)
        .collect();
    before.difference(&after).copied().collect()
}

/// Read-only lock readiness for one account. `Ready` means no attributed
/// live process holds this profile AND its on-disk lock (if any) is stale
/// or absent; `Busy` means a live holder exists; `Unknown` means readiness
/// could not be determined (unresolvable paths, unavailable attribution)
/// and the caller must fail closed.
///
/// Chromium/Electron `SingletonLock` is a symlink whose target names the
/// holder (`"<hostname>-<pid>"`); a leftover 0-byte file or dangling link
/// from a crash is NOT proof of a live holder, so the probe parses the
/// target and checks process liveness instead of mere existence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockReadiness {
    Ready,
    Busy,
    Unknown(String),
}

/// The single readiness check backup/snapshot callers must use instead of
/// `lockfile.exists()`. Read-only: nothing is signaled, written, created,
/// or deleted.
pub fn profile_lock_ready(ctx: &AccountContext) -> ManagerResult<LockReadiness> {
    let snapshot = live_process_snapshot();
    if !snapshot.attribution_complete {
        return Ok(LockReadiness::Unknown(
            "process attribution unavailable".into(),
        ));
    }
    if !attribute_account_in(ctx, &snapshot).is_empty() {
        return Ok(LockReadiness::Busy);
    }
    let base = account_base_dir(ctx)
        .map_err(|error| format!("Unable to resolve the account base dir: {error}"))?;
    match base {
        Some(base) => Ok(probe_singleton_lock(&base)),
        // External default: canonical roots hold the real locks.
        None => Ok(default_lock_state(ctx.provider)),
    }
}

/// Chromium/Electron singleton-lock probe. A link whose target names a
/// live pid is `Busy`; missing, dangling, unparseable-after-a-live-check,
/// or stale-holder targets are `Ready`; an unreadable directory is
/// `Unknown` (fail closed, never assume).
fn probe_singleton_lock(base: &Path) -> LockReadiness {
    let link = base.join("SingletonLock");
    let target = match std::fs::read_link(&link) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockReadiness::Ready,
        Err(error) => return LockReadiness::Unknown(error.to_string()),
    };
    match target
        .to_string_lossy()
        .rsplit_once('-')
        .and_then(|(_, pid)| pid.trim().parse::<u32>().ok())
    {
        Some(pid) if process_alive(pid) => LockReadiness::Busy,
        _ => LockReadiness::Ready,
    }
}

/// True when a process with this pid is currently alive (handle probe;
/// no signaling, no window interaction).
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(handle) => {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
                true
            }
            Err(_) => false,
        }
    }
}

/// Lock state for the external Default: probe every canonical data root.
/// Any live holder is `Busy`; any unreadable root without a live holder
/// is `Unknown`; otherwise `Ready`.
fn default_lock_state(provider: ProviderId) -> LockReadiness {
    let mut unknown: Option<String> = None;
    for root in canonical_default_dir_paths(provider) {
        match probe_singleton_lock(&root) {
            LockReadiness::Busy => return LockReadiness::Busy,
            LockReadiness::Unknown(reason) => {
                unknown.get_or_insert(reason);
            }
            LockReadiness::Ready => {}
        }
    }
    unknown
        .map(LockReadiness::Unknown)
        .unwrap_or(LockReadiness::Ready)
}

/// Lock-release verification after stop: only an explicit `Ready` counts;
/// `Busy` and `Unknown` both fail closed.
fn wait_verified(ctx: &AccountContext) -> bool {
    matches!(profile_lock_ready(ctx), Ok(LockReadiness::Ready))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Read-only process/window status for one account. `running` reflects live
/// PIDs only: hidden tray windows count as hidden, never as stopped.
pub fn account_status_for(ctx: &AccountContext) -> ManagerResult<AccountStatus> {
    account_status_in(ctx, &live_process_snapshot())
}

/// Status over an already-taken shared snapshot (widget lists all accounts
/// from one snapshot instead of snapshotting per account).
pub fn account_status_in(
    ctx: &AccountContext,
    snapshot: &Snapshot,
) -> ManagerResult<AccountStatus> {
    let unknown = !snapshot.attribution_complete;
    let processes = attribute_account_in(ctx, snapshot);
    let pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
    let pid_set: BTreeSet<u32> = pids.iter().copied().collect();
    let windows = if pid_set.is_empty() {
        Vec::new()
    } else {
        windows_for_pids(&pid_set)
    };
    let (visible, hidden) =
        windows.iter().fold(
            (0, 0),
            |(v, h), w| if w.visible { (v + 1, h) } else { (v, h + 1) },
        );
    let running = !unknown && !processes.is_empty();
    // Cosmetic only when actually running: never run discovery on a
    // per-status basis just to fill a path.
    let exe = processes.first().and_then(|p| process_image_path(p.pid));
    let mut notes = String::new();
    if unknown {
        notes = "Process attribution is incomplete (command-line probe unavailable); state unknown"
            .into();
    } else if processes.is_empty() && account_command_marker(ctx).unwrap_or(None).is_none() {
        notes = "External default profile: no attributed windows; service state is read from quiesce probes, not guessed".into();
    }
    Ok(AccountStatus {
        provider: ctx.provider,
        account_id: ctx.profile.id.clone(),
        name: ctx.profile.name.clone(),
        running,
        running_unknown: unknown,
        pids,
        visible_windows: visible,
        hidden_windows: hidden,
        exe,
        notes,
    })
}

/// Graceful/force stop outcome for one account. `attribution_unknown` is
/// set when the command-line probe was unavailable: `verified_idle` is
/// then always false and destructive callers must abort instead of
/// deleting (unknown is never "stopped").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopReport {
    pub provider: ProviderId,
    pub account_id: String,
    pub force: bool,
    pub signaled_windows: usize,
    pub stopped_pids: Vec<u32>,
    pub remaining_pids: Vec<u32>,
    pub verified_idle: bool,
    #[serde(default)]
    pub attribution_unknown: bool,
    pub notes: String,
}

/// Read-only process/window status for one account. `running` reflects live
/// attributed PIDs only; `running_unknown` is set when attribution was
/// incomplete (probe unavailable) — unknown is never "stopped".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountStatus {
    pub provider: ProviderId,
    pub account_id: String,
    pub name: String,
    pub running: bool,
    #[serde(default)]
    pub running_unknown: bool,
    pub pids: Vec<u32>,
    pub visible_windows: usize,
    pub hidden_windows: usize,
    pub exe: Option<PathBuf>,
    pub notes: String,
}

struct ComGuard;

impl ComGuard {
    fn acquire() -> ManagerResult<Self> {
        unsafe {
            // S_FALSE (already initialized with a compatible model) is fine.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

/// Build a `PROPVARIANT` string by hand: `SHStrDupW` allocates the
/// `VT_LPWSTR` buffer. The SDK `PROPVARIANT` implements `Drop`
/// (`PropVariantClear`), which frees that buffer exactly once when the
/// variant leaves scope, on success and on error alike. `SetValue` copies
/// the value, so the variant must never be freed by hand: an extra
/// `CoTaskMemFree` here double-frees the string inside
/// `PropVariantClearWorker` and corrupts the process heap.
fn set_store_string(store: &IPropertyStore, key: &PROPERTYKEY, value: &str) -> ManagerResult<()> {
    unsafe {
        let wide = wide_str(value);
        let duplicated: PWSTR = SHStrDupW(PCWSTR::from_raw(wide.as_ptr()))
            .map_err(|error| format!("Unable to build a property value: {error}"))?;
        let mut variant: PROPVARIANT = std::mem::zeroed();
        // `PROPVARIANT_0.Anonymous` is a `ManuallyDrop`: deref explicitly.
        (*variant.Anonymous.Anonymous).vt = VT_LPWSTR;
        (*variant.Anonymous.Anonymous).Anonymous.pwszVal = duplicated;
        let result = store.SetValue(key as *const PROPERTYKEY, &variant as *const PROPVARIANT);
        // `variant` owns `duplicated`; its `Drop`/`PropVariantClear` frees it.
        result.map_err(|error| format!("Unable to set the taskbar property: {error}"))?;
    }
    Ok(())
}

/// Stamp `System.AppUserModel.ID` on a shortcut file so Windows groups the
/// account's windows under one taskbar identity. `IPropertyStore` + commit:
/// no sidecar, no description hack.
fn stamp_shortcut_app_id(link: &Path, aumid: &str) -> ManagerResult<()> {
    let _com = ComGuard::acquire()?;
    unsafe {
        // Shell parsing names reject forward slashes even though Win32 file
        // I/O accepts them (E_INVALIDARG); preserve the original UTF-16 path.
        let path_w: Vec<u16> = link
            .as_os_str()
            .encode_wide()
            .map(|unit| {
                if unit == b'/' as u16 {
                    b'\\' as u16
                } else {
                    unit
                }
            })
            .chain(std::iter::once(0))
            .collect();
        let pbc: Option<&IBindCtx> = None;
        let store: IPropertyStore = SHGetPropertyStoreFromParsingName(
            PCWSTR::from_raw(path_w.as_ptr()),
            pbc,
            GPS_READWRITE,
        )
        .map_err(|error| format!("Unable to open the shortcut store: {error}"))?;
        let result = (|| {
            set_store_string(&store, &PKEY_AppUserModel_ID, aumid)?;
            store
                .Commit()
                .map_err(|error| format!("Unable to commit shortcut properties: {error}"))
        })();
        // `store` is ref-counted: dropping it releases, no manual Release.
        result
    }
}

fn store_string_matches(
    store: &IPropertyStore,
    key: &PROPERTYKEY,
    expected: &str,
) -> ManagerResult<bool> {
    unsafe {
        let value = store
            .GetValue(key)
            .map_err(|error| format!("Unable to read the taskbar property: {error}"))?;
        let inner = &*value.Anonymous.Anonymous;
        Ok(inner.vt == VT_LPWSTR
            && !inner.Anonymous.pwszVal.is_null()
            && inner
                .Anonymous
                .pwszVal
                .as_wide()
                .iter()
                .copied()
                .eq(expected.encode_utf16()))
    }
}

/// Bind a managed window to its existing account shortcut. Windows uses that
/// shortcut's icon and launch command; duplicated relaunch properties can
/// override it with stale data. Canonical vendor windows never use this path.
pub fn stamp_window_identity(hwnd: isize, aumid: &str) -> ManagerResult<()> {
    let _com = ComGuard::acquire()?;
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(HWND(hwnd as *mut _))
            .map_err(|error| format!("Unable to open the window store: {error}"))?;
        let owned = store_string_matches(&store, &PKEY_AppUserModel_ID, aumid)?;
        let empty: PROPVARIANT = std::mem::zeroed();
        let mut legacy = false;
        for key in [
            &PKEY_AppUserModel_RelaunchCommand,
            &PKEY_AppUserModel_RelaunchIconResource,
            &PKEY_AppUserModel_RelaunchDisplayNameResource,
        ] {
            let value = store
                .GetValue(key)
                .map_err(|error| format!("Unable to read account relaunch metadata: {error}"))?;
            if value.Anonymous.Anonymous.vt != VT_EMPTY {
                store.SetValue(key, &empty).map_err(|error| {
                    format!("Unable to clear account relaunch metadata: {error}")
                })?;
                legacy = true;
            }
        }
        if owned && !legacy {
            return Ok(());
        }
        if owned {
            // Retire Explorer's cached broken icon only for this owned group.
            store
                .SetValue(&PKEY_AppUserModel_ID, &empty)
                .map_err(|error| format!("Unable to refresh account icon identity: {error}"))?;
        }
        // Publish identity last, after the matching shortcut is ready.
        set_store_string(&store, &PKEY_AppUserModel_ID, aumid)
    }
}
/// Undo only the identity previously stamped by UsageHub on this Default.
/// An unrelated/vendor-owned window property is never changed.
fn restore_default_window_identity(hwnd: isize, ctx: &AccountContext) -> ManagerResult<bool> {
    let _com = ComGuard::acquire()?;
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(HWND(hwnd as *mut _))
            .map_err(|error| format!("Unable to open the Default window store: {error}"))?;
        let value = store
            .GetValue(&PKEY_AppUserModel_ID)
            .map_err(|error| format!("Unable to read the Default window identity: {error}"))?;
        let inner = &*value.Anonymous.Anonymous;
        if inner.vt != VT_LPWSTR || inner.Anonymous.pwszVal.is_null() {
            return Ok(false);
        }
        let current = inner
            .Anonymous
            .pwszVal
            .to_string()
            .map_err(|error| format!("Invalid Default window identity: {error}"))?;
        if current != account_app_user_model_id(ctx.provider, &ctx.profile.id) {
            return Ok(false);
        }
        let empty: PROPVARIANT = std::mem::zeroed();
        for key in [
            &PKEY_AppUserModel_RelaunchCommand,
            &PKEY_AppUserModel_RelaunchIconResource,
            &PKEY_AppUserModel_RelaunchDisplayNameResource,
        ] {
            store
                .SetValue(key, &empty)
                .map_err(|error| format!("Unable to clear Default relaunch metadata: {error}"))?;
        }
        let family = match ctx.provider {
            ProviderId::Claude => "Claude_pzs8sxrjxfjjc",
            ProviderId::Codex => "OpenAI.Codex_2p2nqsd0c76g0",
            _ => "",
        };
        if !registered_package_roots(family).is_empty() {
            set_store_string(
                &store,
                &PKEY_AppUserModel_ID,
                packaged_app_id(ctx.provider).unwrap(),
            )?;
        } else {
            store
                .SetValue(&PKEY_AppUserModel_ID, &empty)
                .map_err(|error| format!("Unable to restore Default window identity: {error}"))?;
        }
        Ok(true)
    }
}

/// Stamp every live window of `ctx` in `snapshot`. Returns successes;
/// refusals are skipped, never fatal. Cheap when unchanged: a per-process
/// `(hwnd -> pid + identity fingerprint)` cache skips windows that already
/// carry these exact props, so controller ticks only touch new HWNDs (or
/// windows whose identity changed, e.g. after a rename). Dead handles are
/// pruned on the way.
pub fn stamp_account_windows_in(ctx: &AccountContext, snapshot: &Snapshot) -> usize {
    if is_canonical_default(ctx) {
        let mut restored = 0;
        for window in account_live_windows_in(ctx, snapshot) {
            match restore_default_window_identity(window.hwnd, ctx) {
                Ok(true) => restored += 1,
                Ok(false) => {}
                Err(error) => {
                    crate::diagnose::log(format!("Default window identity repair failed: {error}"))
                }
            }
        }
        return restored;
    }
    let Ok(shortcut) = ensure_account_shortcut(ctx) else {
        return 0;
    };
    stamp_managed_account_windows_in(ctx, snapshot, &shortcut)
}

fn stamp_managed_account_windows_in(
    ctx: &AccountContext,
    snapshot: &Snapshot,
    shortcut: &ShortcutInfo,
) -> usize {
    let aumid = &shortcut.app_user_model_id;
    let fingerprint = format!(
        "{}|{}|{}",
        shortcut_fingerprint(ctx, &shortcut.target, &shortcut.arguments),
        aumid,
        shortcut.icon_path.display()
    );
    let windows = account_live_windows_in(ctx, snapshot);
    // Decide under the lock, stamp outside it: a hung cross-process COM
    // call must never block other accounts or the worker thread.
    let todo: Vec<LiveWindow> = {
        let Ok(cache) = STAMP_CACHE.lock() else {
            return 0;
        };
        windows
            .iter()
            .filter(|window| {
                cache
                    .get(&window.hwnd)
                    .is_none_or(|(pid, fp)| *pid != window.pid || fp != &fingerprint)
            })
            .cloned()
            .collect()
    };
    let mut stamped = 0usize;
    let mut done: Vec<(isize, u32)> = Vec::new();
    for window in &todo {
        match stamp_window_identity(window.hwnd, aumid) {
            Ok(()) => {
                done.push((window.hwnd, window.pid));
                stamped += 1;
            }
            Err(error) => crate::diagnose::log(format!("Account window identity failed: {error}")),
        }
    }
    if let Ok(mut cache) = STAMP_CACHE.lock() {
        for (hwnd, pid) in done {
            // Re-check the pid: an HWND value recycled mid-stamp must not
            // inherit the previous owner's fingerprint.
            let current = windows.iter().find(|w| w.hwnd == hwnd).map(|w| w.pid);
            if current == Some(pid) {
                cache.insert(hwnd, (pid, fingerprint.clone()));
            }
        }
        // Prune strictly to this account's live set: foreign HWNDs (other
        // accounts, recycled values) are never retained.
        let live: BTreeSet<isize> = windows.iter().map(|window| window.hwnd).collect();
        cache.retain(|hwnd, _| live.contains(hwnd));
    }
    stamped
}

/// Per-process stamped-window cache: `(hwnd -> owning pid + props)`.
static STAMP_CACHE: Mutex<BTreeMap<isize, (u32, String)>> = Mutex::new(BTreeMap::new());

/// Bounded post-launch window wait for the first open: `spawn_detached`
/// returns once the process spawns, which is before any HWND exists, so
/// this polls (off the caller's thread — the UI/CLI never blocks on it)
/// until the account's first window appears, then stamps props + icon.
/// Stamping is best-effort and never turns a good launch into an error;
/// only per-account windows are ever touched, never a global AUMID.
pub const OPEN_WINDOW_WAIT: Duration = Duration::from_secs(15);

/// Spawn the official app for `ctx`, wait (bounded) for its first window,
/// and stamp that window with the account identity. Returns the child pid.
///
/// The wait watches the FULL snapshot, not just post-spawn processes: a
/// forward-to-existing-instance launch exits immediately while the real
/// window already exists, and that must stamp at once instead of stalling.
/// If the spawned process is dead AND no account window exists, the wait
/// aborts early with an error instead of burning the whole budget.
/// Stamping is best-effort and never turns a good launch into an error.
pub fn launch_and_stamp_account(ctx: &AccountContext, extra_args: &[String]) -> ManagerResult<u32> {
    if is_canonical_default(ctx) {
        let family = match ctx.provider {
            ProviderId::Claude => "Claude_pzs8sxrjxfjjc",
            ProviderId::Codex => "OpenAI.Codex_2p2nqsd0c76g0",
            _ => return Err("This provider does not support account profiles".into()),
        };
        let roots = registered_package_roots(family);
        if !roots.is_empty() {
            if !extra_args.is_empty() {
                return Err(
                    "Extra arguments are not supported for packaged Default activation".into(),
                );
            }
            let snapshot = live_process_snapshot();
            let existing = account_live_windows_in(ctx, &snapshot);
            if let Some(window) = account_primary_window(&existing) {
                if focus_account_window(ctx)? {
                    return Ok(window.pid);
                }
                return Err("Account window changed before focus".into());
            }
            let pid = activate_packaged_default(ctx.provider)?;
            let start = std::time::Instant::now();
            loop {
                let snapshot = live_process_snapshot();
                let windows = account_live_windows_in(ctx, &snapshot);
                if let Some(window) = account_primary_window(&windows) {
                    stamp_account_windows_in(ctx, &snapshot);
                    return Ok(window.pid);
                }
                if start.elapsed() >= OPEN_WINDOW_WAIT {
                    return Err(format!(
                        "Activation of {} returned process {pid}, but no attributed Default window appeared within {} seconds",
                        packaged_app_id(ctx.provider).unwrap_or(family),
                        OPEN_WINDOW_WAIT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        if ctx.provider == ProviderId::Codex {
            return Err(format!(
                "Official Codex package {family} is not registered for the current user"
            ));
        }
    }
    let (exe_display, command) = prepare_launch_command(ctx, extra_args)?;
    let (pid, mut child) = spawn_detached(command, &exe_display)?;
    let start = std::time::Instant::now();
    loop {
        let snapshot = live_process_snapshot();
        if account_primary_window(&account_live_windows_in(ctx, &snapshot)).is_some() {
            stamp_account_windows_in(ctx, &snapshot);
            break;
        }
        let dead = child
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(false);
        if dead {
            return Err(format!(
                "Launched process {pid} exited without creating a window for account '{}'",
                ctx.profile.id
            ));
        }
        if start.elapsed() >= OPEN_WINDOW_WAIT {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(pid)
}

/// Legacy generated-icon path, retained only for scoped cleanup and existing
/// shortcut references. New account identities use the official app icon.
fn account_icon_path(provider: ProviderId, id: &str, color: &str) -> ManagerResult<PathBuf> {
    let root = paths::fork_config_dir().ok_or("Unable to resolve the config directory")?;
    let (red, green, blue) = icon_rgb(color, &format!("{provider:?}:{id}"));
    let tag = format!("{red:02x}{green:02x}{blue:02x}");
    let provider_key = match provider {
        ProviderId::Claude => "claude",
        ProviderId::Codex => "codex",
        _ => "account",
    };
    Ok(root.join("icons").join(format!(
        "{provider_key}-{}-{tag}.ico",
        sanitize_file_stem(id)?
    )))
}

/// Exact taskbar-artifact paths owned by one account, for deletion preview:
/// the id-keyed Start Menu `.lnk` plus the id-bound generated icon for the
/// account's current color tag — and nothing else. Read-only: no ensure,
/// create, rewrite, or directory enumeration of any kind (user-pinned
/// links are never scanned). Only existing regular files under the owned
/// roots are returned; the file NAME is the immutable id, so pins survive
/// renames and `account_1` never matches `account_10` (exact file names,
/// never substrings). A link/reparse at either exact path, or a path
/// outside its owned root, is refused as an error (fail-closed) instead
/// of being silently skipped or followed. External profiles have no owned
/// artifacts: cleanup keeps their launchers via the surviving registry
/// entry, so this returns an empty list for them.
pub fn owned_account_artifact_paths(ctx: &AccountContext) -> ManagerResult<Vec<PathBuf>> {
    if ctx.ownership != crate::accounts::AccountOwnership::Managed {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let dir = shortcuts_root()?;
    let lnk = dir.join(shortcut_file_name(ctx.provider, &ctx.profile));
    if !paths::is_within(&dir, &lnk) {
        return Err(format!(
            "Refusing artifact outside the shortcuts root: '{}'",
            lnk.display()
        ));
    }
    if lnk.is_file() {
        if paths::is_reparse_point(&lnk) {
            return Err(format!(
                "Refusing unexpected link at shortcut path: '{}'",
                lnk.display()
            ));
        }
        out.push(lnk);
    }
    let icons = paths::fork_config_dir()
        .ok_or("Unable to resolve the config directory")?
        .join("icons");
    let icon = account_icon_path(ctx.provider, &ctx.profile.id, &ctx.profile.color)?;
    if !paths::is_within(&icons, &icon) {
        return Err(format!(
            "Refusing artifact outside the icons root: '{}'",
            icon.display()
        ));
    }
    if icon.is_file() {
        if paths::is_reparse_point(&icon) {
            return Err(format!(
                "Refusing unexpected link at icon path: '{}'",
                icon.display()
            ));
        }
        out.push(icon);
    }
    Ok(out)
}

fn sanitize_file_stem(id: &str) -> ManagerResult<String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!("Unsafe account id '{id}'"));
    }
    Ok(id.to_string())
}

fn icon_rgb(color: &str, fallback_seed: &str) -> (u8, u8, u8) {
    if let Some(hex) = color.trim().strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(red), Ok(green), Ok(blue)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return (red, green, blue);
            }
        }
    }
    const FALLBACK: [(u8, u8, u8); 8] = [
        (76, 141, 211),
        (94, 168, 123),
        (196, 138, 74),
        (168, 110, 190),
        (208, 104, 104),
        (84, 170, 178),
        (150, 150, 150),
        (120, 140, 200),
    ];
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in fallback_seed.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    FALLBACK[hash as usize % FALLBACK.len()]
}

/// Stable shortcut file name: the immutable account id, never the display
/// name. Renames must not move the file, or the taskbar pin breaks; the
/// display name lives in the description only.
fn shortcut_file_name(provider: ProviderId, profile: &AccountProfile) -> String {
    let provider_key = match provider {
        ProviderId::Claude => "Claude",
        ProviderId::Codex => "Codex",
        _ => "Account",
    };
    format!("UsageHub {provider_key} - {}.lnk", profile.id)
}

/// Wrapper launch through the current UsageHub exe so EVERY launch —
/// Start Menu shortcut, taskbar jump-list relaunch, or CLI — runs under
/// `lifecycle` with the explicit child environment (`CLAUDE_CONFIG_DIR`,
/// `TEMP`/`TMP`, nonessential-traffic opt-outs). Launching the official
/// exe directly from a shortcut would lose that env and reuse global CLI
/// history/auth. The official exe is still what ultimately runs; this
/// worker only resolves it, spawns it, stamps it, and exits — no resident
/// process is created.
///
/// `--from-shortcut` marks GUI-surface launches (taskbar/Start Menu) whose
/// stderr is lost: the `--manage open` handler must surface failures to
/// the user (dialog) instead of JSON-only stderr. Coordinated with the
/// CLI slice; see [`show_error_dialog`].
pub fn manager_launch_command(ctx: &AccountContext) -> ManagerResult<(PathBuf, String)> {
    let wrapper = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the UsageHub executable: {error}"))?;
    let key = ctx.provider.descriptor().key;
    Ok((
        wrapper,
        format!(
            "--manage open --provider {key} --id {} --from-shortcut",
            ctx.profile.id
        ),
    ))
}

/// Blocking error dialog for GUI-surface launches whose stderr is lost
/// (shortcuts, jump lists). The CLI `--manage open --from-shortcut` path
/// calls this on failure IN ADDITION to the JSON stderr envelope, so the
/// exit code contract is unchanged and nothing is silently swallowed.
pub fn show_error_dialog(title: &str, message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    unsafe {
        let title_w = wide_str(title);
        let message_w = wide_str(message);
        let _ = MessageBoxW(
            None,
            PCWSTR::from_raw(message_w.as_ptr()),
            PCWSTR::from_raw(title_w.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Shortcuts root: `USAGEHUB_SHORTCUTS_ROOT` when set (absolute only;
/// smoke/portable isolation), otherwise `%Start Menu%\\Programs\\UsageHub`.
/// In `cfg(test)` the per-test-thread fixture wins and env is ignored.
pub fn shortcuts_root() -> ManagerResult<PathBuf> {
    #[cfg(test)]
    {
        return Ok(crate::app_settings::app_data_directory().join("shortcuts"));
    }
    #[cfg(not(test))]
    {
        if let Some(value) = std::env::var_os(SHORTCUTS_ROOT_ENV) {
            if value.is_empty() {
                return Err(format!("{SHORTCUTS_ROOT_ENV} must not be empty"));
            }
            let root = PathBuf::from(value);
            if !root.is_absolute() {
                return Err(format!("{SHORTCUTS_ROOT_ENV} must be an absolute path"));
            }
            return Ok(root);
        }
        crate::known_folders::roaming_app_data_dir()
            .map(|roaming| {
                roaming
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs")
                    .join("UsageHub")
            })
            .ok_or_else(|| "Unable to locate the Start Menu".to_string())
    }
}

/// Metadata for one account's taskbar identity (Start Menu shortcut,
/// stable icon, AppUserModelID stamp). Returned by [`ensure_account_shortcut`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShortcutInfo {
    pub provider: ProviderId,
    pub account_id: String,
    pub name: String,
    pub shortcut_path: PathBuf,
    pub target: PathBuf,
    pub arguments: String,
    pub app_user_model_id: String,
    pub icon_path: PathBuf,
}

/// Ensure the Start Menu shortcut for one account and return its metadata.
///
/// The shortcut targets the current UsageHub exe with `--manage open`
/// (see [`manager_launch_command`]) — never the official exe directly —
/// so taskbar/Start Menu launches run under `lifecycle` with the explicit
/// child environment instead of losing it and reusing global CLI
/// history/auth. Isolated accounts carry an id-keyed icon and AppUserModelID;
/// canonical Default uses the official executable icon and package identity.
/// The file name is the immutable id, so pins survive renames.
///
/// Cheap when unchanged: a per-process fingerprint of the profile metadata
/// (name, color, dirs, wrapper exe, arguments) plus shortcut/icon presence
/// is cached, so a controller tick that changed nothing performs no `.lnk`
/// rewrite and no COM commit — only existence checks. Any metadata change
/// (rename, color, new exe path after an update) re-writes and re-stamps.
pub fn ensure_account_shortcut(ctx: &AccountContext) -> ManagerResult<ShortcutInfo> {
    let canonical = is_canonical_default(ctx);
    let aumid = if canonical {
        packaged_app_id(ctx.provider).unwrap_or("").to_string()
    } else {
        account_app_user_model_id(ctx.provider, &ctx.profile.id)
    };
    let icon = official_app_icon(ctx.provider)?;
    let (exe, arguments) = manager_launch_command(ctx)?;
    let dir = shortcuts_root()?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("Unable to create the shortcuts folder: {error}"))?;
    let path = dir.join(shortcut_file_name(ctx.provider, &ctx.profile));
    let fingerprint = format!(
        "{}|{}|{}",
        shortcut_fingerprint(ctx, &exe, &arguments),
        aumid,
        icon.display()
    );
    let cache_key = (ctx.provider, ctx.profile.id.clone());
    let unchanged = path.is_file()
        && icon.is_file()
        && SHORTCUT_CACHE
            .lock()
            .map(|cache| cache.get(&cache_key).is_some_and(|fp| fp == &fingerprint))
            .unwrap_or(false);
    if !unchanged {
        // Discovery runs only when a write is actually needed: it walks
        // the registry/PATH/Packages, so it must not run on every
        // controller tick. It still validates the official app is
        // installed before any `.lnk` is (re)written.
        let _official = discover_official_exe(ctx.provider)?;
        write_shortcut(
            &path,
            &exe,
            &arguments,
            &format!("{} [{aumid}]", ctx.profile.name),
            &icon,
        )?;
        // Real COM stamp: the only grouping signal Windows honors.
        stamp_shortcut_app_id(&path, &aumid)?;
        if let Ok(mut cache) = SHORTCUT_CACHE.lock() {
            cache.insert(cache_key, fingerprint);
        }
    }
    // Drop any obsolete sidecar left by the pre-`IPropertyStore` wiring.
    std::fs::remove_file(path.with_extension("aumid")).ok();
    Ok(ShortcutInfo {
        provider: ctx.provider,
        account_id: ctx.profile.id.clone(),
        name: ctx.profile.name.clone(),
        shortcut_path: path,
        target: exe,
        arguments,
        app_user_model_id: aumid,
        icon_path: icon,
    })
}

/// Per-process shortcut fingerprints: identical profile metadata skips
/// `.lnk` rewrites and COM commits on every controller tick.
static SHORTCUT_CACHE: Mutex<BTreeMap<(ProviderId, String), String>> = Mutex::new(BTreeMap::new());

/// Fingerprint of everything the shortcut embeds: rename/color/dir/exe
/// changes all cut over to a rewrite + re-stamp.
fn shortcut_fingerprint(ctx: &AccountContext, exe: &Path, arguments: &str) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        ctx.profile.name,
        ctx.profile.color,
        ctx.profile.desktop_dir,
        ctx.profile.config_dir,
        exe.display(),
        arguments
    )
}

fn write_shortcut(
    path: &Path,
    target: &Path,
    arguments: &str,
    aumid: &str,
    icon: &Path,
) -> ManagerResult<()> {
    let _com = ComGuard::acquire()?;
    // Bind every wide string before the raw pointers are taken: the `PCWSTR`
    // borrows must outlive the COM calls below.
    let target_w = wide_str(&target.to_string_lossy());
    let args_w = wide_str(arguments);
    let work_w = wide_str(
        &target
            .parent()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_default(),
    );
    let desc_w = wide_str(&format!("UsageHub managed account [{aumid}]"));
    let icon_w = wide_str(&icon.to_string_lossy());
    let path_w = wide_str(&path.to_string_lossy());
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("Unable to create a shell link: {error}"))?;
        link.SetPath(PCWSTR::from_raw(target_w.as_ptr()))
            .map_err(|error| format!("Unable to set the shortcut target: {error}"))?;
        link.SetArguments(PCWSTR::from_raw(args_w.as_ptr()))
            .map_err(|error| format!("Unable to set shortcut arguments: {error}"))?;
        if work_w.len() > 1 {
            link.SetWorkingDirectory(PCWSTR::from_raw(work_w.as_ptr()))
                .map_err(|error| format!("Unable to set the working directory: {error}"))?;
        }
        link.SetDescription(PCWSTR::from_raw(desc_w.as_ptr()))
            .map_err(|error| format!("Unable to set the shortcut description: {error}"))?;
        link.SetIconLocation(PCWSTR::from_raw(icon_w.as_ptr()), 0)
            .map_err(|error| format!("Unable to set the shortcut icon: {error}"))?;
        let persist: IPersistFile = link
            .cast()
            .map_err(|error| format!("Unable to persist the shortcut: {error}"))?;
        persist
            .Save(PCWSTR::from_raw(path_w.as_ptr()), true.into())
            .map_err(|error| format!("Unable to save '{}': {error}", path.display()))?;
    }
    Ok(())
}

/// Reconcile every known account's taskbar identity: ensure shortcuts+icons,
/// stamp live windows from ONE shared snapshot, prune orphan `.lnk`/icon
/// files plus obsolete `.aumid` sidecars. Designed for the controller widget
/// worker thread: the registry load is read-only and no guards are taken.
/// Returns `(ensured, pruned)`.
pub fn refresh_taskbar_identities() -> ManagerResult<(usize, usize)> {
    let settings = state::load_fresh();
    let snapshot = live_process_snapshot();
    let mut expected_lnk: BTreeSet<PathBuf> = BTreeSet::new();
    let mut expected_icons: BTreeSet<PathBuf> = BTreeSet::new();
    let mut ensured = 0usize;
    let mut pruned = 0usize;
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let Some(accounts) = settings.accounts.get(provider) else {
            continue;
        };
        for profile in &accounts.profiles {
            let ctx = match AccountContext::from_profile(provider, profile) {
                Ok(ctx) => ctx,
                Err(_) => continue,
            };
            let shortcut = ensure_account_shortcut(&ctx);
            if shortcut.is_ok() {
                ensured += 1;
            }
            if profile.enabled {
                if is_canonical_default(&ctx) {
                    stamp_account_windows_in(&ctx, &snapshot);
                } else if let Ok(shortcut) = &shortcut {
                    stamp_managed_account_windows_in(&ctx, &snapshot, shortcut);
                }
            }
            // Track expected files without re-deriving names elsewhere.
            if let Ok(dir) = shortcuts_root() {
                expected_lnk.insert(dir.join(shortcut_file_name(provider, profile)));
            }
            if !is_canonical_default(&ctx) {
                if let Ok(icon) = account_icon_path(provider, &profile.id, &profile.color) {
                    expected_icons.insert(icon);
                }
            }
        }
    }
    // Bound per-process memory: drop fingerprints for retired ids so a
    // rename/delete cycle cannot grow the cache without limit.
    if let Ok(mut cache) = SHORTCUT_CACHE.lock() {
        cache.retain(|(provider, id), _| {
            settings
                .accounts
                .get(*provider)
                .is_some_and(|accounts| accounts.find(id).is_some())
        });
    }
    if let Ok(dir) = shortcuts_root() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let obsolete_sidecar = path.extension().is_some_and(|ext| ext == "aumid");
                let orphan_link = path.extension().is_some_and(|ext| ext == "lnk")
                    && !expected_lnk.contains(&path);
                if (obsolete_sidecar || orphan_link) && std::fs::remove_file(&path).is_ok() {
                    pruned += 1;
                }
            }
        }
    }
    if let Some(root) = paths::fork_config_dir() {
        let icons = root.join("icons");
        if let Ok(entries) = std::fs::read_dir(&icons) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "ico")
                    && !expected_icons.contains(&path)
                    && std::fs::remove_file(&path).is_ok()
                {
                    pruned += 1;
                }
            }
        }
    }
    Ok((ensured, pruned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(hwnd: isize, visible: bool, style: u32, ex_style: u32) -> WindowCandidate {
        WindowCandidate {
            hwnd,
            pid: 1,
            owner: 0,
            visible,
            style,
            ex_style,
            has_area: true,
        }
    }

    #[test]
    fn primary_selection_prefers_real_windows_over_visible_companions() {
        let tool = candidate(1, true, 0x14000000, 0x200088);
        let hidden_primary = candidate(2, false, 0x04c70000, 0x240100);
        assert_eq!(primary_window(&[tool, hidden_primary]).unwrap().hwnd, 2);

        // Claude's ordinary window has no APPWINDOW bit.
        let visible_primary = candidate(3, true, 0x14c70000, 0x280100);
        assert_eq!(
            primary_window(&[tool, hidden_primary, visible_primary])
                .unwrap()
                .hwnd,
            3
        );
        assert_eq!(
            primary_window(&[visible_primary, hidden_primary])
                .unwrap()
                .hwnd,
            3
        );
    }

    #[test]
    fn primary_selection_rejects_helpers_and_respects_modal_ownership() {
        let mut primary = candidate(1, true, WS_CAPTION.0, 0);
        let mut dialog = candidate(2, true, WS_CAPTION.0, 0);
        dialog.owner = primary.hwnd;
        let mut zero_area = candidate(3, true, WS_CAPTION.0, 0);
        zero_area.has_area = false;
        let helper = candidate(4, true, 0, 0);
        let no_activate = candidate(5, true, WS_CAPTION.0, WS_EX_NOACTIVATE.0);
        let child = candidate(6, true, WS_CAPTION.0 | WS_CHILD.0, 0);
        assert!(primary_window(&[dialog, zero_area, helper, no_activate, child]).is_none());
        assert_eq!(primary_window(&[dialog, primary]).unwrap().hwnd, 1);
        primary.style |= WS_DISABLED.0;
        assert_eq!(primary_window(&[dialog, primary]).unwrap().hwnd, 2);
        let mut nested = candidate(7, true, WS_CAPTION.0, 0);
        nested.owner = dialog.hwnd;
        dialog.style |= WS_DISABLED.0;
        assert_eq!(primary_window(&[dialog, primary, nested]).unwrap().hwnd, 7);
        // A missing or ineligible intermediate cannot establish attribution.
        assert!(primary_window(&[primary, nested]).is_none());
        dialog.ex_style |= WS_EX_TOOLWINDOW.0;
        assert!(primary_window(&[dialog, primary, nested]).is_none());
        dialog.ex_style = 0;
        dialog.visible = false;
        assert!(primary_window(&[dialog, primary, nested]).is_none());
        dialog.visible = true;
        // A stale cyclic chain that does not reach the primary is rejected.
        let mut cycle = candidate(8, true, WS_CAPTION.0 | WS_DISABLED.0, 0);
        dialog.owner = cycle.hwnd;
        cycle.owner = dialog.hwnd;
        assert!(primary_window(&[dialog, primary, nested, cycle]).is_none());
    }

    #[test]
    fn shortcut_fingerprint_tracks_identity_inputs() {
        let profile = AccountProfile {
            id: "account_1".into(),
            name: "Work".into(),
            color: "#112233".into(),
            ..AccountProfile::default()
        };
        let ctx =
            AccountContext::from_profile(ProviderId::Claude, &profile).expect("profile context");
        let exe = PathBuf::from("C:\\Apps\\Claude.exe");
        let args = "--user-data-dir=\"C:\\x\"";
        let base = shortcut_fingerprint(&ctx, &exe, args);
        // Rename and recolor both cut over to a rewrite + re-stamp.
        for mutated in [
            AccountProfile {
                name: "Renamed".into(),
                ..profile.clone()
            },
            AccountProfile {
                color: "#445566".into(),
                ..profile.clone()
            },
        ] {
            let other = AccountContext::from_profile(ProviderId::Claude, &mutated)
                .expect("mutated context");
            assert_ne!(base, shortcut_fingerprint(&other, &exe, args));
        }
        // New exe path (post-update) cuts over; identical input is stable.
        assert_ne!(
            base,
            shortcut_fingerprint(&ctx, &PathBuf::from("C:\\Apps\\Claude2.exe"), args)
        );
        assert_eq!(base, shortcut_fingerprint(&ctx, &exe, args));
    }

    #[test]
    fn command_line_ownership_respects_id_boundaries() {
        let marker = "c:\\usagehub\\profiles\\claude\\account_1";
        assert!(command_line_owns(
            "\"C:\\Program Files\\Claude\\Claude.exe\" --user-data-dir=\"C:\\UsageHub\\profiles\\claude\\account_1\"",
            marker
        ));
        // Sibling account_10 must NOT attribute to account_1.
        assert!(!command_line_owns(
            "--user-data-dir=\"C:\\UsageHub\\profiles\\claude\\account_10\"",
            marker
        ));
        assert!(!command_line_owns(
            "--user-data-dir=\"C:\\other\\place\"",
            marker
        ));
        // Unquoted trailing separator still owns.
        assert!(command_line_owns(
            "--user-data-dir=C:\\UsageHub\\profiles\\claude\\account_1\\",
            marker
        ));
    }

    #[test]
    fn app_user_model_ids_are_stable_and_safe() {
        let first = account_app_user_model_id(ProviderId::Claude, "account_1");
        let second = account_app_user_model_id(ProviderId::Claude, "account_1");
        assert_eq!(first, second);
        assert!(first.starts_with("CodeZeno.UsageHub.Claude."));
        assert_ne!(
            first,
            account_app_user_model_id(ProviderId::Codex, "account_1")
        );
        assert_eq!(
            account_app_user_model_id(ProviderId::Claude, "a/b c"),
            "CodeZeno.UsageHub.Claude.a_b_c"
        );
    }

    #[test]
    fn command_line_json_parses_object_and_array() {
        let single = r#"{"ProcessId": 42, "CommandLine": "claude --user-data-dir=x"}"#;
        assert_eq!(parse_command_lines(single)[&42], "claude --user-data-dir=x");
        assert!(parse_command_lines("").is_empty());
        assert!(parse_command_lines("not json").is_empty());
    }

    #[test]
    fn attribution_needs_a_marker_and_never_guesses() {
        let snapshot = vec![LiveProcess {
            pid: 1234,
            provider: ProviderId::Claude,
            image: "Claude.exe".into(),
            packaged_codex: false,
            command_line: Some("claude.exe --user-data-dir=\"C:\\x\\account_1\"".into()),
            created_unix: None,
            parent_pid: 0,
        }];
        let profile = AccountProfile {
            id: "default".into(),
            ..AccountProfile::default()
        };
        // Marker-scoped attribution never fires without a marker.
        let ctx =
            AccountContext::from_profile(ProviderId::Claude, &profile).expect("default context");
        assert!(attribute_snapshot(&ctx, &snapshot).is_empty());
        // And the full Default arm refuses a command line carrying
        // another account's marker.
        let full = Snapshot {
            processes: snapshot,
            attribution_complete: true,
        };
        assert!(attribute_account_in(&ctx, &full).is_empty());
    }

    fn live_process(pid: u32, command_line: Option<&str>, parent_pid: u32) -> LiveProcess {
        LiveProcess {
            pid,
            provider: ProviderId::Claude,
            image: "Claude.exe".into(),
            packaged_codex: false,
            command_line: command_line.map(str::to_string),
            created_unix: Some(1_700_000_000 + pid as u64),
            parent_pid,
        }
    }

    fn default_ctx() -> AccountContext {
        let profile = AccountProfile {
            id: "default".into(),
            ..AccountProfile::default()
        };
        AccountContext::from_profile(ProviderId::Claude, &profile).expect("default context")
    }

    #[test]
    fn default_arm_owns_switchless_launches_only() {
        let ctx = default_ctx();
        // Switch-less official launch: owned by the external Default.
        let plain = Snapshot {
            processes: vec![live_process(101, Some("C:\\Apps\\Claude.exe"), 1)],
            attribution_complete: true,
        };
        assert_eq!(
            attribute_account_in(&ctx, &plain)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            vec![101]
        );
        // A command line carrying another account's marker is excluded,
        // even without a switch of its own shape.
        let foreign = Snapshot {
            processes: vec![
                live_process(
                    102,
                    Some("claude.exe --user-data-dir=\"C:\\x\\account_9\""),
                    1,
                ),
                live_process(103, Some("C:\\Apps\\Claude.exe"), 1),
            ],
            attribution_complete: true,
        };
        assert_eq!(
            attribute_account_in(&ctx, &foreign)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            vec![103]
        );
        // No command line at all is unknown, never Default-owned.
        let blind = Snapshot {
            processes: vec![live_process(104, None, 1)],
            attribution_complete: false,
        };
        assert!(attribute_account_in(&ctx, &blind).is_empty());

        // A canonical Desktop binding must not lose an existing vendor
        // launch; a custom External mapping must never claim that launch.
        struct RestoreRoots(Option<Vec<PathBuf>>);
        impl Drop for RestoreRoots {
            fn drop(&mut self) {
                TEST_DEFAULT_ROOTS.with(|slot| *slot.borrow_mut() = self.0.take());
            }
        }
        let desktop = PathBuf::from("C:\\UsageHub-native-test\\default");
        let cli = PathBuf::from("C:\\UsageHub-native-test\\cli");
        let previous =
            TEST_DEFAULT_ROOTS.with(|slot| slot.replace(Some(vec![desktop.clone(), cli.clone()])));
        let _restore = RestoreRoots(previous);
        let mut profile = ctx.profile.clone();
        profile.desktop_dir = desktop.display().to_string();
        profile.config_dir = cli.display().to_string();
        let mapped = AccountContext::from_profile(ProviderId::Claude, &profile).unwrap();
        assert!(is_canonical_default(&mapped));
        assert!(account_child_env(&mapped)
            .unwrap()
            .iter()
            .any(|(key, value)| key == "CLAUDE_CONFIG_DIR"
                && value.as_str() == cli.to_string_lossy().as_ref()));
        assert_eq!(
            attribute_account_in(&mapped, &plain)
                .iter()
                .map(|process| process.pid)
                .collect::<Vec<_>>(),
            vec![101]
        );
        profile.desktop_dir = "C:\\UsageHub-native-test\\custom".into();
        let custom = AccountContext::from_profile(ProviderId::Claude, &profile).unwrap();
        assert!(!is_canonical_default(&custom));
        assert!(account_child_env(&custom)
            .unwrap()
            .iter()
            .any(|(key, _)| key == "CLAUDE_CONFIG_DIR"));
        let mut inconsistent = ctx.clone();
        inconsistent.config_dir = Some(PathBuf::from("C:\\UsageHub-native-test\\custom"));
        assert!(!is_canonical_default(&inconsistent));
        let mut config_only = ctx.profile.clone();
        config_only.config_dir = "C:\\UsageHub-native-test\\custom".into();
        let config_only = AccountContext::from_profile(ProviderId::Claude, &config_only).unwrap();
        assert!(!is_canonical_default(&config_only));
        assert!(attribute_account_in(&custom, &plain).is_empty());
    }

    #[test]
    fn codex_default_does_not_claim_cli_processes() {
        let profile = AccountProfile {
            id: "default".into(),
            ..AccountProfile::default()
        };
        let ctx = AccountContext::from_profile(ProviderId::Codex, &profile).unwrap();
        let mut cli = live_process(1, Some("codex.exe"), 0);
        cli.provider = ProviderId::Codex;
        cli.image = "codex.exe".into();
        let mut desktop = live_process(2, Some("ChatGPT.exe"), 0);
        desktop.provider = ProviderId::Codex;
        desktop.image = "ChatGPT.exe".into();
        desktop.packaged_codex = true;
        let mut package_window = live_process(3, Some("Codex.exe"), 0);
        package_window.provider = ProviderId::Codex;
        package_window.image = "Codex.exe".into();
        package_window.packaged_codex = true;
        let snapshot = Snapshot {
            processes: vec![cli, desktop, package_window],
            attribution_complete: true,
        };
        assert_eq!(
            attribute_account_in(&ctx, &snapshot)
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn codex_package_image_requires_exact_registered_path() {
        let roots = vec![PathBuf::from(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1",
        )];
        assert!(is_packaged_codex_image(
            Path::new(r"C:\Program Files\WindowsApps\OpenAI.Codex_1\app\Codex.exe"),
            &roots,
        ));
        assert!(is_packaged_codex_image(
            Path::new(r"C:\Program Files\WindowsApps\OpenAI.Codex_1\app\ChatGPT.exe"),
            &roots,
        ));
        assert!(!is_packaged_codex_image(
            Path::new(r"C:\tools\codex.exe"),
            &roots,
        ));
        assert!(!is_packaged_codex_image(
            Path::new(r"C:\Program Files\WindowsApps\OpenAI.Codex_1_other\app\Codex.exe"),
            &roots,
        ));
    }

    #[test]
    fn children_follow_their_marker_parent() {
        let marker = "c:\\usagehub\\profiles\\claude\\account_1";
        let snapshot = Snapshot {
            processes: vec![
                live_process(
                    201,
                    Some(&format!("claude.exe --user-data-dir=\"{marker}\"")),
                    1,
                ),
                live_process(202, Some("claude.exe --type=renderer"), 201),
                live_process(203, Some("claude.exe --type=gpu-process"), 202),
                live_process(204, Some("C:\\Apps\\Claude.exe"), 1),
            ],
            attribution_complete: true,
        };
        let mut profile = AccountProfile::default();
        profile.id = "account_1".into();
        profile.desktop_dir = "C:\\UsageHub\\profiles\\claude\\account_1".into();
        profile.ownership = crate::accounts::AccountOwnership::Managed;
        let ctx =
            AccountContext::from_profile(ProviderId::Claude, &profile).expect("managed context");
        let mut pids: Vec<u32> = attribute_account_in(&ctx, &snapshot)
            .iter()
            .map(|p| p.pid)
            .collect();
        pids.sort_unstable();
        // Browser plus its transitive renderer children; the switch-less
        // sibling belongs to the external Default arm, not this account.
        assert_eq!(pids, vec![201, 202, 203]);
    }

    #[test]
    fn unknown_attribution_is_never_reported_running() {
        let ctx = default_ctx();
        let snapshot = Snapshot {
            processes: vec![live_process(301, None, 1)],
            attribution_complete: false,
        };
        let status = account_status_in(&ctx, &snapshot).expect("status");
        assert!(!status.running);
        assert!(status.running_unknown);
        assert!(status.pids.is_empty());
    }

    #[test]
    fn shortcut_filename_identity_survives_display_rename() {
        let profile = AccountProfile {
            id: "account_7".into(),
            name: "Work Machines".into(),
            ..AccountProfile::default()
        };
        // File name is id-keyed (no display-name collisions, no UA-name
        // mangling); renames keep the same file, name only shows in UI.
        let before = shortcut_file_name(ProviderId::Claude, &profile);
        assert!(before.contains("account_7"));
        let renamed = AccountProfile {
            name: "Renamed / Odd".into(),
            ..profile.clone()
        };
        assert_eq!(before, shortcut_file_name(ProviderId::Claude, &renamed));
        assert_ne!(
            before,
            shortcut_file_name(ProviderId::Codex, &profile),
            "provider stays distinguished"
        );
    }

    fn managed_ctx(id: &str, ownership: crate::accounts::AccountOwnership) -> AccountContext {
        let mut profile = AccountProfile::default();
        profile.id = id.into();
        profile.name = format!("Name {id}");
        profile.ownership = ownership;
        // Explicit isolated roots keep this hermetic (no registry reads).
        profile.desktop_dir = format!("C:\\UsageHub\\fake\\{id}");
        AccountContext::from_profile(ProviderId::Claude, &profile).expect("managed context")
    }

    #[test]
    fn artifact_paths_are_exact_id_scoped_and_read_only() {
        let dir = shortcuts_root().expect("shortcuts root");
        std::fs::create_dir_all(&dir).unwrap();
        let ctx1 = managed_ctx("account_1", crate::accounts::AccountOwnership::Managed);
        let before = owned_account_artifact_paths(&ctx1).expect("list");
        // Nothing created by the listing itself.
        assert!(before.is_empty());
        assert!(!dir
            .join(shortcut_file_name(ProviderId::Claude, &ctx1.profile))
            .exists());
        // Sibling account_10's launcher must never match account_1
        // (exact file names, never substrings).
        let ctx10 = managed_ctx("account_10", crate::accounts::AccountOwnership::Managed);
        let p10 = dir.join(shortcut_file_name(ProviderId::Claude, &ctx10.profile));
        std::fs::write(&p10, "dummy").unwrap();
        assert!(owned_account_artifact_paths(&ctx1)
            .expect("list")
            .is_empty());
        assert_eq!(
            owned_account_artifact_paths(&ctx10).expect("list"),
            vec![p10.clone()]
        );
        // account_1's own launcher plus its current-tag icon are listed.
        let p1 = dir.join(shortcut_file_name(ProviderId::Claude, &ctx1.profile));
        std::fs::write(&p1, "dummy").unwrap();
        let icon = account_icon_path(ProviderId::Claude, "account_1", "").expect("icon path");
        std::fs::create_dir_all(icon.parent().unwrap()).unwrap();
        std::fs::write(&icon, "dummy").unwrap();
        let mut got = owned_account_artifact_paths(&ctx1).expect("list");
        got.sort();
        let mut want = vec![p1.clone(), icon.clone()];
        want.sort();
        assert_eq!(got, want);
        std::fs::remove_file(&p1).ok();
        std::fs::remove_file(&icon).ok();
        std::fs::remove_file(&p10).ok();
        // External profiles own no artifacts: launchers survive via the
        // surviving registry entry.
        let ext = managed_ctx("default", crate::accounts::AccountOwnership::External);
        assert!(owned_account_artifact_paths(&ext).expect("list").is_empty());
    }
}
