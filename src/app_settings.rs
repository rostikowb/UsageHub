//! Shared, atomically persisted state used by the widget and studio processes.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use crate::models::{AppUsageData, CodexCreditsState};
use crate::providers::{ProviderId, ProviderSet};

pub const POLL_1_MIN_SECONDS: u32 = 60;
pub const POLL_5_MIN_SECONDS: u32 = 300;
pub const POLL_15_MIN_SECONDS: u32 = 900;
pub const POLL_1_HOUR_SECONDS: u32 = 3_600;
pub const POLL_1_MIN: u32 = POLL_1_MIN_SECONDS * 1_000;
pub const POLL_5_MIN: u32 = POLL_5_MIN_SECONDS * 1_000;
pub const POLL_15_MIN: u32 = POLL_15_MIN_SECONDS * 1_000;
pub const POLL_1_HOUR: u32 = POLL_1_HOUR_SECONDS * 1_000;
// SetTimer clamps longer intervals to USER_TIMER_MAXIMUM (i32::MAX ms).
pub const MAX_POLL_MINUTES: u32 = i32::MAX as u32 / POLL_1_MIN;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettingsFile {
    #[serde(default)]
    pub accounts: crate::accounts::AccountSettings,
    #[serde(default, skip_serializing)]
    pub tray_offset: i32,
    #[serde(default, skip_serializing)]
    pub taskbar_index: usize,
    /// True only when the settings file still contains the pre-theme placement
    /// fields. While this remains true, ordinary settings saves preserve those
    /// fields so only the startup migration can consume them.
    #[serde(skip)]
    pub legacy_placement_pending: bool,
    #[serde(default = "default_true", skip_serializing)]
    pub widget_visible: bool,
    /// True only while the pre-theme `widget_visible` value still needs to be
    /// transferred to the main root's Render expression.
    #[serde(skip)]
    pub legacy_visibility_pending: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_update_check_unix: Option<u64>,
    #[serde(default = "default_true")]
    show_claude_code: bool,
    #[serde(default)]
    show_codex: bool,
    #[serde(default)]
    show_antigravity: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_antigravity_gemini: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_antigravity_external: Option<bool>,
    #[serde(default)]
    show_opencode: bool,
    #[serde(default)]
    show_cursor: bool,
    #[serde(default)]
    show_grok: bool,
    #[serde(default)]
    show_copilot: bool,
    #[serde(default = "default_true")]
    pub custom_theme_enabled: bool,
    /// Show what is left of each allowance instead of what has been spent, so
    /// the widget counts down towards a limit rather than up from zero.
    #[serde(default)]
    pub usage_countdown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_theme_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_width: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_height: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_card_opacity: Option<u8>,
    /// Prevent auto-ejection and outside drops for widgets configured with a
    /// taskbar host. Intentional floating, desktop and tray hosts are unaffected.
    #[serde(default)]
    pub lock_taskbar: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement_override: Option<PlacementOverride>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementOverride {
    pub nest: String,
    #[serde(default)]
    pub monitor_index: usize,
    /// GDI device name (e.g. `\\.\DISPLAY2`) of the saved monitor. Enumeration
    /// order can change between boots, so this wins over `monitor_index`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_device: Option<String>,
    #[serde(default)]
    pub screen_x: i32,
    #[serde(default)]
    pub screen_y: i32,
    #[serde(default)]
    pub tray_offset: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_host: Option<FloatingHost>,
}

/// The unpadded, logical host size retained when a taskbar root is undocked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingHost {
    pub theme_id: String,
    pub surface_id: String,
    pub width: u32,
    pub height: u32,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            accounts: Default::default(),
            tray_offset: 0,
            taskbar_index: 0,
            legacy_placement_pending: false,
            widget_visible: true,
            legacy_visibility_pending: false,
            poll_interval_ms: default_poll_interval(),
            language: None,
            last_update_check_unix: None,
            show_claude_code: true,
            show_codex: false,
            show_antigravity: false,
            show_antigravity_gemini: None,
            show_antigravity_external: None,
            show_opencode: false,
            show_cursor: false,
            show_grok: false,
            show_copilot: false,
            custom_theme_enabled: true,
            usage_countdown: false,
            active_theme_path: None,
            dashboard_width: None,
            dashboard_height: None,
            floating_card_opacity: None,
            lock_taskbar: false,
            placement_override: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyPlacement {
    pub tray_offset: i32,
    pub taskbar_index: usize,
}

impl SettingsFile {
    pub fn normalize(&mut self) {
        self.accounts.claude.normalize();
        self.accounts.codex.normalize();
        if !(POLL_1_MIN..=MAX_POLL_MINUTES * POLL_1_MIN).contains(&self.poll_interval_ms)
            || !self.poll_interval_ms.is_multiple_of(POLL_1_MIN)
        {
            self.poll_interval_ms = default_poll_interval();
        }
        if self.show_antigravity_gemini.is_none() {
            self.show_antigravity_gemini = Some(self.show_antigravity);
        }
        if self.show_antigravity_external.is_none() {
            self.show_antigravity_external = Some(self.show_antigravity);
        }
        self.show_antigravity = self.show_antigravity_gemini.unwrap_or(false)
            || self.show_antigravity_external.unwrap_or(false);
        if self.enabled_providers().is_empty() {
            self.set_enabled_providers(ProviderSet::default());
        }
        if let Some(opacity) = self.floating_card_opacity {
            self.floating_card_opacity = Some(opacity.min(100));
        }
        // The widget and Theme Studio are now one system. Keep accepting this
        // legacy setting so older settings files migrate cleanly.
        self.custom_theme_enabled = true;
        self.dashboard_width = valid_dashboard_dimension(self.dashboard_width);
        self.dashboard_height = valid_dashboard_dimension(self.dashboard_height);
    }

    pub fn legacy_placement(&self) -> Option<LegacyPlacement> {
        self.legacy_placement_pending.then_some(LegacyPlacement {
            tray_offset: self.tray_offset,
            taskbar_index: self.taskbar_index,
        })
    }

    pub fn consume_legacy_placement(&mut self) -> Option<LegacyPlacement> {
        let placement = self.legacy_placement()?;
        self.legacy_placement_pending = false;
        self.tray_offset = 0;
        self.taskbar_index = 0;
        Some(placement)
    }

    pub fn legacy_widget_visibility(&self) -> Option<bool> {
        self.legacy_visibility_pending
            .then_some(self.widget_visible)
    }

    pub fn consume_legacy_widget_visibility(&mut self) -> Option<bool> {
        let visible = self.legacy_widget_visibility()?;
        self.legacy_visibility_pending = false;
        self.widget_visible = true;
        Some(visible)
    }

    pub fn enabled_providers(&self) -> ProviderSet {
        ProviderSet::from_enabled(
            ProviderId::ALL
                .into_iter()
                .filter(|provider| self.provider_enabled(*provider)),
        )
    }

    pub fn provider_enabled(&self, provider: ProviderId) -> bool {
        match provider {
            ProviderId::Claude => self.show_claude_code,
            ProviderId::Codex => self.show_codex,
            ProviderId::Antigravity => {
                self.show_antigravity_gemini
                    .unwrap_or(self.show_antigravity)
                    || self
                        .show_antigravity_external
                        .unwrap_or(self.show_antigravity)
            }
            ProviderId::OpenCode => self.show_opencode,
            ProviderId::Cursor => self.show_cursor,
            ProviderId::Grok => self.show_grok,
            ProviderId::Copilot => self.show_copilot,
        }
    }

    pub fn antigravity_gemini_enabled(&self) -> bool {
        self.show_antigravity_gemini
            .unwrap_or(self.show_antigravity)
    }

    pub fn antigravity_external_enabled(&self) -> bool {
        self.show_antigravity_external
            .unwrap_or(self.show_antigravity)
    }

    #[allow(dead_code)]
    pub fn set_antigravity_gemini_enabled(&mut self, enabled: bool) {
        let external = self.antigravity_external_enabled();
        self.show_antigravity_gemini = Some(enabled);
        self.show_antigravity_external = Some(external);
        self.show_antigravity = enabled || external;
    }

    #[allow(dead_code)]
    pub fn set_antigravity_external_enabled(&mut self, enabled: bool) {
        let gemini = self.antigravity_gemini_enabled();
        self.show_antigravity_external = Some(enabled);
        self.show_antigravity_gemini = Some(gemini);
        self.show_antigravity = gemini || enabled;
    }

    pub fn set_provider_enabled(&mut self, provider: ProviderId, enabled: bool) {
        match provider {
            ProviderId::Claude => self.show_claude_code = enabled,
            ProviderId::Codex => self.show_codex = enabled,
            ProviderId::Antigravity => {
                // Re-saving an already enabled Antigravity keeps the user's
                // per-pool choice; only an off/on transition resets both pools.
                let was_enabled =
                    self.antigravity_gemini_enabled() || self.antigravity_external_enabled();
                if enabled != was_enabled {
                    self.show_antigravity_gemini = Some(enabled);
                    self.show_antigravity_external = Some(enabled);
                }
                self.show_antigravity = enabled;
            }
            ProviderId::OpenCode => self.show_opencode = enabled,
            ProviderId::Cursor => self.show_cursor = enabled,
            ProviderId::Grok => self.show_grok = enabled,
            ProviderId::Copilot => self.show_copilot = enabled,
        }
    }

    pub fn set_enabled_providers(&mut self, providers: ProviderSet) {
        for provider in ProviderId::ALL {
            self.set_provider_enabled(provider, providers.contains(provider));
        }
    }

    pub fn toggle_provider(&mut self, provider: ProviderId) -> bool {
        let mut providers = self.enabled_providers();
        if !providers.toggle(provider) {
            return false;
        }
        self.set_enabled_providers(providers);
        true
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageCache {
    pub updated_unix: u64,
    pub poll_ok: bool,
    pub data: AppUsageData,
}

#[cfg(not(test))]
pub fn app_data_directory() -> PathBuf {
    // Single root definition: the fork config dir owns settings, themes,
    // context menus, skills and credits. Fall back to the legacy %APPDATA%
    // layout only when the known-folder lookup fails.
    if let Some(dir) = crate::manager::paths::fork_config_dir() {
        return dir;
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("UsageHub")
}

/// Test threads get independent settings, themes, menus, and caches. Do not
/// change APPDATA: provider discovery and parallel tests also read it.
#[cfg(test)]
pub fn app_data_directory() -> PathBuf {
    thread_local! {
        static DIRECTORY: TestAppData = TestAppData::new();
    }
    DIRECTORY.with(|directory| directory.0.clone())
}

#[cfg(test)]
struct TestAppData(PathBuf);

#[cfg(test)]
impl TestAppData {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        loop {
            let path = std::env::temp_dir().join(format!(
                "usagehub-test-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("cannot create test settings directory: {error}"),
            }
        }
    }
}

#[cfg(test)]
impl Drop for TestAppData {
    fn drop(&mut self) {
        // Only remove the directory this thread successfully created.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn settings_path() -> PathBuf {
    app_data_directory().join("settings.json")
}
pub fn usage_cache_path() -> PathBuf {
    app_data_directory().join("usage-cache.json")
}

pub fn load_settings() -> SettingsFile {
    let mut settings = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|content| decode_settings(&content))
        .unwrap_or_default();
    settings.normalize();
    settings
}

pub fn save_settings(settings: &SettingsFile) -> Result<(), String> {
    // Every ordinary settings save serializes on the shared settings mutex
    // so a manager read-modify-write cannot interleave with a UI snapshot
    // save. Never acquire this inside [`crate::manager::state::mutate_settings`];
    // that transaction already holds the guard and must use
    // `save_settings_unlocked` (the named mutex is not re-entrant here).
    let _guard = crate::manager::state::SettingsOperationGuard::acquire(
        crate::manager::state::SETTINGS_MUTEX_TIMEOUT_MS,
    )?;
    let mut merged = settings.clone();
    merged.normalize();
    // A long-lived UI snapshot may predate a manager registry change. Rebase
    // the caller session's accounts onto the freshly persisted registry so a
    // dashboard/window save cannot drop manager-owned managed profiles or
    // retired IDs. The caller's external profiles stay authoritative.
    if let Some(fresh) = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|content| decode_settings(&content))
    {
        merge_account_registry(&mut merged, &fresh);
        merged.normalize();
    }
    save_settings_unlocked(&merged)
}

/// Authoritative settings write for the [`crate::manager::state`] transaction
/// owner. Caller must hold `SettingsOperationGuard`; no merge is applied so
/// explicit registry mutations persist exactly as produced.
pub(crate) fn save_settings_unlocked(settings: &SettingsFile) -> Result<(), String> {
    let mut normalized = settings.clone();
    normalized.normalize();
    // Registry commit first: its result stays accurate even when the cache
    // purge below fails.
    write_json_atomic(&settings_path(), &settings_json(&normalized))?;
    // The caller already holds `SettingsOperationGuard`: reconcile the disk
    // cache inline via the unlocked writer (never the guarded
    // `save_usage_cache` — the named mutex is not re-entrant here).
    if let Err(error) = reconcile_usage_cache_locked(&normalized) {
        return Err(format!("settings saved; usage cache purge failed: {error}"));
    }
    Ok(())
}

/// Drop deleted/disabled/re-sourced accounts from the disk cache after an
/// authoritative registry write. Skips the write entirely when the cached
/// registry already matches, so unrelated setting flips never churn the
/// cache. Retired ids are opaque settings metadata and are never touched;
/// providers without an account registry pass through `select_accounts`
/// unchanged, and the encrypted 7-day deletion-undo snapshots owned by
/// `manager::backup` are out of scope here.
fn reconcile_usage_cache_locked(settings: &SettingsFile) -> Result<(), String> {
    let Some(raw): Option<UsageCache> = read_json(&usage_cache_path()) else {
        return Ok(());
    };
    let mut filtered = raw.data.clone();
    filtered.select_accounts(&settings.accounts);
    if filtered == raw.data {
        return Ok(());
    }
    write_usage_cache_locked(&filtered, raw.poll_ok)
}

/// Rebase `incoming` (a possibly stale UI snapshot) onto `fresh` (just loaded
/// under the settings mutex). Managed profiles and retired IDs are owned by
/// the manager transaction path, so the fresh disk copy wins for them; the
/// caller's external profiles (edits, additions, removals, selection) win for
/// everything else.
fn merge_account_registry(incoming: &mut SettingsFile, fresh: &SettingsFile) {
    merge_provider_accounts(&mut incoming.accounts.claude, &fresh.accounts.claude);
    merge_provider_accounts(&mut incoming.accounts.codex, &fresh.accounts.codex);
}

fn merge_provider_accounts(
    incoming: &mut crate::accounts::ProviderAccounts,
    fresh: &crate::accounts::ProviderAccounts,
) {
    use crate::accounts::AccountOwnership;
    use std::collections::HashSet;
    let mut merged: Vec<crate::accounts::AccountProfile> = Vec::new();
    let mut ids: HashSet<String> = HashSet::new();
    // Manager-owned registry is authoritative on disk: every managed profile
    // on disk survives verbatim. All managed mutations (including display
    // fields) go through manager transactions, so the stale caller's copy of
    // a managed id is never used.
    for profile in fresh
        .profiles
        .iter()
        .filter(|profile| profile.ownership == AccountOwnership::Managed)
    {
        ids.insert(profile.id.clone());
        merged.push(profile.clone());
    }
    // External profiles belong to the caller session: its additions, edits
    // and removals apply as-is. Fresh-only externals stay dropped (the
    // session removed them or never saw a concurrent addition); managed ids
    // already merged always win over a colliding external copy.
    for profile in incoming
        .profiles
        .iter()
        .filter(|profile| profile.ownership != AccountOwnership::Managed)
    {
        // A stale external snapshot may predate in-place adoption. If the
        // manager subsequently deleted that id, its reservation is a tombstone.
        if fresh.find(&profile.id).is_none() && fresh.used_ids.contains(&profile.id) {
            continue;
        }
        if ids.insert(profile.id.clone()) {
            let mut profile = profile.clone();
            if let Some(current) = fresh.find(&profile.id) {
                profile.cleanup_epoch = profile.cleanup_epoch.max(current.cleanup_epoch);
            }
            merged.push(profile);
        }
    }
    incoming.profiles = merged;
    // Retired IDs are never reused: union both sides.
    incoming.used_ids.extend(fresh.used_ids.iter().cloned());
    // Keep the caller's selection when it still resolves; otherwise adopt the
    // fresh selection when it resolves and let `normalize` repair the rest.
    if !incoming.profiles.iter().any(|p| p.id == incoming.selected)
        && fresh.profiles.iter().any(|p| p.id == fresh.selected)
    {
        incoming.selected = fresh.selected.clone();
    }
}

fn decode_settings(content: &str) -> Option<SettingsFile> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let legacy_placement_pending = value.as_object().is_some_and(|object| {
        object.contains_key("tray_offset") || object.contains_key("taskbar_index")
    });
    let legacy_visibility_pending = value
        .as_object()
        .is_some_and(|object| object.contains_key("widget_visible"));
    let mut settings: SettingsFile = serde_json::from_value(value).ok()?;
    settings.legacy_placement_pending = legacy_placement_pending;
    settings.legacy_visibility_pending = legacy_visibility_pending;
    Some(settings)
}

fn settings_json(settings: &SettingsFile) -> serde_json::Value {
    let mut value = serde_json::to_value(settings).unwrap_or_default();
    if settings.legacy_placement_pending {
        if let Some(object) = value.as_object_mut() {
            object.insert("tray_offset".into(), settings.tray_offset.into());
            object.insert("taskbar_index".into(), settings.taskbar_index.into());
        }
    }
    if settings.legacy_visibility_pending {
        if let Some(object) = value.as_object_mut() {
            object.insert("widget_visible".into(), settings.widget_visible.into());
        }
    }
    value
}

pub fn codex_credits_path() -> PathBuf {
    app_data_directory().join("codex-credits.json")
}

pub fn load_codex_credits() -> Option<CodexCreditsState> {
    read_json(&codex_credits_path())
}

pub fn save_codex_credits(state: &CodexCreditsState) -> Result<(), String> {
    write_json_atomic(&codex_credits_path(), state)
}

pub fn load_usage_cache() -> Option<UsageCache> {
    let mut cache: UsageCache = read_json(&usage_cache_path())?;
    // A deleted/disabled/re-sourced account must never reappear from disk:
    // rebind to the fresh registry first, then drop changed credentials.
    // `load_settings` only reads the file, so no settings guard is needed.
    let settings = load_settings();
    cache.data.select_accounts(&settings.accounts);
    cache.data.invalidate_changed_credentials();
    Some(cache)
}

/// Poller snapshot publish, ordered against registry commits. Holds the
/// settings guard while rebasing onto fresh settings so a late pre-delete
/// snapshot cannot resurrect a removed profile on disk. Every writer call
/// site is a poller path that never holds the guard; the authoritative
/// registry path (`save_settings_unlocked`) already holds it and must use
/// `write_usage_cache_locked` directly.
/// Lock ordering: this guard always ranks outer to the write-serial mutex
/// held inside `write_json_atomic`; never acquire this guard while holding
/// that lock.
pub fn save_usage_cache(data: &AppUsageData, poll_ok: bool) -> Result<(), String> {
    let _guard = crate::manager::state::SettingsOperationGuard::acquire(
        crate::manager::state::SETTINGS_MUTEX_TIMEOUT_MS,
    )?;
    let settings = load_settings();
    let mut filtered = data.clone();
    filtered.select_accounts(&settings.accounts);
    write_usage_cache_locked(&filtered, poll_ok)
}

fn write_usage_cache_locked(data: &AppUsageData, poll_ok: bool) -> Result<(), String> {
    write_json_atomic(
        &usage_cache_path(),
        &UsageCache {
            updated_unix: now_unix(),
            poll_ok,
            data: data.clone(),
        },
    )
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    use std::fs::OpenOptions;
    use std::io::Write;
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // Process-local serialization for the temp/write/replace critical section.
    // Unique temp names alone do not prevent simultaneous MoveFileExW replaces
    // of the same destination from racing on Windows, so hold this across the
    // whole section. Lock ordering: the cross-process settings guard (held by
    // `save_settings`/`mutate_settings`) always ranks outer to this lock;
    // never acquire that guard while holding this lock.
    static WRITE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let parent = path.parent().ok_or("Invalid settings path")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.json");
    let json = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let _serial = WRITE_SERIAL
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Unique temp per call (pid + process-wide sequence): concurrent same-process
    // writers (settings, usage-cache, codex-credits) never truncate each other's
    // temp file. `create_new` retries on the residual sequence-wraparound hit.
    let mut attempts = 0;
    let (temporary, mut file) = loop {
        let sequence = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.{}.{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let write_result = (|| {
        file.write_all(&json).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        Ok::<_, String>(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    let source = wide_path(&temporary);
    let destination = wide_path(path);
    let moved = unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = moved {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}
fn default_true() -> bool {
    true
}
fn valid_dashboard_dimension(value: Option<f32>) -> Option<f32> {
    value.filter(|value| value.is_finite() && (64.0..=16_384.0).contains(value))
}
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskbar_lock_is_opt_in_and_survives_settings_round_trip() {
        assert!(!decode_settings("{}").unwrap().lock_taskbar);
        for lock_taskbar in [false, true] {
            let settings = SettingsFile {
                lock_taskbar,
                ..Default::default()
            };
            let decoded = decode_settings(&settings_json(&settings).to_string()).unwrap();
            assert_eq!(decoded.lock_taskbar, lock_taskbar);
        }
    }

    #[test]
    fn application_files_stay_inside_the_test_directory() {
        let root = app_data_directory();
        assert!(root.starts_with(std::env::temp_dir()));
        if let Some(real) = std::env::var_os("APPDATA") {
            assert!(!root.starts_with(PathBuf::from(real).join("UsageHub")));
        }
        for path in [
            settings_path(),
            usage_cache_path(),
            codex_credits_path(),
            crate::theme_engine::themes_directory(),
            crate::theme_engine::assets_directory(),
            crate::context_menu::context_menus_directory(),
            crate::theme_engine::ensure_starter_theme().unwrap(),
            crate::context_menu::ensure_builtin_context_menus().unwrap(),
        ] {
            assert!(path.starts_with(&root), "{}", path.display());
        }
        save_settings(&SettingsFile::default()).unwrap();
        assert!(settings_path().is_file());
    }

    #[test]
    fn parallel_test_threads_have_independent_settings_and_clean_up() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [7, 11]
            .into_iter()
            .map(|minutes| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let settings = SettingsFile {
                        poll_interval_ms: minutes * POLL_1_MIN,
                        ..Default::default()
                    };
                    save_settings(&settings).unwrap();
                    barrier.wait();
                    assert_eq!(load_settings().poll_interval_ms, minutes * POLL_1_MIN);
                    app_data_directory()
                })
            })
            .collect();
        let paths: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_ne!(paths[0], paths[1]);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn custom_poll_minutes_and_presets_survive_settings_round_trip() {
        for minutes in [1, 2, 5, 7, 15, 60, 120, 1_440, MAX_POLL_MINUTES] {
            let interval = minutes * POLL_1_MIN;
            let mut decoded =
                decode_settings(&format!(r#"{{"poll_interval_ms":{interval}}}"#)).unwrap();
            decoded.normalize();
            assert_eq!(decoded.poll_interval_ms, interval);
            let mut reloaded = decode_settings(&settings_json(&decoded).to_string()).unwrap();
            reloaded.normalize();
            assert_eq!(reloaded.poll_interval_ms, interval);
        }
    }

    #[test]
    fn invalid_poll_intervals_fall_back_to_the_default() {
        for interval in [
            0,
            POLL_1_MIN - 1,
            POLL_1_MIN + 1,
            (MAX_POLL_MINUTES + 1) * POLL_1_MIN,
            u32::MAX,
        ] {
            let mut decoded =
                decode_settings(&format!(r#"{{"poll_interval_ms":{interval}}}"#)).unwrap();
            decoded.normalize();
            assert_eq!(decoded.poll_interval_ms, default_poll_interval());
        }
    }

    #[test]
    fn named_accounts_round_trip_without_changing_legacy_provider_preferences() {
        let old = decode_settings(r#"{"show_claude_code":true,"show_codex":true}"#).unwrap();
        assert_eq!(old.accounts, crate::accounts::AccountSettings::default());
        let mut settings = old;
        settings.accounts.codex.add();
        settings.accounts.codex.profiles[1].config_dir = "C:\\Users\\Test\\.codex-work".into();
        settings.accounts.codex.profiles[1].enabled = true;
        settings.accounts.codex.selected = "account_1".into();
        let decoded = decode_settings(&settings_json(&settings).to_string()).unwrap();
        assert_eq!(decoded.accounts, settings.accounts);
        assert_eq!(decoded.enabled_providers(), settings.enabled_providers());
    }

    #[test]
    fn settings_never_disable_every_provider() {
        let mut settings = SettingsFile {
            show_claude_code: false,
            show_codex: false,
            show_antigravity: false,
            ..Default::default()
        };
        settings.normalize();
        assert_eq!(settings.enabled_providers(), ProviderSet::default());
    }

    #[test]
    fn provider_selection_keeps_the_existing_settings_keys() {
        let mut settings = SettingsFile::default();
        settings.set_enabled_providers(ProviderSet::from_enabled([
            ProviderId::Codex,
            ProviderId::Antigravity,
            ProviderId::OpenCode,
            ProviderId::Cursor,
            ProviderId::Grok,
            ProviderId::Copilot,
        ]));

        let json = settings_json(&settings);
        assert_eq!(json["show_claude_code"], false);
        assert_eq!(json["show_codex"], true);
        assert_eq!(json["show_antigravity"], true);
        assert_eq!(json["show_opencode"], true);
        assert_eq!(json["show_cursor"], true);
        assert_eq!(json["show_grok"], true);
        assert_eq!(json["show_copilot"], true);

        let decoded = decode_settings(&json.to_string()).unwrap();
        assert_eq!(decoded.enabled_providers(), settings.enabled_providers());
    }

    #[test]
    fn provider_toggle_keeps_the_last_provider_enabled() {
        let mut settings = SettingsFile::default();
        assert!(!settings.toggle_provider(ProviderId::Claude));
        assert_eq!(settings.enabled_providers(), ProviderSet::default());
    }

    #[test]
    fn usage_direction_defaults_to_counting_up_and_round_trips() {
        let settings = SettingsFile::default();
        assert!(!settings.usage_countdown);
        assert_eq!(settings_json(&settings)["usage_countdown"], false);

        let counting_up = decode_settings(r#"{"poll_interval_ms":900000}"#).unwrap();
        assert!(!counting_up.usage_countdown);

        let counting_down = decode_settings(r#"{"usage_countdown":true}"#).unwrap();
        assert!(counting_down.usage_countdown);
        assert_eq!(settings_json(&counting_down)["usage_countdown"], true);
    }

    #[test]
    fn settings_always_use_the_theme_widget() {
        let mut settings = SettingsFile {
            custom_theme_enabled: false,
            ..Default::default()
        };
        settings.normalize();
        assert!(settings.custom_theme_enabled);
    }

    #[test]
    fn legacy_widget_visibility_is_preserved_until_migration_consumes_it() {
        let mut settings = decode_settings(r#"{"widget_visible":false}"#).unwrap();
        assert_eq!(settings.legacy_widget_visibility(), Some(false));
        assert_eq!(settings_json(&settings)["widget_visible"], false);

        assert_eq!(settings.consume_legacy_widget_visibility(), Some(false));
        assert_eq!(settings.legacy_widget_visibility(), None);
        assert!(settings_json(&settings).get("widget_visible").is_none());
    }

    #[test]
    fn legacy_placement_is_preserved_until_the_migration_consumes_it() {
        let mut settings = decode_settings(
            r#"{
                "tray_offset": 144,
                "taskbar_index": 2,
                "poll_interval_ms": 60000,
                "show_claude_code": true
            }"#,
        )
        .unwrap();

        assert_eq!(
            settings.legacy_placement(),
            Some(LegacyPlacement {
                tray_offset: 144,
                taskbar_index: 2,
            })
        );
        let pending = settings_json(&settings);
        assert_eq!(pending["tray_offset"], 144);
        assert_eq!(pending["taskbar_index"], 2);

        settings.consume_legacy_placement();
        let migrated = settings_json(&settings);
        assert!(migrated.get("tray_offset").is_none());
        assert!(migrated.get("taskbar_index").is_none());
        assert_eq!(migrated["poll_interval_ms"], 60000);
    }

    #[test]
    fn modern_settings_do_not_request_legacy_migration() {
        let settings = decode_settings(
            r#"{
                "poll_interval_ms": 900000,
                "active_theme_path": "migrated-theme.json"
            }"#,
        )
        .unwrap();
        assert_eq!(settings.legacy_placement(), None);
        assert_eq!(settings.legacy_widget_visibility(), None);
    }

    #[test]
    fn dashboard_dimensions_are_preserved_and_validated() {
        let settings = decode_settings(
            r#"{
                "dashboard_width": 1280.5,
                "dashboard_height": 760.0
            }"#,
        )
        .unwrap();
        assert_eq!(settings.dashboard_width, Some(1280.5));
        assert_eq!(settings.dashboard_height, Some(760.0));

        let mut invalid = SettingsFile {
            dashboard_width: Some(0.0),
            dashboard_height: Some(20_000.0),
            ..Default::default()
        };
        invalid.normalize();
        assert_eq!(invalid.dashboard_width, None);
        assert_eq!(invalid.dashboard_height, None);
    }

    #[test]
    fn resaving_enabled_providers_keeps_antigravity_pool_choice() {
        let mut settings = SettingsFile::default();
        settings.set_provider_enabled(ProviderId::Antigravity, true);
        settings.set_antigravity_gemini_enabled(false);
        settings.set_enabled_providers(ProviderSet::from_enabled([
            ProviderId::Claude,
            ProviderId::Antigravity,
        ]));
        assert!(!settings.antigravity_gemini_enabled());
        assert!(settings.antigravity_external_enabled());

        settings.set_provider_enabled(ProviderId::Antigravity, false);
        assert!(!settings.antigravity_gemini_enabled());
        assert!(!settings.antigravity_external_enabled());
        settings.set_provider_enabled(ProviderId::Antigravity, true);
        assert!(settings.antigravity_gemini_enabled() && settings.antigravity_external_enabled());
    }
    #[test]
    fn enabling_one_antigravity_pool_does_not_enable_the_other_on_migration() {
        for gemini in [true, false] {
            let mut settings = SettingsFile::default();
            if gemini {
                settings.set_antigravity_gemini_enabled(true);
            } else {
                settings.set_antigravity_external_enabled(true);
            }
            settings.normalize();
            assert!(settings.provider_enabled(ProviderId::Antigravity));
            assert_eq!(settings.antigravity_gemini_enabled(), gemini);
            assert_eq!(settings.antigravity_external_enabled(), !gemini);
        }
    }

    #[test]
    fn antigravity_legacy_settings_migrate_and_disabled_pools_stay_disabled() {
        for enabled in [true, false] {
            let mut settings: SettingsFile = serde_json::from_value(serde_json::json!({
                "show_antigravity": enabled
            }))
            .unwrap();
            settings.normalize();
            assert_eq!(settings.antigravity_gemini_enabled(), enabled);
            assert_eq!(settings.antigravity_external_enabled(), enabled);
            settings.set_provider_enabled(ProviderId::Antigravity, false);
            let mut reloaded: SettingsFile =
                serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
            reloaded.normalize();
            assert!(!reloaded.provider_enabled(ProviderId::Antigravity));
            assert!(!reloaded.antigravity_gemini_enabled());
            assert!(!reloaded.antigravity_external_enabled());
        }
    }

    #[test]
    fn stale_ordinary_save_keeps_managed_registry_and_external_edits() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        // A long-lived UI snapshot taken before the manager transaction.
        let mut stale = SettingsFile::default();
        // Manager transaction adds a managed account after the snapshot.
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.codex.profiles.push(AccountProfile {
                id: "account_1".into(),
                name: "Managed One".into(),
                enabled: true,
                ownership: AccountOwnership::Managed,
                ..Default::default()
            });
            settings.accounts.codex.used_ids.insert("account_1".into());
            settings.accounts.codex.selected = "account_1".into();
            Ok::<_, String>(())
        })
        .unwrap();
        // The stale session edits its own external profile and a UI flag.
        stale.accounts.codex.profiles[0].config_dir = "C:\\Users\\Test\\.codex-ui".into();
        stale.usage_countdown = true;
        save_settings(&stale).unwrap();
        let saved = load_settings();
        // Manager-owned registry survives the stale save.
        let managed = saved
            .accounts
            .codex
            .find("account_1")
            .expect("managed account must survive a stale ordinary save");
        assert_eq!(managed.ownership, AccountOwnership::Managed);
        assert_eq!(managed.name, "Managed One");
        assert!(saved.accounts.codex.used_ids.contains("account_1"));
        // The caller's own external edits survive too.
        assert_eq!(
            saved.accounts.codex.find("default").unwrap().config_dir,
            "C:\\Users\\Test\\.codex-ui"
        );
        assert!(saved.usage_countdown);
    }

    #[test]
    fn stale_display_save_never_rewinds_manager_managed_edits() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        // Seed a managed account, then snapshot it as a stale UI session.
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles.push(AccountProfile {
                id: "account_1".into(),
                name: "Managed One".into(),
                enabled: true,
                ownership: AccountOwnership::Managed,
                ..Default::default()
            });
            settings.accounts.claude.used_ids.insert("account_1".into());
            Ok::<_, String>(())
        })
        .unwrap();
        let mut stale = load_settings();
        // Manager transaction renames, pins and re-points the same id.
        crate::manager::state::mutate_settings(|settings| {
            let profile = settings
                .accounts
                .claude
                .find_mut("account_1")
                .expect("managed account must exist");
            profile.name = "Managed Renamed".into();
            profile.pinned = true;
            profile.sort_order = 7;
            profile.config_dir = "C:\\Managed\\New".into();
            Ok::<_, String>(())
        })
        .unwrap();
        // The stale session only flips a display flag; its outdated managed
        // copy must not clobber the manager's latest values.
        stale.accounts.claude.find("account_1").unwrap();
        stale.usage_countdown = true;
        save_settings(&stale).unwrap();
        let saved = load_settings();
        let managed = saved
            .accounts
            .claude
            .find("account_1")
            .expect("managed account must survive a stale ordinary save");
        assert_eq!(managed.name, "Managed Renamed");
        assert!(managed.pinned);
        assert_eq!(managed.sort_order, 7);
        assert_eq!(managed.config_dir, "C:\\Managed\\New");
        assert!(saved.usage_countdown);
    }

    #[test]
    fn stale_ordinary_save_keeps_retired_ids_and_manager_selection() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        // Retire a managed id through the authoritative transaction path:
        // the profile is gone but the id stays reserved.
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles.push(AccountProfile {
                id: "account_1".into(),
                ownership: AccountOwnership::Managed,
                ..Default::default()
            });
            settings.accounts.claude.used_ids.insert("account_1".into());
            Ok::<_, String>(())
        })
        .unwrap();
        let mut stale = load_settings();
        crate::manager::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "account_1");
            Ok::<_, String>(())
        })
        .unwrap();
        // The stale snapshot still lists the retired profile; the ordinary
        // save must not resurrect it, but must keep the retired id reserved
        // and adopt the manager's selection.
        assert!(stale.accounts.claude.find("account_1").is_some());
        stale.usage_countdown = true;
        save_settings(&stale).unwrap();
        let saved = load_settings();
        assert!(saved.accounts.claude.find("account_1").is_none());
        assert!(saved.accounts.claude.used_ids.contains("account_1"));
        assert_eq!(saved.accounts.claude.selected, "default");
        assert!(saved.usage_countdown);
    }

    #[test]
    fn stale_external_save_cannot_revive_adopted_deleted_profile() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles.push(AccountProfile {
                id: "adopted_second".into(),
                ownership: AccountOwnership::External,
                cleanup_epoch: 4,
                ..Default::default()
            });
            settings
                .accounts
                .claude
                .used_ids
                .insert("adopted_second".into());
            Ok::<_, String>(())
        })
        .unwrap();
        let mut stale = load_settings();
        crate::manager::state::mutate_settings(|settings| {
            let adopted = settings.accounts.claude.find_mut("adopted_second").unwrap();
            adopted.ownership = AccountOwnership::Managed;
            adopted.cleanup_epoch = 5;
            Ok::<_, String>(())
        })
        .unwrap();
        // This is the authoritative retirement transaction used after deletion.
        crate::manager::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|p| p.id != "adopted_second");
            Ok::<_, String>(())
        })
        .unwrap();
        stale.accounts.claude.profiles.push(AccountProfile {
            id: "new_external".into(),
            ownership: AccountOwnership::External,
            ..Default::default()
        });
        stale.usage_countdown = true;
        save_settings(&stale).unwrap();
        let saved = load_settings();
        assert!(saved.accounts.claude.find("adopted_second").is_none());
        assert!(saved.accounts.claude.used_ids.contains("adopted_second"));
        assert!(saved.accounts.claude.find("new_external").is_some());
        assert!(saved.usage_countdown);
    }

    #[test]
    fn concurrent_atomic_writes_share_no_temp_file() {
        let target = app_data_directory().join("concurrent-probe.json");
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let target = target.clone();
                std::thread::spawn(move || {
                    for round in 0..25 {
                        write_json_atomic(
                            &target,
                            &serde_json::json!({ "worker": index, "round": round }),
                        )
                        .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let content = std::fs::read_to_string(&target).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(value.get("worker").is_some());
        let leftovers: Vec<_> = std::fs::read_dir(app_data_directory())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp leftovers: {leftovers:?}");
    }

    fn test_source_dir(marker: &str, file_name: &str) -> (std::path::PathBuf, String) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Real temp credential file under the system temp root (never a
        // production root): deterministic signatures for hermetic asserts.
        let dir = std::env::temp_dir().join(format!(
            "usagehub-cache-purge-{}-{}-{}-{marker}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(dir.starts_with(std::env::temp_dir()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file_name), "{\"token\":\"fixture\"}").unwrap();
        let config_dir = dir.to_string_lossy().into_owned();
        (dir, config_dir)
    }

    fn test_cache_account(
        provider: ProviderId,
        profile: crate::accounts::AccountProfile,
        percentage: f64,
    ) -> crate::models::AccountUsage {
        use crate::models::{AccountUsage, UsageData, UsageSection};
        // Mirror poller output: provenance points at the real explicit
        // source so `invalidate_changed_credentials` keeps the reading.
        let source_path = crate::poller::explicit_source_path(provider, &profile);
        let source_signature = crate::poller::profile_source_signature(provider, &profile);
        AccountUsage {
            provider,
            profile,
            source_signature,
            source_path,
            usage: Some(UsageData {
                session: UsageSection {
                    available: true,
                    percentage,
                    ..Default::default()
                },
                ..Default::default()
            }),
            error: None,
            selected: false,
        }
    }

    #[test]
    fn registry_removal_purges_deleted_account_from_disk_cache() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        use crate::models::{UsageData, UsageSection};
        let (personal_dir, personal_config) = test_source_dir("personal", ".credentials.json");
        let (work_dir, work_config) = test_source_dir("work", ".credentials.json");
        let (codex_dir, codex_config) = test_source_dir("codex", "auth.json");
        // Directory names end with `-{marker}` (no trailing dash): match the
        // exact file names so these asserts track `test_source_dir` format.
        let personal_tag = personal_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let work_tag = work_dir.file_name().unwrap().to_string_lossy().into_owned();
        let codex_tag = codex_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // Authoritative registry path: holds the Windows named settings
        // lock, mirroring production deletion.
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles = vec![
                AccountProfile {
                    id: "personal".into(),
                    name: "Personal".into(),
                    config_dir: personal_config.clone(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                },
                AccountProfile {
                    id: "work".into(),
                    name: "Work".into(),
                    config_dir: work_config.clone(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                },
            ];
            settings.accounts.claude.selected = "personal".into();
            settings.accounts.claude.used_ids.extend([
                "personal".to_string(),
                "work".to_string(),
                "account_9".to_string(),
            ]);
            settings.accounts.codex.profiles[0].config_dir = codex_config.clone();
            Ok::<_, String>(())
        })
        .unwrap();
        let settings = load_settings();
        let personal = settings.accounts.claude.find("personal").unwrap().clone();
        let work = settings.accounts.claude.find("work").unwrap().clone();
        let codex = settings.accounts.codex.find("default").unwrap().clone();
        let mut data = AppUsageData::from_iter([(
            ProviderId::Cursor,
            UsageData {
                session: UsageSection {
                    available: true,
                    percentage: 12.0,
                    ..Default::default()
                },
                ..Default::default()
            },
        )]);
        data.accounts = vec![
            test_cache_account(ProviderId::Claude, personal, 25.0),
            test_cache_account(ProviderId::Claude, work, 60.0),
            test_cache_account(ProviderId::Codex, codex, 10.0),
        ];
        save_usage_cache(&data, true).unwrap();
        let seeded = std::fs::read_to_string(usage_cache_path()).unwrap();
        assert!(seeded.contains(&personal_tag), "{seeded}");
        assert!(seeded.contains(&work_tag), "{seeded}");
        // Delete the work account through the authoritative path.
        crate::manager::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "work");
            Ok::<_, String>(())
        })
        .unwrap();
        let raw = std::fs::read_to_string(usage_cache_path()).unwrap();
        assert!(
            !raw.contains(&work_tag),
            "deleted work source leaked on disk: {raw}"
        );
        assert!(
            !raw.contains("\"work\""),
            "deleted work id leaked on disk: {raw}"
        );
        assert!(raw.contains(&personal_tag), "sibling account lost: {raw}");
        assert!(
            raw.contains(&codex_tag),
            "unrelated provider account lost: {raw}"
        );
        // Opaque retired ids stay reserved; the registry still resolves.
        let settings = load_settings();
        assert!(settings.accounts.claude.find("work").is_none());
        assert!(settings.accounts.claude.used_ids.contains("work"));
        assert!(settings.accounts.claude.used_ids.contains("account_9"));
        // Filtered load agrees with disk: no resurrection, siblings kept.
        let cache = load_usage_cache().unwrap();
        assert!(cache
            .data
            .accounts
            .iter()
            .all(|account| account.profile.id != "work"));
        assert_eq!(
            cache
                .data
                .get(ProviderId::Claude)
                .unwrap()
                .session
                .percentage,
            25.0
        );
        assert_eq!(
            cache
                .data
                .get(ProviderId::Codex)
                .unwrap()
                .session
                .percentage,
            10.0
        );
        assert_eq!(
            cache
                .data
                .get(ProviderId::Cursor)
                .unwrap()
                .session
                .percentage,
            12.0
        );
        std::fs::remove_dir_all(&personal_dir).unwrap();
        std::fs::remove_dir_all(&work_dir).unwrap();
        std::fs::remove_dir_all(&codex_dir).unwrap();
    }

    #[test]
    fn late_pre_delete_snapshot_cannot_resurrect_removed_profile() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        let (personal_dir, personal_config) = test_source_dir("personal", ".credentials.json");
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles = vec![AccountProfile {
                id: "personal".into(),
                name: "Personal".into(),
                config_dir: personal_config.clone(),
                enabled: true,
                ownership: AccountOwnership::Managed,
                ..Default::default()
            }];
            settings.accounts.claude.selected = "personal".into();
            settings
                .accounts
                .claude
                .used_ids
                .extend(["personal".to_string(), "work".to_string()]);
            settings.accounts.codex.profiles[0].config_dir.clear();
            Ok::<_, String>(())
        })
        .unwrap();
        let settings = load_settings();
        let personal = settings.accounts.claude.find("personal").unwrap().clone();
        // A poller snapshot taken before the delete still lists the work
        // account; publishing it after the delete must not bring it back.
        let mut stale = AppUsageData::default();
        stale.accounts = vec![
            test_cache_account(ProviderId::Claude, personal, 25.0),
            test_cache_account(
                ProviderId::Claude,
                AccountProfile {
                    id: "work".into(),
                    name: "Work".into(),
                    config_dir: "C:\\removed\\work".into(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                },
                60.0,
            ),
        ];
        save_usage_cache(&stale, true).unwrap();
        let raw = std::fs::read_to_string(usage_cache_path()).unwrap();
        assert!(
            !raw.contains("removed"),
            "stale snapshot resurrected deleted source: {raw}"
        );
        assert!(
            !raw.contains("\"work\""),
            "stale snapshot resurrected deleted id: {raw}"
        );
        let cache = load_usage_cache().unwrap();
        assert_eq!(cache.data.accounts.len(), 1);
        assert_eq!(cache.data.accounts[0].profile.id, "personal");
        std::fs::remove_dir_all(&personal_dir).unwrap();
    }

    #[test]
    fn unrelated_setting_change_leaves_disk_cache_untouched() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        let (personal_dir, personal_config) = test_source_dir("personal", ".credentials.json");
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles = vec![AccountProfile {
                id: "personal".into(),
                name: "Personal".into(),
                config_dir: personal_config.clone(),
                enabled: true,
                ownership: AccountOwnership::Managed,
                ..Default::default()
            }];
            settings.accounts.claude.selected = "personal".into();
            settings.accounts.codex.profiles[0].config_dir.clear();
            Ok::<_, String>(())
        })
        .unwrap();
        let settings = load_settings();
        let personal = settings.accounts.claude.find("personal").unwrap().clone();
        let mut data = AppUsageData::default();
        data.accounts = vec![test_cache_account(ProviderId::Claude, personal, 25.0)];
        save_usage_cache(&data, true).unwrap();
        let before = std::fs::read(usage_cache_path()).unwrap();
        crate::manager::state::mutate_settings(|settings| {
            settings.usage_countdown = !settings.usage_countdown;
            Ok::<_, String>(())
        })
        .unwrap();
        let after = std::fs::read(usage_cache_path()).unwrap();
        assert_eq!(
            before, after,
            "unrelated setting flip churned the disk cache"
        );
        std::fs::remove_dir_all(&personal_dir).unwrap();
    }

    #[test]
    fn cache_purge_failure_keeps_registry_truthful() {
        use crate::accounts::{AccountOwnership, AccountProfile};
        let (personal_dir, personal_config) = test_source_dir("personal", ".credentials.json");
        let (work_dir, work_config) = test_source_dir("work", ".credentials.json");
        let work_tag = work_dir.file_name().unwrap().to_string_lossy().into_owned();
        crate::manager::state::mutate_settings(|settings| {
            settings.accounts.claude.profiles = vec![
                AccountProfile {
                    id: "personal".into(),
                    name: "Personal".into(),
                    config_dir: personal_config.clone(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                },
                AccountProfile {
                    id: "work".into(),
                    name: "Work".into(),
                    config_dir: work_config.clone(),
                    enabled: true,
                    ownership: AccountOwnership::Managed,
                    ..Default::default()
                },
            ];
            settings.accounts.claude.selected = "personal".into();
            settings.accounts.codex.profiles[0].config_dir.clear();
            Ok::<_, String>(())
        })
        .unwrap();
        let settings = load_settings();
        let mut data = AppUsageData::default();
        data.accounts = vec![
            test_cache_account(
                ProviderId::Claude,
                settings.accounts.claude.find("personal").unwrap().clone(),
                25.0,
            ),
            test_cache_account(
                ProviderId::Claude,
                settings.accounts.claude.find("work").unwrap().clone(),
                60.0,
            ),
        ];
        save_usage_cache(&data, true).unwrap();
        // Read-only cache file: the registry commit succeeds but the purge
        // replace fails. The guard below restores writability on any exit.
        struct WritableOnDrop(std::path::PathBuf);
        impl Drop for WritableOnDrop {
            fn drop(&mut self) {
                if let Ok(metadata) = std::fs::metadata(&self.0) {
                    let mut permissions = metadata.permissions();
                    if permissions.readonly() {
                        permissions.set_readonly(false);
                        let _ = std::fs::set_permissions(&self.0, permissions);
                    }
                }
            }
        }
        let cache_path = usage_cache_path();
        let mut permissions = std::fs::metadata(&cache_path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&cache_path, permissions).unwrap();
        let _writable = WritableOnDrop(cache_path.clone());
        let result = crate::manager::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "work");
            Ok::<_, String>(())
        });
        let error = result.expect_err("read-only cache must surface a partial error");
        assert!(error.contains("usage cache"), "unexpected error: {error}");
        // Registry commit stayed accurate despite the purge failure.
        assert!(load_settings().accounts.claude.find("work").is_none());
        drop(_writable);
        // Recovery purges the deleted account and reports success.
        save_settings(&load_settings()).unwrap();
        let raw = std::fs::read_to_string(&cache_path).unwrap();
        assert!(
            !raw.contains(&work_tag),
            "deleted work source leaked on disk: {raw}"
        );
        std::fs::remove_dir_all(&personal_dir).unwrap();
        std::fs::remove_dir_all(&work_dir).unwrap();
    }
}
