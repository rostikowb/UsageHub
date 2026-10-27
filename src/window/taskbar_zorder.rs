//! Keeps taskbar-embedded surfaces above Explorer's own taskbar children.
//!
//! Windows 11 raises the taskbar's `DesktopWindowContentBridge` child when the
//! taskbar is activated, e.g. after a click on the widget focuses another app.
//! The widget then stays hidden beneath the bridge until an unrelated layout
//! pass repositions it. An `EVENT_OBJECT_REORDER` hook scoped to Explorer
//! reports each such restack; covered surfaces get raised again.
//!
//! The callback never touches `STATE`: it can run while a nested message loop
//! on this thread holds the lock. It only walks sibling HWNDs and posts.
use super::*;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};

pub(super) const WM_APP_TASKBAR_ZORDER: u32 = WM_APP + 20;
const EVENT_OBJECT_REORDER: u32 = 0x8004;

/// (Explorer pid, hook). Only the UI thread installs or replaces it.
static REORDER_HOOK: Mutex<Option<(u32, SendWinEventHook)>> = Mutex::new(None);
// A reorder burst can arrive before the UI thread dispatches its first post.
// This lock is never held across Win32 calls and is independent of STATE.
static PENDING: Mutex<Vec<isize>> = Mutex::new(Vec::new());

pub(super) fn remove_reorder_hook() {
    let old = REORDER_HOOK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some((_, hook)) = old {
        unsafe {
            let _ = UnhookWinEvent(hook.to_hook());
        }
    }
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

pub(super) fn forget_surface(surface: HWND) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|item| *item != surface.0 as isize);
}

/// Install the hook for the Explorer process owning `taskbar`, replacing a
/// hook left from a previous Explorer instance.
pub(super) fn ensure_reorder_hook(taskbar: HWND) {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(taskbar, Some(&mut pid));
    }
    if pid == 0 {
        return;
    }
    let mut hook = REORDER_HOOK.lock().unwrap_or_else(|e| e.into_inner());
    if hook.as_ref().is_some_and(|(owner, _)| *owner == pid) {
        return;
    }
    if let Some((_, old)) = hook.take() {
        unsafe {
            let _ = UnhookWinEvent(old.to_hook());
        }
    }
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let installed = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_REORDER,
            EVENT_OBJECT_REORDER,
            None,
            Some(on_taskbar_reorder),
            pid,
            0,
            native_interop::WINEVENT_OUTOFCONTEXT,
        )
    };
    if !installed.is_invalid() {
        *hook = Some((pid, SendWinEventHook::from_hook(installed)));
    }
}

fn is_taskbar_host(hwnd: HWND) -> bool {
    matches!(
        native_interop::window_class_name(hwnd).as_deref(),
        Some("Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
    )
}

fn is_our_surface(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid == unsafe { GetCurrentProcessId() }
        && matches!(
            native_interop::window_class_name(hwnd).as_deref(),
            Some("UsageHubSurface" | "UHThemeMirror" | "UHDesktopSurface")
        )
}

fn overlaps(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// Only a visible foreign sibling intersecting a surface can cover it.
/// Other UsageHub surfaces are ignored to avoid mutual restacking.
pub(super) fn covered_surfaces(taskbar: HWND) -> Vec<HWND> {
    let mut covered = Vec::new();
    let mut foreign_rects = Vec::new();
    unsafe {
        let mut child = GetWindow(taskbar, GW_CHILD).unwrap_or_default();
        while !child.is_invalid() {
            if IsWindowVisible(child).as_bool() {
                if let Some(rect) = native_interop::get_window_rect_safe(child) {
                    if is_our_surface(child) {
                        if foreign_rects.iter().any(|foreign| overlaps(&rect, foreign)) {
                            covered.push(child);
                        }
                    } else {
                        let mut pid = 0;
                        GetWindowThreadProcessId(child, Some(&mut pid));
                        if pid != GetCurrentProcessId() {
                            foreign_rects.push(rect);
                        }
                    }
                }
            }
            child = GetWindow(child, GW_HWNDNEXT).unwrap_or_default();
        }
    }
    covered
}

unsafe extern "system" fn on_taskbar_reorder(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // REORDER reports the parent whose children were restacked.
    if hwnd.is_invalid() || !matches!(id_object, 0 | -4) || id_child != 0 || !is_taskbar_host(hwnd)
    {
        return;
    }
    for surface in covered_surfaces(hwnd) {
        let key = surface.0 as isize;
        let should_post = {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            if pending.contains(&key) {
                false
            } else {
                pending.push(key);
                true
            }
        };
        if should_post
            && PostMessageW(Some(surface), WM_APP_TASKBAR_ZORDER, WPARAM(0), LPARAM(0)).is_err()
        {
            PENDING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|item| *item != key);
        }
    }
}

/// Handler for [`WM_APP_TASKBAR_ZORDER`], run from normal dispatch.
pub(super) fn raise_if_covered(surface: HWND) {
    PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|item| *item != surface.0 as isize);
    if !is_our_surface(surface) {
        return;
    }
    unsafe {
        let Ok(parent) = GetParent(surface) else {
            return;
        };
        if !is_taskbar_host(parent) || !covered_surfaces(parent).contains(&surface) {
            return;
        }
        let _ = SetWindowPos(
            surface,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
    diagnose::log("taskbar surface raised above a restacked taskbar child");
}
