//! Theme surface creation, DPI/scale resolution, placement, and layered rendering.

use super::*;

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
pub(super) fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

pub(super) fn display_scale(display_index: usize) -> f64 {
    let displays = native_interop::find_monitors();
    let Some(display) = displays
        .get(display_index)
        .copied()
        .or_else(|| displays.first().copied())
    else {
        return 1.0;
    };
    monitor_scale(display)
}

pub(super) fn monitor_scale(display: native_interop::DisplayMonitor) -> f64 {
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    if unsafe { GetDpiForMonitor(display.handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
        && dpi_x > 0
    {
        (dpi_x as f64 / 96.0).clamp(0.25, 8.0)
    } else {
        let system_dpi = unsafe { GetDpiForSystem() };
        if system_dpi > 0 {
            (system_dpi as f64 / 96.0).clamp(0.25, 8.0)
        } else {
            1.0
        }
    }
}

pub(super) fn migrated_theme_placement(legacy: LegacyPlacement) -> (usize, i32) {
    let displays = native_interop::find_monitors();
    let taskbars = native_interop::find_taskbars();
    let display_index = taskbars
        .get(legacy.taskbar_index)
        .or_else(|| taskbars.first())
        .and_then(|taskbar| native_interop::monitor_for_taskbar(&taskbars, &displays, taskbar.hwnd))
        .and_then(|monitor| {
            displays
                .iter()
                .position(|display| display.handle == monitor)
        })
        .unwrap_or_else(|| legacy.taskbar_index.min(displays.len().saturating_sub(1)));
    let offset_x = legacy_offset_to_theme_offset(legacy.tray_offset, display_scale(display_index));
    (display_index, offset_x)
}

pub(super) fn legacy_offset_to_theme_offset(tray_offset: i32, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    -((tray_offset.max(0) as f64 / scale).round() as i32)
}

pub(super) fn theme_surface_scale(theme: &ThemeDocument, surface_index: usize) -> f64 {
    let display_index = theme
        .surfaces
        .get(surface_index)
        .map(|surface| surface.placement.reference.display)
        .unwrap_or(theme.placement.reference.display);
    display_scale(display_index)
}

pub(super) fn logical_host_dimension(physical: i32, scale: f64) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (physical.max(1) as f64 / scale)
        .round()
        .clamp(1.0, u32::MAX as f64) as u32
}

// Studio is a separate process and does not receive the bar's layout snapshots.
// Bound shell enumeration even when an editor resolves many surfaces per frame.
static STUDIO_HOST_NEXT_CHECK: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) fn query_theme_runtime_for_surface(
    theme: &ThemeDocument,
    surface_index: usize,
    runtime: ThemeRuntime,
) -> ThemeRuntime {
    let now = Instant::now();
    let refresh = {
        let mut next = STUDIO_HOST_NEXT_CHECK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if next.is_some_and(|deadline| now < deadline) {
            false
        } else {
            // Set the deadline before native calls: shell queries can dispatch
            // messages and reenter this function. Never hold the lock over them.
            *next = Some(now + Duration::from_secs(1));
            true
        }
    };
    if refresh {
        host_geometry::refresh_theme_host_geometry();
    }
    host_geometry::theme_runtime_for_surface(theme, surface_index, runtime)
}

pub(super) fn scaled_theme_dimension(logical: u32, scale: f64) -> i32 {
    (logical as f64 * scale).round().clamp(1.0, 8192.0) as i32
}

pub(super) fn theme_runtime_from_state(state: &AppState) -> ThemeRuntime {
    let (poll_ok, has_error) = poll_display_state(
        state.last_poll_ok,
        state.retry_count,
        state.auth_error_paused_polling,
        state.data.as_ref(),
    );
    let nest = if state.auto_ejected {
        SurfaceNest::Floating
    } else if let Some(ref ov) = state.placement_override {
        if ov.nest == "floating" {
            SurfaceNest::Floating
        } else {
            SurfaceNest::Taskbar
        }
    } else {
        SurfaceNest::Taskbar
    };
    let opacity = state.floating_card_opacity.unwrap_or(85);
    ThemeRuntime::from_providers(state.providers)
        .with_poll_state(poll_ok, has_error)
        .with_language(state.language)
        .with_countdown(state.usage_countdown)
        .with_nest(nest)
        .with_floating_card_opacity(opacity)
        .with_antigravity_pools(
            state.show_antigravity_gemini,
            state.show_antigravity_external,
            state.crossfade_opacity,
        )
}

pub(super) fn effective_theme_from_state(state: &AppState) -> Option<ThemeDocument> {
    theme_with_placement(state, state.auto_ejected)
}

pub(super) fn theme_with_placement(state: &AppState, auto_ejected: bool) -> Option<ThemeDocument> {
    let mut theme = state.active_theme.as_ref().map(|theme| {
        theme_engine::apply_mouse_action_overrides(theme, &state.mouse_action_overrides)
    })?;
    let floating = if auto_ejected {
        state.auto_ejected_origin.map(|point| (None, point))
    } else {
        state
            .placement_override
            .as_ref()
            .filter(|p| p.nest == "floating")
            .map(|p| {
                (
                    Some((p.monitor_index, p.monitor_device.clone())),
                    POINT {
                        x: p.screen_x,
                        y: p.screen_y,
                    },
                )
            })
    };
    if let Some((saved_monitor, point)) = floating {
        let displays = native_interop::find_monitors();
        let selected = saved_monitor
            .and_then(|(index, device)| {
                positioning::resolve_saved_monitor(&displays, index, device.as_deref())
            })
            .and_then(|index| displays.get(index).copied().map(|d| (index, d)))
            .unwrap_or_else(|| positioning::monitor_for_point(&displays, point));
        apply_floating_position(&mut theme, state, selected.0, selected.1, point);
    } else if let Some(p) = state
        .placement_override
        .as_ref()
        .filter(|p| p.nest == "taskbar")
    {
        let displays = native_interop::find_monitors();
        let index = positioning::resolve_saved_monitor(
            &displays,
            p.monitor_index,
            p.monitor_device.as_deref(),
        )
        .unwrap_or(0);
        let horizontal = taskbar_is_horizontal(index);
        let placement = if p.screen_x > 0 || p.screen_y > 0 {
            let offset = if horizontal { p.screen_x } else { p.screen_y };
            positioning::taskbar_dock_placement(index, offset, display_scale(index), horizontal)
        } else {
            positioning::dock_placement(index, p.tray_offset, display_scale(index), horizontal)
        };
        positioning::override_primary_placement(&mut theme, placement);
    }
    // Dynamic account slots: regenerate columns/rows from the pinned registry
    // on every layout. Non-aware custom themes pass through untouched; the
    // third pinned account appears without any manual theme JSON.
    let budget = host_geometry::account_bar_budget(state.taskbar_index);
    let cards = crate::manager::widget::visible_cards(
        &state.accounts,
        state.data.as_ref(),
        state.providers,
    );
    let mut layout = theme_engine::account_layout::layout_accounts(&cards, budget);
    if theme_engine::account_layout::is_left_dock_classic(&theme) {
        if let Some(surface_index) = theme
            .surfaces
            .iter()
            .position(|surface| surface.id == "main")
        {
            loop {
                let expanded =
                    theme_engine::account_layout::expand_theme_for_accounts(&theme, &layout);
                let runtime = theme_runtime_for_surface(
                    &expanded,
                    surface_index,
                    theme_runtime_from_state(state),
                );
                let (width, _) = theme_engine::resolve_surface_size(
                    &expanded,
                    surface_index,
                    state.data.as_ref(),
                    runtime,
                );
                if f64::from(width) <= budget || layout.visible.len() <= 1 {
                    theme = expanded;
                    break;
                }
                let Some(card) = layout.visible.pop() else {
                    theme = expanded;
                    break;
                };
                layout.overflow.insert(0, card);
            }
        } else {
            theme = theme_engine::account_layout::expand_theme_for_accounts(&theme, &layout);
        }
    } else {
        theme = theme_engine::account_layout::expand_theme_for_accounts(&theme, &layout);
    }
    Some(theme)
}

fn apply_floating_position(
    theme: &mut ThemeDocument,
    state: &AppState,
    index: usize,
    display: native_interop::DisplayMonitor,
    point: POINT,
) {
    let mut placement = positioning::floating_placement(index);
    let saved_host = if state.auto_ejected {
        state.auto_ejected_host.as_ref()
    } else {
        state
            .placement_override
            .as_ref()
            .and_then(|p| p.floating_host.as_ref())
    };
    placement.host_dimensions =
        floating_host_for_theme(theme, saved_host).map(|host| (host.width, host.height));
    positioning::override_primary_placement(theme, placement.clone());
    let scale = monitor_scale(display);
    let runtime = theme_runtime_for_surface(theme, 0, theme_runtime_from_state(state));
    let frame = positioning::widget_frame(theme, state.data.as_ref(), runtime, scale);
    let offset = positioning::clamped_floating_offset(point, display.rect, &frame, scale);
    placement.offset_x = offset.x;
    placement.offset_y = offset.y;
    positioning::override_primary_placement(theme, placement);
}

/// Resolve the actual rectangle that will be restored, including authored
/// placement or a saved drag offset. The watchdog must test this same target.
pub(super) fn restored_dock_rect(
    state: &AppState,
    taskbar: HWND,
    taskbar_rect: RECT,
) -> Option<RECT> {
    let theme = theme_with_placement(state, false)?;
    let surface = theme.surfaces.first()?;
    if surface
        .placement
        .nest
        .resolve(surface.placement.reference.region)
        != SurfaceNest::Taskbar
    {
        return None;
    }
    let displays = native_interop::find_monitors();
    let display = displays
        .get(surface.placement.reference.display)
        .or_else(|| displays.first())?;
    if native_interop::taskbar_for_monitor(
        &native_interop::find_taskbars(),
        &displays,
        display.handle,
    )
    .map(|candidate| candidate.hwnd)
        != Some(taskbar)
    {
        return None;
    }
    let runtime = theme_runtime_for_surface(&theme, 0, theme_runtime_from_state(state));
    let scale = monitor_scale(*display);
    let frame = positioning::widget_frame(&theme, state.data.as_ref(), runtime, scale);
    let offsets = theme_engine::resolve_surface_placement(&theme, 0, state.data.as_ref(), runtime);
    let mut placement = surface.placement.clone();
    placement.offset_x = offsets.offset_x;
    placement.offset_y = offsets.offset_y;
    let tray = native_interop::find_child_window(taskbar, "TrayNotifyWnd")
        .and_then(native_interop::get_window_rect_safe);
    Some(positioning::surface_screen_rect(
        &placement,
        frame.width,
        frame.height,
        scale,
        display.rect,
        Some(taskbar_rect),
        tray,
    ))
}

pub(super) fn theme_has_floating_surface(theme: &ThemeDocument) -> bool {
    theme.surfaces.iter().any(|surface| {
        surface
            .placement
            .nest
            .resolve(surface.placement.reference.region)
            == SurfaceNest::Floating
    })
}

pub(super) fn window_state_timer_required(state: &AppState) -> bool {
    state.custom_theme_enabled
        && effective_theme_from_state(state)
            .as_ref()
            .is_some_and(theme_has_floating_surface)
}

pub(super) fn sync_window_state_timer(hwnd: HWND) {
    let required = lock_state()
        .as_ref()
        .is_some_and(window_state_timer_required);
    set_window_state_timer(hwnd, required);
}

fn set_window_state_timer(hwnd: HWND, required: bool) {
    {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        if required == state.window_state_timer_active {
            return;
        }
        state.window_state_timer_active = required;
    }

    unsafe {
        if required {
            let _ = set_controller_timer(
                Some(hwnd),
                TIMER_WINDOW_STATE,
                WINDOW_STATE_INTERVAL_MS,
                None,
            );
        } else {
            let _ = kill_controller_timer(Some(hwnd), TIMER_WINDOW_STATE);
        }
    }
}

pub(super) fn total_widget_width_for_state(state: &AppState) -> i32 {
    widget_frame_for_state(state, None).width
}

pub(super) fn widget_frame_for_state(
    state: &AppState,
    nest: Option<SurfaceNest>,
) -> positioning::WidgetFrame {
    effective_theme_from_state(state).as_ref().map_or(
        positioning::WidgetFrame {
            width: 1,
            height: 1,
            content_width: 1,
            inset: 0,
        },
        |theme| {
            let runtime = theme_runtime_for_surface(theme, 0, theme_runtime_from_state(state));
            let mut runtime = nest.map_or(runtime, |nest| runtime.with_nest(nest));
            if nest == Some(SurfaceNest::Floating) {
                if let Some(host) = floating_host_for_theme(theme, None) {
                    runtime = runtime.with_host_dimensions(host.width, host.height);
                }
            }
            let scale = theme_surface_scale(theme, 0);
            positioning::widget_frame(theme, state.data.as_ref(), runtime, scale)
        },
    )
}

pub(super) fn floating_frame_for_state(
    state: &AppState,
    display_index: usize,
    scale: f64,
) -> Option<positioning::WidgetFrame> {
    let mut theme = effective_theme_from_state(state)?;
    let mut placement = positioning::floating_placement(display_index);
    placement.host_dimensions =
        floating_host_for_theme(&theme, None).map(|host| (host.width, host.height));
    positioning::override_primary_placement(&mut theme, placement);
    let runtime = theme_runtime_for_surface(&theme, 0, theme_runtime_from_state(state));
    Some(positioning::widget_frame(
        &theme,
        state.data.as_ref(),
        runtime,
        scale,
    ))
}

pub(super) fn clear_auto_ejection(state: &mut AppState) {
    state.auto_ejected = false;
    state.auto_ejected_for_capacity = false;
    state.auto_ejected_origin = None;
    state.auto_ejected_host = None;
}

pub(super) fn replace_active_theme(
    state: &mut AppState,
    loaded: ThemeDocument,
    path: Option<PathBuf>,
) {
    // A fallback belongs to the configured root that was ejected. Preserve it
    // during ordinary refreshes, but never carry it into a new theme or host.
    let changed = state.active_theme.as_ref().is_none_or(|current| {
        current.id != loaded.id
            || current.surfaces.first().map(|s| (&s.id, &s.placement))
                != loaded.surfaces.first().map(|s| (&s.id, &s.placement))
    }) || path
        .as_ref()
        .is_some_and(|p| state.active_theme_path.as_ref() != Some(p));
    if changed {
        clear_auto_ejection(state);
    }
    state.active_theme = Some(loaded);
    if path.is_some() {
        state.active_theme_path = path;
    }
}

pub(super) fn update_configured_placement(
    state: &mut AppState,
    placement: Option<PlacementOverride>,
    taskbar_index: usize,
    tray_offset: i32,
) {
    if state.placement_override != placement
        || state.taskbar_index != taskbar_index
        || state.tray_offset != tray_offset
    {
        clear_auto_ejection(state);
    }
    state.placement_override = placement;
    state.taskbar_index = taskbar_index;
    state.tray_offset = tray_offset;
}

pub(super) fn apply_custom_theme(
    hwnd: HWND,
    _enabled: bool,
    path: Option<PathBuf>,
    document: Option<ThemeDocument>,
) -> Result<(), String> {
    let loaded = match (document, path.as_deref()) {
        (Some(document), _) => Some(document),
        (None, Some(path)) => Some(theme_engine::load_theme(path)?),
        (None, None) => lock_state()
            .as_ref()
            .and_then(|state| state.active_theme.clone()),
    };
    let loaded = loaded.unwrap_or_else(ThemeDocument::starter);
    let theme_clock_interval = loaded.current_time_refresh_interval();
    let tray_theme_uses_current_time = theme_tray_uses_current_time(&loaded);
    let (detach_to_popup, old_hook) = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return Err("Application is not ready".into());
        };
        state.custom_theme_enabled = true;
        replace_active_theme(state, loaded, path);
        state.theme_clock_interval = theme_clock_interval;
        state.tray_theme_uses_current_time = tray_theme_uses_current_time;
        state.mouse_action_overrides.clear();
        state.hovered_mouse_layer = None;
        state.pending_mouse_click = None;
        state.suppress_next_left_up = false;
        let stays_taskbar_docked = state.embedded
            && !state.auto_ejected
            && state
                .placement_override
                .as_ref()
                .map(|p| p.nest.as_str())
                .unwrap_or_else(|| {
                    match state
                        .active_theme
                        .as_ref()
                        .and_then(|t| t.surfaces.first())
                        .map(|s| s.placement.nest.resolve(s.placement.reference.region))
                    {
                        Some(SurfaceNest::Taskbar) => "taskbar",
                        _ => "other",
                    }
                })
                == "taskbar";
        let detach = !stays_taskbar_docked;
        let hook = if detach {
            state.embedded = false;
            state.win_event_hook.take()
        } else {
            None
        };
        (detach, hook)
    };
    if let Some(hook) = old_hook {
        native_interop::unhook_win_event(hook.to_hook());
    }
    unsafe {
        if detach_to_popup {
            native_interop::make_popup(hwnd, false);
        }
        ensure_layered_window(hwnd);
        if detach_to_popup {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_NOTOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
    sync_surface_windows();
    sync_window_state_timer(hwnd);
    schedule_countdown_timer();
    schedule_clock_timer();
    Ok(())
}

pub(super) fn sync_surface_windows() {
    sync_surface_windows_with(|| unsafe { create_desktop_surface_window() });
}

pub(super) fn sync_surface_windows_with(mut create_desktop: impl FnMut() -> HWND) {
    let primary = lock_state().as_ref().map(|state| state.hwnd.to_hwnd());
    let Some(primary) = primary else {
        return;
    };
    if !lifecycle::surface_has_class(primary, "UsageHubSurface") {
        let replacement = unsafe { lifecycle::create_primary_surface() };
        if replacement.is_invalid() {
            diagnose::log_recovery("unable to recreate primary surface; will retry");
            return;
        }
        if let Some(state) = lock_state().as_mut() {
            state.hwnd = SendHwnd::from_hwnd(replacement);
            state.embedded = false;
            state.taskbar_hwnd = None;
            state.tray_notify_hwnd = None;
            state.dragging = false;
            state.pending_drag = false;
            state.auto_ejected = false;
            state.auto_ejected_for_capacity = false;
            state.auto_ejected_origin = None;
            state.auto_ejected_host = None;
            state.is_snapped = false;
            state.is_switching_window_style = false;
            state.pending_mouse_click = None;
            state.hovered_mouse_layer = None;
            set_window_title(replacement, state.language.strings());
        }
        diagnose::log_recovery(format!("recreated primary surface hwnd={replacement:?}"));
    }
    let mirrors = lock_state()
        .as_ref()
        .map(|state| state.mirror_hwnds.clone())
        .unwrap_or_default();
    for (index, mirror) in mirrors.into_iter().enumerate() {
        if !lifecycle::surface_has_class(mirror.to_hwnd(), "UHThemeMirror") {
            let replacement = unsafe { create_mirror_window() };
            // Window creation can re-enter; never index a list it may shrink.
            let orphaned = match lock_state()
                .as_mut()
                .and_then(|state| state.mirror_hwnds.get_mut(index))
            {
                Some(slot) => {
                    *slot = SendHwnd::from_hwnd(replacement);
                    false
                }
                None => true,
            };
            if orphaned {
                unsafe { lifecycle::destroy_surface(replacement) };
            }
        }
    }
    let (desired_total, desktop_surfaces) = {
        let state = lock_state();
        state
            .as_ref()
            .map(|state| {
                let surfaces = state
                    .active_theme
                    .as_ref()
                    .map(|theme| {
                        theme
                            .surfaces
                            .iter()
                            .map(|surface| {
                                surface
                                    .placement
                                    .nest
                                    .resolve(surface.placement.reference.region)
                                    == SurfaceNest::Desktop
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                (surfaces.len().max(1), surfaces)
            })
            .unwrap_or_else(|| (1, Vec::new()))
    };
    let desired_mirrors = desired_total.saturating_sub(1);
    loop {
        let remove = {
            let mut state = lock_state();
            state.as_mut().and_then(|state| {
                if state.mirror_hwnds.len() > desired_mirrors {
                    state.mirror_hwnds.pop()
                } else {
                    None
                }
            })
        };
        match remove {
            Some(hwnd) => unsafe {
                lifecycle::destroy_surface(hwnd.to_hwnd());
            },
            None => break,
        }
    }
    while lock_state()
        .as_ref()
        .map(|state| state.mirror_hwnds.len())
        .unwrap_or(0)
        < desired_mirrors
    {
        let mirror = unsafe { create_mirror_window() };
        if mirror.is_invalid() {
            break;
        }
        if let Some(state) = lock_state().as_mut() {
            state.mirror_hwnds.push(SendHwnd::from_hwnd(mirror));
        }
    }

    let stale_desktop_windows = {
        let mut state = lock_state();
        let Some(state) = state.as_mut() else {
            return;
        };
        let removed = if state.desktop_hwnds.len() > desired_total {
            state.desktop_hwnds.split_off(desired_total)
        } else {
            Vec::new()
        };
        state.desktop_hwnds.resize_with(desired_total, || None);
        let mut stale = removed.into_iter().flatten().collect::<Vec<_>>();
        for (surface_index, window) in state.desktop_hwnds.iter_mut().enumerate() {
            let wanted = desktop_surfaces.get(surface_index) == Some(&true);
            let valid = window.is_some_and(|window| {
                lifecycle::surface_has_class(window.to_hwnd(), "UHDesktopSurface")
            });
            if !wanted || !valid {
                if let Some(window) = window.take() {
                    stale.push(window);
                }
            }
        }
        stale
    };
    for window in stale_desktop_windows {
        unsafe {
            if lifecycle::surface_has_class(window.to_hwnd(), "UHDesktopSurface") {
                lifecycle::destroy_surface(window.to_hwnd());
            } else {
                crate::desktop_compositor::remove(window.to_hwnd());
            }
        }
    }
    for (surface_index, wanted) in desktop_surfaces.into_iter().enumerate() {
        if !wanted {
            continue;
        }
        let missing = lock_state()
            .as_ref()
            .and_then(|state| state.desktop_hwnds.get(surface_index))
            .is_none_or(Option::is_none);
        if !missing {
            continue;
        }
        let window = create_desktop();
        if window.is_invalid() {
            continue;
        }
        unsafe {
            let _ = ShowWindow(window, SW_HIDE);
        }
        if let Some(slot) = lock_state()
            .as_mut()
            .and_then(|state| state.desktop_hwnds.get_mut(surface_index))
        {
            *slot = Some(SendHwnd::from_hwnd(window));
        } else {
            unsafe {
                lifecycle::destroy_surface(window);
            }
        }
    }
}

unsafe fn create_desktop_surface_window() -> HWND {
    let Some(desktop) = native_interop::find_desktop_host() else {
        return HWND::default();
    };
    create_desktop_surface_window_in(desktop)
}

pub(super) unsafe fn create_desktop_surface_window_in(
    desktop: native_interop::DesktopHost,
) -> HWND {
    let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
        return HWND::default();
    };
    let class = native_interop::wide_str("UHDesktopSurface");
    let title = native_interop::wide_str("");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(mirror_wnd_proc),
        hInstance: HINSTANCE(instance.0),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszClassName: PCWSTR::from_raw(class.as_ptr()),
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let previous_hosting = SetThreadDpiHostingBehavior(DPI_HOSTING_BEHAVIOR_MIXED);
    let previous_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE);
    let window = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(class.as_ptr()),
        PCWSTR::from_raw(title.as_ptr()),
        WINDOW_STYLE(
            native_interop::WS_CHILD_STYLE | native_interop::WS_CLIPSIBLINGS_STYLE | WS_VISIBLE.0,
        ),
        0,
        0,
        198,
        144,
        Some(desktop.parent),
        None,
        Some(HINSTANCE(instance.0)),
        None,
    )
    .unwrap_or_default();
    let _ = SetThreadDpiAwarenessContext(previous_dpi);
    let _ = SetThreadDpiHostingBehavior(previous_hosting);
    if window.is_invalid() {
        diagnose::log("unable to create raised-desktop surface window");
    }
    window
}

unsafe fn create_mirror_window() -> HWND {
    let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
        return HWND::default();
    };
    let class = native_interop::wide_str("UHThemeMirror");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(mirror_wnd_proc),
        hInstance: HINSTANCE(instance.0),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszClassName: PCWSTR::from_raw(class.as_ptr()),
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let title = native_interop::wide_str("Usage theme mirror");
    CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
        PCWSTR::from_raw(class.as_ptr()),
        PCWSTR::from_raw(title.as_ptr()),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(HINSTANCE(instance.0)),
        None,
    )
    .unwrap_or_default()
}

unsafe extern "system" fn mirror_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_SETCURSOR if set_surface_cursor(hwnd) => LRESULT(1),
        taskbar_zorder::WM_APP_TASKBAR_ZORDER => {
            taskbar_zorder::raise_if_covered(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            update_mouse_hover(hwnd, lparam);
            LRESULT(0)
        }
        WM_MOUSEHOVER => {
            widget_tooltip::show(hwnd);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            clear_mouse_hover(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            widget_tooltip::hide();
            let suppressed = {
                let mut state = lock_state();
                state.as_mut().is_some_and(|state| {
                    let suppressed = state.suppress_next_left_up;
                    state.suppress_next_left_up = false;
                    suppressed
                })
            };
            if !suppressed {
                if let Some((surface, object)) = mouse_target_at(hwnd, lparam) {
                    schedule_or_dispatch_click(hwnd, surface, object);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            widget_tooltip::hide();
            if let Some((surface, object)) = mouse_target_at(hwnd, lparam) {
                dispatch_double_click(hwnd, surface, object);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            widget_tooltip::hide();
            dispatch_surface_context_menu(hwnd, mouse_target_at(hwnd, lparam));
            LRESULT(0)
        }
        WM_CONTEXTMENU if lparam.0 == -1 => {
            widget_tooltip::hide();
            dispatch_keyboard_context_menu(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut paint);
            let _ = EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DESTROY => {
            taskbar_zorder::forget_surface(hwnd);
            lifecycle::surface_destroyed(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub(super) fn total_widget_height_for_state(state: &AppState) -> i32 {
    widget_frame_for_state(state, None).height
}

pub(super) fn total_widget_height() -> i32 {
    lock_state()
        .as_ref()
        .map(total_widget_height_for_state)
        .unwrap_or(1)
}

pub(super) fn total_widget_width() -> i32 {
    lock_state()
        .as_ref()
        .map(total_widget_width_for_state)
        .unwrap_or(1)
}

/// Render every theme surface, then dispatch it to the presenter selected by
/// its nest: DirectComposition for desktop and layered windows elsewhere.
pub(super) fn render_layered() {
    refresh_dpi();
    sync_surface_windows();
    let (hwnd_val, active_theme, usage_data, runtime, mirror_hwnds, desktop_hwnds) = {
        let state = lock_state();
        let Some(state) = state.as_ref() else {
            return;
        };
        (
            state.hwnd,
            effective_theme_from_state(state),
            state.data.clone(),
            theme_runtime_from_state(state),
            state.mirror_hwnds.clone(),
            state.desktop_hwnds.clone(),
        )
    };

    // Theme rendering is the widget renderer. Startup and theme changes always
    // install Classic in memory when a selected theme cannot be loaded.
    let theme = active_theme.unwrap_or_else(ThemeDocument::starter);
    let hwnd = hwnd_val.to_hwnd();
    set_window_state_timer(hwnd, theme_has_floating_surface(&theme));
    let target_count = theme.surfaces.len();
    for surface_index in 0..target_count {
        let regular_hwnd = if surface_index == 0 {
            hwnd
        } else if let Some(mirror) = mirror_hwnds.get(surface_index - 1) {
            mirror.to_hwnd()
        } else {
            continue;
        };
        let surface = &theme.surfaces[surface_index];
        let surface_runtime = theme_runtime_for_surface(&theme, surface_index, runtime);
        let nest = surface
            .placement
            .nest
            .resolve(surface.placement.reference.region);
        let desktop_nested = nest == SurfaceNest::Desktop;
        let target_hwnd = if desktop_nested {
            unsafe {
                let _ = ShowWindow(regular_hwnd, SW_HIDE);
            }
            desktop_hwnds
                .get(surface_index)
                .and_then(|window| *window)
                .map(SendHwnd::to_hwnd)
                .unwrap_or_default()
        } else {
            regular_hwnd
        };
        if target_hwnd.is_invalid() {
            continue;
        }
        if nest == SurfaceNest::TrayIcon {
            unsafe {
                let _ = ShowWindow(target_hwnd, SW_HIDE);
            }
            continue;
        }
        if !theme_engine::surface_should_render(
            &theme,
            surface_index,
            usage_data.as_ref(),
            surface_runtime,
        ) {
            unsafe {
                let _ = ShowWindow(target_hwnd, SW_HIDE);
            }
            continue;
        }

        let scale = theme_surface_scale(&theme, surface_index);
        let rendered = theme_engine::render_theme_surface_with_runtime_at_scale(
            &theme,
            surface_index,
            usage_data.as_ref(),
            surface_runtime,
            scale,
        );
        let mut positioned = theme_for_surface(&theme, surface_index);
        let (logical_width, logical_height) = theme_engine::resolve_surface_size(
            &theme,
            surface_index,
            usage_data.as_ref(),
            surface_runtime,
        );
        positioned.canvas.width = logical_width;
        positioned.canvas.height = logical_height;
        let placement = theme_engine::resolve_surface_placement(
            &theme,
            surface_index,
            usage_data.as_ref(),
            surface_runtime,
        );
        positioned.placement.offset_x = placement.offset_x;
        positioned.placement.offset_y = placement.offset_y;
        let hosted = position_custom_theme(target_hwnd, &positioned, scale);
        if !hosted {
            continue;
        }
        if desktop_nested {
            unsafe {
                let _ = ShowWindow(target_hwnd, SW_SHOWNOACTIVATE);
            }
        }
        render_custom_window(target_hwnd, &rendered, desktop_nested);
        unsafe {
            let show = nest != SurfaceNest::Floating
                || !foreground_is_fullscreen_on_display(positioned.placement.reference.display);
            let _ = ShowWindow(target_hwnd, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
    }

    for target in std::iter::once(hwnd)
        .chain(mirror_hwnds.iter().map(|mirror| mirror.to_hwnd()))
        .skip(target_count)
    {
        unsafe {
            let _ = ShowWindow(target, SW_HIDE);
        }
    }
}

fn theme_for_surface(theme: &ThemeDocument, surface_index: usize) -> ThemeDocument {
    let mut result = theme.clone();
    if let Some(surface) = theme.surfaces.get(surface_index) {
        result.canvas.width_expression = Some(surface.width.clone());
        result.canvas.height_expression = Some(surface.height.clone());
        result.canvas.background = match &surface.background {
            crate::theme_engine::LayerBackground::Colour { colour } => colour.clone(),
            crate::theme_engine::LayerBackground::None
            | crate::theme_engine::LayerBackground::Gradient { .. }
            | crate::theme_engine::LayerBackground::Image { .. } => Default::default(),
        };
        result.placement = surface.placement.clone();
        result.children = surface.children.clone();
    }
    result
}
