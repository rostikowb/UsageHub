use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Diagnostics::Debug::IsDebuggerPresent;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::Controls::{HOVER_DEFAULT, WM_MOUSEHOVER, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, ReleaseCapture, SetCapture, TrackMouseEvent, TME_HOVER, TME_LEAVE,
    TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app_settings::{
    self, load_settings, save_settings, LegacyPlacement, PlacementOverride, SettingsFile,
    POLL_15_MIN, POLL_15_MIN_SECONDS, POLL_1_HOUR, POLL_1_HOUR_SECONDS, POLL_1_MIN,
    POLL_1_MIN_SECONDS, POLL_5_MIN, POLL_5_MIN_SECONDS,
};
use crate::context_menu::{self, ContextMenuAction, ContextMenuItem, ContextMenuItemKind};
use crate::diagnose;
use crate::localization::{self, LanguageId, Strings};
use crate::models::AppUsageData;
use crate::native_interop::{
    self, TIMER_CLOCK, TIMER_COUNTDOWN, TIMER_CROSSFADE_SCHEDULE, TIMER_MOUSE_CLICK, TIMER_POLL,
    TIMER_RESET_POLL, TIMER_TRAY_HOVER, TIMER_TRAY_REPOSITION, TIMER_UPDATE_CHECK,
    TIMER_WINDOW_STATE, WM_APP_DISABLE_DIAGNOSTICS, WM_APP_ENABLE_DIAGNOSTICS,
    WM_APP_OPEN_DASHBOARD, WM_APP_QUIT, WM_APP_REFRESH_NOW, WM_APP_SETTINGS_UPDATED,
    WM_APP_TASKBAR_COLLISION, WM_APP_TRAY, WM_APP_USAGE_UPDATED,
};
use crate::poller;
use crate::providers::{ProviderId, ProviderSet};
use crate::theme;
use crate::theme_engine::{
    self, Canvas, DataContext, HorizontalAnchor, MouseActionEffect, MouseEventKind,
    ReferenceRegion, SurfaceNest, ThemeDocument, ThemeRuntime, VerticalAnchor,
};
use crate::tray_icon;
use crate::updater::{self, AvailableUpdate, InstallChannel, ReleaseDescriptor, UpdateCheckResult};

fn account_bar_budget(state: &AppState) -> f64 {
    host_geometry::account_bar_budget(state.taskbar_index)
}

const PBT_APMRESUMESUSPEND: u32 = 0x7;

mod app_state;
use app_state::*;
mod polling;
use polling::*;
mod surfaces;
pub(crate) use surfaces::query_theme_runtime_for_surface;
use surfaces::*;
mod tray;
use tray::*;
mod update_flow;
use update_flow::*;
mod watchdog;
use watchdog::*;

fn language_menu_command_id(language: LanguageId) -> u16 {
    IDM_LANG_FIRST.saturating_add(u16::try_from(language.index()).unwrap_or(0))
}

fn language_from_menu_command_id(command: u16) -> Option<LanguageId> {
    command
        .checked_sub(IDM_LANG_FIRST)
        .and_then(|index| LanguageId::from_index(index.into()))
}

fn save_state_settings() {
    let settings_snapshot = {
        let state = lock_state();
        let Some(s) = state.as_ref() else {
            return;
        };

        let mut persisted = load_settings();
        persisted.tray_offset = s.tray_offset;
        persisted.taskbar_index = s.taskbar_index;
        persisted.legacy_placement_pending = false;
        persisted.widget_visible = true;
        persisted.legacy_visibility_pending = false;
        persisted.poll_interval_ms = s.poll_interval_ms;
        persisted.language = s
            .language_override
            .map(|language| language.code().to_string());
        persisted.last_update_check_unix = s.last_update_check_unix;
        persisted.set_enabled_providers(s.providers);
        // Pool flags are only meaningful while Antigravity is enabled; persisting
        // them as on for a disabled provider would re-enable it on the next load.
        let antigravity_on = s.providers.contains(ProviderId::Antigravity);
        persisted.show_antigravity_gemini = Some(antigravity_on && s.show_antigravity_gemini);
        persisted.show_antigravity_external = Some(antigravity_on && s.show_antigravity_external);
        persisted.custom_theme_enabled = s.custom_theme_enabled;
        persisted.active_theme_path = s
            .active_theme_path
            .as_ref()
            .map(|path| path.to_string_lossy().to_string());
        persisted.placement_override = s.placement_override.clone();
        persisted.floating_card_opacity = s.floating_card_opacity;
        persisted.lock_taskbar = s.lock_taskbar;
        persisted
    };

    save_settings_or_log(&settings_snapshot, "unable to save settings");
}

fn save_settings_or_log(settings: &SettingsFile, context: &str) {
    if let Err(error) = save_settings(settings) {
        diagnose::log(format!("{context}: {error}"));
    }
}

pub(super) fn toggle_taskbar_lock(hwnd: HWND) {
    let enabled = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        state.lock_taskbar = !state.lock_taskbar;
        state.lock_taskbar
    };
    save_state_settings();
    if enabled {
        redock_locked_widget(hwnd);
    }
    diagnose::log(format!("taskbar lock toggled: {enabled}"));
}

fn taskbar_created_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe {
        let name = native_interop::wide_str("TaskbarCreated");
        RegisterWindowMessageW(PCWSTR::from_raw(name.as_ptr()))
    })
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "UsageHub";

/// Returns true only if the startup registry value points to this executable.
pub(crate) fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
    }
}

pub(crate) fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                // Write the wide string including null terminator
                let byte_len = ((len + 1) * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    None,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        exe_buf.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

pub fn run() {
    let run_args: Vec<String> = std::env::args().collect();
    let open_dashboard_on_start = run_args.iter().any(|argument| argument == "--dashboard");
    let allow_multiple = run_args
        .iter()
        .any(|argument| argument == "--allow-multiple");
    let no_poll = run_args.iter().any(|argument| argument == "--no-poll");
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running in this session.
    // Use the local namespace so other users' desktop/RDP sessions remain independent.
    // Exception: when relaunched after a UI hang (ENV_RELAUNCH set),
    // wait for the previous instance to release the mutex, then take over.
    let is_relaunch = std::env::var(ENV_RELAUNCH).is_ok();
    let mutex_name = native_interop::wide_str(&if allow_multiple {
        format!("Local\\UsageHub-{}", std::process::id())
    } else {
        "Local\\UsageHub".to_string()
    });
    let _mutex = unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    if is_relaunch {
                        diagnose::log("relaunch: waiting for previous instance to exit");
                        let wait_result = WaitForSingleObject(h, 10_000);
                        if wait_result != WAIT_OBJECT_0 && wait_result != WAIT_ABANDONED {
                            diagnose::log(format!(
                                "startup aborted: previous instance did not exit cleanly ({wait_result:?})"
                            ));
                            return;
                        }
                    } else {
                        if open_dashboard_on_start {
                            if let Err(error) = crate::dashboard::request_from_existing_monitor() {
                                crate::dashboard::report_launch_failure(HWND::default(), &error);
                            }
                        }
                        diagnose::log("startup aborted: another instance is already running");
                        return;
                    }
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("UsageHub");

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(instance) => instance,
            Err(error) => {
                diagnose::log_error("startup aborted: GetModuleHandleW failed", error);
                return;
            }
        };
        let (large_icon, small_icon) = tray_icon::load_app_icons();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
            lpfnWndProc: Some(lifecycle::controller_wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hIcon: large_icon,
            hIconSm: small_icon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let mut settings = load_settings();
        let classic_theme_path = theme_engine::ensure_starter_theme().ok();
        let mut configured_theme_path = settings.active_theme_path.as_deref().map(PathBuf::from);
        let mut configured_theme = configured_theme_path
            .as_deref()
            .and_then(|path| theme_engine::load_theme(path).ok())
            .filter(|theme| !theme.is_obsolete_studio_starter());
        let legacy_placement = settings.legacy_placement();
        let legacy_visibility = settings.legacy_widget_visibility();
        if legacy_placement.is_some() || legacy_visibility.is_some() {
            if configured_theme
                .as_ref()
                .is_some_and(|theme| !theme.is_builtin_classic())
            {
                // A user-selected writable theme already owns its presentation.
                // Consume the obsolete settings without replacing that theme.
                settings.consume_legacy_placement();
                settings.consume_legacy_widget_visibility();
                save_settings_or_log(&settings, "unable to consume legacy settings");
            } else if legacy_placement.is_some() || legacy_visibility == Some(false) {
                let placement = legacy_placement.map(migrated_theme_placement);
                let migrated = ThemeDocument::migrated_from_legacy(
                    placement,
                    legacy_visibility.unwrap_or(true),
                );
                match theme_engine::save_theme(&migrated) {
                    Ok(path) => {
                        configured_theme_path = Some(path.clone());
                        configured_theme = Some(migrated);
                        settings.active_theme_path = Some(path.to_string_lossy().into_owned());
                        settings.custom_theme_enabled = true;
                        settings.consume_legacy_placement();
                        settings.consume_legacy_widget_visibility();
                        if let Err(error) = save_settings(&settings) {
                            diagnose::log(format!(
                                "migrated theme created but settings cleanup failed: {error}"
                            ));
                        } else {
                            diagnose::log(
                                "legacy placement and visibility migrated to Migrated Theme",
                            );
                        }
                    }
                    Err(error) => diagnose::log(format!(
                        "legacy theme migration deferred because the copied theme could not be saved: {error}"
                    )),
                }
            } else {
                // An explicitly visible v1.4.9 widget already matches the
                // built-in theme's Render value, so no copy is necessary.
                settings.consume_legacy_widget_visibility();
                save_settings_or_log(&settings, "unable to consume legacy visibility");
            }
        }
        // Seed selected accounts for a fresh namespace; the built-in Classic
        // remains the default and explicit theme selections are untouched.
        if configured_theme.is_none() && settings.active_theme_path.is_none() {
            let enabled = settings.enabled_providers();
            if crate::manager::widget::visible_cards(&settings.accounts, None, enabled).is_empty() {
                let mut seeded = Vec::new();
                for provider in [ProviderId::Claude, ProviderId::Codex] {
                    if !enabled.contains(provider) {
                        continue;
                    }
                    let selected = settings
                        .accounts
                        .get(provider)
                        .and_then(|configured| configured.selected())
                        .map(|profile| profile.id.clone());
                    if let Some(id) = selected {
                        let target = match provider {
                            ProviderId::Claude => &mut settings.accounts.claude,
                            ProviderId::Codex => &mut settings.accounts.codex,
                            _ => continue,
                        };
                        if let Some(profile) = target.find_mut(&id) {
                            if !profile.pinned {
                                profile.pinned = true;
                                seeded.push(format!("{}/{}", provider.descriptor().key, id));
                            }
                        }
                    }
                }
                if !seeded.is_empty() {
                    save_settings_or_log(&settings, "unable to persist seeded account pins");
                    diagnose::log(format!("seeded initial account pins: {}", seeded.join(",")));
                }
            }
        }
        let (active_theme_path, active_theme) = configured_theme
            .map(|theme| (configured_theme_path, Some(theme)))
            .unwrap_or_else(|| {
                let path = classic_theme_path;
                let theme = path
                    .as_deref()
                    .and_then(|path| theme_engine::load_theme(path).ok())
                    .or_else(|| Some(ThemeDocument::starter()));
                (path, theme)
            });
        let custom_theme_enabled = true;
        let theme_clock_interval = active_theme
            .as_ref()
            .and_then(ThemeDocument::current_time_refresh_interval);
        let tray_theme_uses_current_time = active_theme
            .as_ref()
            .is_some_and(theme_tray_uses_current_time);
        if let Some(path) = &active_theme_path {
            let path = path.to_string_lossy().into_owned();
            if settings.active_theme_path.as_deref() != Some(path.as_str())
                || !settings.custom_theme_enabled
            {
                settings.active_theme_path = Some(path);
                settings.custom_theme_enabled = true;
                save_settings_or_log(&settings, "unable to persist active theme");
            }
        }
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();

        refresh_theme_host_geometry();

        // Hidden top-level lifetime owner: receives shell broadcasts and is
        // never parented into Explorer. The public class stays stable for IPC.
        let title = native_interop::wide_str(language.strings().window_title);
        let controller = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(hinstance.0)),
            None,
        ) {
            Ok(controller) => controller,
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create application controller",
                    error,
                );
                return;
            }
        };
        CONTROLLER_HWND.store(SendHwnd::from_hwnd(controller).0, Ordering::Relaxed);
        let hwnd = lifecycle::create_primary_surface();
        if hwnd.is_invalid() {
            let _ = DestroyWindow(controller);
            return;
        }

        if !large_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(large_icon.0 as isize)),
            );
        }
        if !small_icon.is_invalid() {
            let _ = SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small_icon.0 as isize)),
            );
        }

        diagnose::log(format!("main window created hwnd={:?}", hwnd));
        diagnose::log(format!("controller window created hwnd={controller:?}"));

        let is_dark = theme::is_dark_mode();
        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                providers: settings.enabled_providers(),
                show_antigravity_gemini: settings.antigravity_gemini_enabled(),
                show_antigravity_external: settings.antigravity_external_enabled(),
                crossfade_opacity: 1.0,
                accounts: settings.accounts.clone(),
                data: None,
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                force_notify_auth_error: false,
                auth_error_paused_polling: false,
                auth_watch_mode: poller::CredentialWatchMode::ActiveSource(
                    settings.enabled_providers().first().unwrap_or_default(),
                ),
                auth_watch_snapshot: Vec::new(),
                last_poll_ok: false,
                last_poll_failure: None,
                update_status: UpdateStatus::Idle,
                last_update_check_unix: settings.last_update_check_unix,
                taskbar_index: settings.taskbar_index,
                tray_offset: settings.tray_offset,
                dragging: false,
                pending_drag: false,
                drag_start_cursor: POINT::default(),
                drag_start_origin: POINT::default(),
                drag_start_client_x: 0,
                auto_ejected: false,
                auto_ejected_for_capacity: false,
                auto_ejected_origin: None,
                auto_ejected_host: None,
                lock_taskbar: settings.lock_taskbar,
                is_switching_window_style: false,
                is_snapped: false,
                placement_override: settings.placement_override.clone(),
                floating_card_opacity: settings.floating_card_opacity,
                window_state_timer_active: false,
                custom_theme_enabled,
                usage_countdown: settings.usage_countdown,
                active_theme_path,
                active_theme,
                theme_clock_interval,
                tray_theme_uses_current_time,
                mirror_hwnds: Vec::new(),
                desktop_hwnds: Vec::new(),
                mouse_action_overrides: HashMap::new(),
                hovered_mouse_layer: None,
                pending_mouse_click: None,
                suppress_next_left_up: false,
            });
        }

        if let Some(state) = lock_state().as_ref() {
            publish_update_status(state);
        }
        if let Err(error) = crate::dashboard::start_request_listener(controller) {
            diagnose::log_error("dashboard request listener failed", error);
        }

        sync_surface_windows();
        native_interop::make_popup(hwnd, false);

        // Register the persistent application tray icon.
        if !no_poll {
            sync_tray_icon(hwnd);
        }

        // Theme surfaces decide whether their windows render.
        position_at_taskbar();
        diagnose::log("window shown");

        // Initial render using the presenter selected by the surface nest.
        schedule_countdown_timer();
        schedule_clock_timer();
        let startup_dual_mode = {
            let state = lock_state();
            state.as_ref().is_some_and(|s| {
                s.show_antigravity_gemini
                    && s.show_antigravity_external
                    && s.providers.contains(ProviderId::Antigravity)
            })
        };
        if startup_dual_mode {
            arm_crossfade_schedule(hwnd);
        }
        render_layered();

        if open_dashboard_on_start {
            crate::dashboard::show(controller_for(hwnd));
        }

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        set_controller_timer(Some(hwnd), TIMER_POLL, initial_poll_ms, None);
        sync_window_state_timer(hwnd);
        // Expired deletion-undo snapshots purge on a worker thread: at startup
        // and hourly afterwards. The controller never blocks on it.
        set_controller_timer(
            Some(hwnd),
            TIMER_BACKUP_PURGE,
            BACKUP_PURGE_INTERVAL_MS,
            None,
        );
        crate::manager::widget::schedule_backup_purge();
        // Real shortcut/COM stamping on a worker: at startup, on settings
        // updates, on resume, and every 20s (coalesced, never UI-blocking).
        set_controller_timer(
            Some(hwnd),
            TIMER_TASKBAR_IDENTITIES,
            TASKBAR_IDENTITIES_INTERVAL_MS,
            None,
        );
        crate::manager::widget::schedule_taskbar_identity_refresh("startup");
        // Local account discovery: a throttled worker (default every 6 hours)
        // plus an explicit "Scan for accounts" menu action.
        set_controller_timer(
            Some(hwnd),
            TIMER_DISCOVERY,
            DISCOVERY_TICK_INTERVAL_MS,
            None,
        );
        crate::manager::widget::schedule_account_discovery(false);

        // Sample collision geometry off-thread and ask the stable controller to
        // reconcile shell-hosted windows every two seconds.
        taskbar_collision::spawn_reader();
        spawn_taskbar_watchdog();
        spawn_ui_hang_watchdog();

        // Initial poll
        if !no_poll {
            diagnose::log("initial poll requested");
            request_poll(hwnd);
        }

        if !no_poll {
            schedule_auto_update_check(hwnd);
        }
        let should_check_updates = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| auto_update_check_due(s.last_update_check_unix))
                .unwrap_or(false)
        };
        if should_check_updates && !no_poll {
            begin_update_check(hwnd, false);
        }

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        taskbar_zorder::remove_reorder_hook();
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

/// All widget surfaces use the same wall-clock phase, including after a
/// settings change or a delayed timer following sleep.
fn antigravity_crossfade_frame(now_ms: u64) -> (f64, u32) {
    let phase = now_ms % 20_000;
    let smooth = |elapsed: u64| {
        let t = elapsed as f64 / 2_000.0;
        t * t * (3.0 - 2.0 * t)
    };
    match phase {
        0..8_000 => (1.0, (8_000 - phase) as u32),
        8_000..10_000 => (1.0 - smooth(phase - 8_000), 30),
        10_000..18_000 => (0.0, (18_000 - phase) as u32),
        _ => (smooth(phase - 18_000), 30),
    }
}

pub(super) fn arm_crossfade_schedule(hwnd: HWND) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let (opacity, delay_ms) = antigravity_crossfade_frame(now_ms);
    let mut state = lock_state();
    let dual_mode = state.as_ref().is_some_and(|state| {
        state.show_antigravity_gemini
            && state.show_antigravity_external
            && state.providers.contains(ProviderId::Antigravity)
    });
    if let Some(state) = state.as_mut() {
        state.crossfade_opacity = if dual_mode { opacity } else { 1.0 };
    }
    unsafe {
        if dual_mode {
            let _ = set_controller_timer(Some(hwnd), TIMER_CROSSFADE_SCHEDULE, delay_ms, None);
        } else {
            let _ = kill_controller_timer(Some(hwnd), TIMER_CROSSFADE_SCHEDULE);
        }
    }
}

fn reload_external_settings(hwnd: HWND) {
    let settings = load_settings();
    let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
    let theme_path = settings.active_theme_path.as_ref().map(PathBuf::from);
    let providers_changed;
    let lock_enabled;
    let antigravity_changed;
    let dual_mode;
    {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        providers_changed =
            state.providers != settings.enabled_providers() || state.accounts != settings.accounts;
        let gemini_enabled = settings.antigravity_gemini_enabled();
        let external_enabled = settings.antigravity_external_enabled();
        antigravity_changed = state.show_antigravity_gemini != gemini_enabled
            || state.show_antigravity_external != external_enabled;
        state.show_antigravity_gemini = gemini_enabled;
        state.show_antigravity_external = external_enabled;
        state.accounts = settings.accounts.clone();
        if let Some(data) = state.data.as_mut() {
            data.select_accounts(&settings.accounts);
        }
        state.poll_interval_ms = settings.poll_interval_ms;
        state.providers = settings.enabled_providers();
        state.usage_countdown = settings.usage_countdown;
        dual_mode = state.show_antigravity_gemini
            && state.show_antigravity_external
            && state.providers.contains(ProviderId::Antigravity);
        if !dual_mode {
            unsafe {
                let _ = kill_controller_timer(Some(hwnd), TIMER_CROSSFADE_SCHEDULE);
            }
            state.crossfade_opacity = 1.0;
        }
        update_configured_placement(
            state,
            settings.placement_override,
            settings.taskbar_index,
            settings.tray_offset,
        );
        state.floating_card_opacity = settings.floating_card_opacity;
        state.lock_taskbar = settings.lock_taskbar;
        lock_enabled = state.lock_taskbar;
        apply_language_to_state(state, language_override);
    }
    if dual_mode && (antigravity_changed || providers_changed) {
        arm_crossfade_schedule(hwnd);
    }
    unsafe {
        set_controller_timer(Some(hwnd), TIMER_POLL, settings.poll_interval_ms, None);
    }
    let _ = apply_custom_theme(hwnd, settings.custom_theme_enabled, theme_path, None);
    if lock_enabled {
        // Re-embed a previously ejected widget so the lock takes effect at once.
        redock_locked_widget(hwnd);
    }
    if providers_changed {
        request_poll(hwnd);
    }
    // Regenerated layouts are picked up by render_layered through the
    // expanded effective theme. Real shortcut/COM stamping runs on a worker
    // (coalesced), never on the UI thread.
    crate::manager::widget::schedule_taskbar_identity_refresh("settings");
    sync_tray_icon(hwnd);
    position_at_taskbar();
    render_layered();
}

/// Use the configured host, not the transient popup created by a drag or
/// auto-ejection. An intentional floating placement takes precedence over the
/// theme's authored host.
fn taskbar_lock_applies(state: &AppState) -> bool {
    if !state.lock_taskbar {
        return false;
    }
    match state.placement_override.as_ref().map(|p| p.nest.as_str()) {
        Some("taskbar") => true,
        Some("floating") => false,
        _ => state.active_theme.as_ref().is_some_and(|theme| {
            theme.surfaces.first().is_some_and(|surface| {
                surface
                    .placement
                    .nest
                    .resolve(surface.placement.reference.region)
                    == SurfaceNest::Taskbar
            })
        }),
    }
}

fn clear_locked_auto_ejection(state: &mut AppState) -> bool {
    if !taskbar_lock_applies(state) || !state.auto_ejected {
        return false;
    }
    clear_auto_ejection(state);
    true
}

/// Restore a locked taskbar widget after auto-ejection. Intentional floating,
/// desktop and tray hosts retain their placement.
fn redock_locked_widget(hwnd: HWND) {
    // Snapshot AppState so Win32 queries run without holding STATE.
    let Some(snapshot) = snapshot_state() else {
        return;
    };
    if !taskbar_lock_applies(&snapshot) || !snapshot.auto_ejected {
        return;
    }
    // Locking suppresses collisions with app buttons, but must not restore
    // a mostly hidden layout into the parent. Use the same rule as ejection.
    if let Some(taskbar) = snapshot.taskbar_hwnd.map(SendHwnd::to_hwnd) {
        let Some(bounds) = native_interop::get_taskbar_rect(taskbar) else {
            return;
        };
        let Some(target) = restored_dock_rect(&snapshot, taskbar, bounds) else {
            return;
        };
        if !taskbar_collision::taskbar_shown(bounds)
            || positioning::taskbar_ejection_required(bounds, target, false, true)
        {
            return;
        }
    }
    let taskbar_hwnd = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        if !clear_locked_auto_ejection(state) {
            return;
        }
        state.is_switching_window_style = true;
        state.taskbar_hwnd.map(|h| h.to_hwnd())
    };
    // Re-embed first so position_at_taskbar sees a docked window.
    if let Some(taskbar) = taskbar_hwnd {
        native_interop::embed_as_child(hwnd, taskbar);
        diagnose::log("taskbar lock enabled: re-docked floating widget");
    }
    if let Some(state) = lock_state().as_mut() {
        state.is_switching_window_style = false;
    }
    position_at_taskbar();
    render_layered();
}

mod lifecycle;
use lifecycle::{controller_for, kill_controller_timer, set_controller_timer, WM_APP_RECONCILE};
mod host_geometry;
mod message_loop;
use host_geometry::*;
use message_loop::wnd_proc;
mod positioning;
mod taskbar_collision;
mod taskbar_zorder;
use positioning::*;
mod mouse;
mod widget_tooltip;
use mouse::*;
mod window_context_menu;
use window_context_menu::*;

#[cfg(test)]
mod placement_tests;

#[cfg(test)]
mod layered_window_tests;

#[cfg(test)]
mod language_menu_tests {
    use super::*;

    #[test]
    fn generated_language_menu_commands_round_trip() {
        assert_eq!(language_from_menu_command_id(IDM_LANG_SYSTEM), None);
        for language in LanguageId::ALL {
            assert_eq!(
                language_from_menu_command_id(language_menu_command_id(language)),
                Some(language)
            );
        }
    }
}

#[cfg(test)]
mod placement_regression_tests;

#[cfg(test)]
mod antigravity_crossfade_tests {
    use super::antigravity_crossfade_frame;

    #[test]
    fn pool_crossfade_follows_the_same_phase_after_startup_sleep_and_rearming() {
        for cycle in [0, 20_000, 180_000] {
            for (phase, opacity, delay) in [
                (0, 1.0, 8_000),
                (7_000, 1.0, 1_000),
                (8_000, 1.0, 30),
                (9_000, 0.5, 30),
                (10_000, 0.0, 8_000),
                (17_000, 0.0, 1_000),
                (18_000, 0.0, 30),
                (19_000, 0.5, 30),
            ] {
                assert_eq!(antigravity_crossfade_frame(cycle + phase), (opacity, delay));
            }
        }
        let before_wrap = antigravity_crossfade_frame(19_999).0;
        assert!((1.0 - before_wrap).abs() < 0.000_001);
    }
}
