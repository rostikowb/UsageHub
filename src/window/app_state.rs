//! Shared window state, Win32 handle wrappers, timers, and menu constants.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::WM_APP;

use crate::app_settings::{self, PlacementOverride};
use crate::localization::LanguageId;
use crate::models::AppUsageData;
use crate::poller;
use crate::providers::ProviderSet;
use crate::theme_engine::{self, MouseActionOverrideKey, ThemeDocument};
use crate::updater::{AvailableUpdate, InstallChannel};

/// Copyable HWND value used by the watchdog after the UI thread publishes it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct SendHwnd(pub(super) isize);

// SAFETY: this wrapper never transfers ownership of a window. Cross-thread users
// only pass the value back to Win32 APIs that explicitly accept handles created
// by another thread (for example IsWindow and PostMessageW).
unsafe impl Send for SendHwnd {}

impl SendHwnd {
    pub(super) fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    pub(super) fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Copyable event-hook value whose lifetime remains owned by the UI controller.
#[derive(Clone, Copy)]
pub(super) struct SendWinEventHook(pub(super) isize);

// SAFETY: the hook is only stored or passed to UnhookWinEvent. Callback work is
// marshalled through Win32; Rust data is never dereferenced through this value.
unsafe impl Send for SendWinEventHook {}

impl SendWinEventHook {
    pub(super) fn from_hook(hook: HWINEVENTHOOK) -> Self {
        Self(hook.0 as isize)
    }

    pub(super) fn to_hook(self) -> HWINEVENTHOOK {
        HWINEVENTHOOK(self.0 as *mut _)
    }
}

/// Shared application state.
#[derive(Clone)]
pub(super) struct AppState {
    pub(super) hwnd: SendHwnd,
    pub(super) taskbar_hwnd: Option<SendHwnd>,
    pub(super) tray_notify_hwnd: Option<SendHwnd>,
    pub(super) win_event_hook: Option<SendWinEventHook>,
    pub(super) is_dark: bool,
    pub(super) embedded: bool,
    pub(super) language_override: Option<LanguageId>,
    pub(super) language: LanguageId,
    pub(super) install_channel: InstallChannel,

    pub(super) providers: ProviderSet,
    pub(super) show_antigravity_gemini: bool,
    pub(super) show_antigravity_external: bool,
    pub(super) crossfade_opacity: f64,
    pub(super) accounts: crate::accounts::AccountSettings,

    pub(super) data: Option<AppUsageData>,

    pub(super) poll_interval_ms: u32,
    pub(super) retry_count: u32,
    pub(super) force_notify_auth_error: bool,
    pub(super) auth_error_paused_polling: bool,
    pub(super) auth_watch_mode: poller::CredentialWatchMode,
    pub(super) auth_watch_snapshot: poller::CredentialWatchSnapshot,
    pub(super) last_poll_ok: bool,
    pub(super) last_poll_failure: Option<poller::PollFailure>,
    pub(super) update_status: UpdateStatus,
    pub(super) last_update_check_unix: Option<u64>,

    pub(super) taskbar_index: usize,
    pub(super) tray_offset: i32,
    pub(super) dragging: bool,
    pub(super) pending_drag: bool,
    pub(super) drag_start_cursor: POINT,
    pub(super) drag_start_origin: POINT,
    pub(super) drag_start_client_x: i32,
    pub(super) auto_ejected: bool,
    pub(super) auto_ejected_for_capacity: bool,
    pub(super) auto_ejected_origin: Option<POINT>,
    pub(super) auto_ejected_host: Option<app_settings::FloatingHost>,
    /// When true, never auto-eject to floating on taskbar collisions.
    /// The widget stays docked (but can still be dragged along the bar).
    pub(super) lock_taskbar: bool,
    pub(super) is_switching_window_style: bool,
    pub(super) is_snapped: bool,
    pub(super) placement_override: Option<PlacementOverride>,
    pub(super) floating_card_opacity: Option<u8>,
    pub(super) window_state_timer_active: bool,

    pub(super) custom_theme_enabled: bool,
    pub(super) usage_countdown: bool,
    pub(super) active_theme_path: Option<PathBuf>,
    pub(super) active_theme: Option<ThemeDocument>,
    pub(super) theme_clock_interval: Option<Duration>,
    pub(super) tray_theme_uses_current_time: bool,
    pub(super) mirror_hwnds: Vec<SendHwnd>,
    pub(super) desktop_hwnds: Vec<Option<SendHwnd>>,
    pub(super) mouse_action_overrides: HashMap<MouseActionOverrideKey, theme_engine::Expression>,
    pub(super) hovered_mouse_layer: Option<(usize, String)>,
    pub(super) pending_mouse_click: Option<PendingMouseClick>,
    pub(super) suppress_next_left_up: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PendingMouseClick {
    pub(super) surface_index: usize,
    pub(super) object_id: String,
}

#[derive(Clone, Debug)]
pub(super) enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(AvailableUpdate),
}

pub(super) const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

// Menu item IDs for update frequency
pub(super) const IDM_FREQ_1MIN: u16 = 10;
pub(super) const IDM_FREQ_5MIN: u16 = 11;
pub(super) const IDM_FREQ_15MIN: u16 = 12;
pub(super) const IDM_FREQ_1HOUR: u16 = 13;
pub(super) const IDM_START_WITH_WINDOWS: u16 = 20;
pub(super) const IDM_LOCK_TASKBAR: u16 = 21;
pub(super) const IDM_VERSION_ACTION: u16 = 31;
pub(super) const IDM_LANG_SYSTEM: u16 = 100;
pub(super) const IDM_LANG_FIRST: u16 = 101;
pub(super) const IDM_DASHBOARD: u16 = 71;
// 62 stays the whole-provider Antigravity toggle used by saved custom menus.
pub(super) const IDM_PROVIDER_ANTIGRAVITY_GEMINI: u16 = 68;
pub(super) const IDM_PROVIDER_ANTIGRAVITY_EXTERNAL: u16 = 67;

pub(super) const WM_DPICHANGED_MSG: u32 = 0x02E0;
pub(super) const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;
pub(super) const TRAY_ICON_UPDATE_REPOSITION_SUPPRESS_MS: u64 = 750;
pub(super) const WINDOW_STATE_INTERVAL_MS: u32 = 250;
/// Controller timer that expires 7-day deletion-undo snapshots on a worker
/// thread. Hourly is cheap (one directory walk) and keeps the safety net
/// tight; startup and resume always purge immediately.
pub(super) const TIMER_BACKUP_PURGE: usize = 41;
pub(super) const BACKUP_PURGE_INTERVAL_MS: u32 = 3_600_000;
/// Modest periodic taskbar identity maintenance (shortcut/COM stamping).
/// The worker coalesces behind one in-flight flag; the native helper owns
/// snapshot caching so a tick never shells out per account.
pub(super) const TIMER_TASKBAR_IDENTITIES: usize = 42;
pub(super) const TIMER_DISCOVERY: usize = 43;
/// Cheap throttle tick: the worker itself honours the configured hours.
pub(super) const DISCOVERY_TICK_INTERVAL_MS: u32 = 15 * 60 * 1_000;
pub(super) const TASKBAR_IDENTITIES_INTERVAL_MS: u32 = 20_000;

/// How often the watchdog thread polls for an explorer.exe restart (which
/// recreates the taskbar and wipes our tray-icon registration).
pub(super) const TASKBAR_WATCH_INTERVAL_SECS: u64 = 2;
pub(super) const TASKBAR_COLLISION_REPOSITION: usize = 2;

/// Stable controller handle published for the hang watchdog outside of `STATE`.
///
/// The hang watchdog has to diagnose a wedged UI thread even when that thread
/// is wedged while holding the state mutex, so it never takes that lock. Written
/// once, right after the window is created.
pub(super) static CONTROLLER_HWND: AtomicIsize = AtomicIsize::new(0);

/// Prevent duplicate recovery attempts by the UI hang watchdog.
pub(super) static RELAUNCH_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

pub(super) static SUPPRESS_TRAY_REPOSITION_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
pub(super) static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);
pub(super) static POLL_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
pub(super) static POLL_PENDING: AtomicBool = AtomicBool::new(false);

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex.
pub(super) fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clone `AppState` briefly under lock so Win32/UIA callers never hold `STATE`.
pub(super) fn snapshot_state() -> Option<AppState> {
    lock_state().as_ref().cloned()
}
