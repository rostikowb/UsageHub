//! UI hang watchdog, Explorer/taskbar watchdog, and tray reposition suppression.

use super::*;

/// How long the UI thread may stay unresponsive before the hang watchdog treats
/// the widget as wedged and replaces it with a fresh process. Long enough that a
/// slow repaint or a busy shell never costs a relaunch.
const UI_HANG_RECOVERY_SECS: u64 = 30;

/// Consecutive unanswered probes required on top of that delay. Elapsed time
/// alone would also count a system sleep that began while a probe was failing,
/// and relaunch on wake after a single missed probe on either side of it.
const UI_HANG_MIN_SILENT_PROBES: u32 = 10;

/// A long scheduling gap (for example, system sleep) breaks a run of probes.
const UI_HANG_MAX_PROBE_GAP_SECS: u64 = 10;

/// How long a single liveness probe waits for the UI thread to answer.
const UI_PROBE_TIMEOUT_MS: u32 = 1_000;

/// Environment counter of back-to-back hang relaunches. A replacement that
/// freezes in turn, behind an explorer.exe that stays hung for instance, is
/// given twice as long as the previous one before the next attempt, instead of
/// being replaced every half minute for as long as the cause lasts.
const ENV_HANG_RELAUNCHES: &str = "UH_HANG_RELAUNCHES";

/// A hang relaunch only extends the streak when the replacement freezes within
/// this long of being started; past it, the next hang starts over at 30 s.
const HANG_STREAK_WINDOW_SECS: u64 = 600;

/// Cap on the doubling: 30 s, then 1, 2, 4 and at most 8 minutes.
const HANG_BACKOFF_MAX_DOUBLINGS: u32 = 4;

/// Spacing below which two relaunches are treated as a storm (e.g. explorer.exe
/// crash-looping); when detected we back off instead of spawning in a tight loop.
const RELAUNCH_THROTTLE_SECS: u64 = 10;
const RELAUNCH_BACKOFF_SECS: u64 = 30;
/// Environment flag set on a relaunched child so it waits for the previous
/// instance's single-instance mutex instead of exiting immediately.
pub(super) const ENV_RELAUNCH: &str = "UH_RELAUNCH";
/// Unix timestamp (seconds) of the relaunch that spawned this process, passed to
/// the child so it can detect a relaunch storm.
const ENV_LAST_RELAUNCH_UNIX: &str = "UH_LAST_RELAUNCH_UNIX";

/// A process restart is reserved for an unresponsive UI thread. Shell-hosted
/// surface loss is recovered in place by the controller.
fn relaunch_after_ui_hang(streak: u32) {
    if RELAUNCH_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        return;
    }

    // Back off if we are relaunching very soon after the relaunch that spawned
    // us: that signals the shell is crash-looping, not a one-off restart.
    let now = now_unix_secs();
    let last = std::env::var(ENV_LAST_RELAUNCH_UNIX)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if last != 0 && now.saturating_sub(last) < RELAUNCH_THROTTLE_SECS {
        diagnose::log_recovery("relaunch storm detected; backing off before relaunching");
        std::thread::sleep(Duration::from_secs(RELAUNCH_BACKOFF_SECS));
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            diagnose::log_recovery(format!(
                "watchdog: unable to resolve current executable: {error}"
            ));
            RELAUNCH_IN_PROGRESS.store(false, Ordering::Release);
            return;
        }
    };

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Without this the replacement truncates the log on startup and takes the
    // reason for its own relaunch with it - the one entry worth reading after
    // the widget has recovered on its own. Diagnostics switched on from the
    // dashboard leave no flag on the command line, and --diagnose-append is
    // only read alongside --diagnose, so both are carried over.
    if diagnose::is_enabled() {
        for flag in ["--diagnose", "--diagnose-append"] {
            if !args.iter().any(|arg| arg == flag) {
                args.push(flag.to_string());
            }
        }
    }
    let mut command = std::process::Command::new(exe);
    command
        .args(&args)
        .env(ENV_RELAUNCH, "1")
        .env(ENV_LAST_RELAUNCH_UNIX, now.to_string())
        .env_remove(ENV_HANG_RELAUNCHES);
    command.env(ENV_HANG_RELAUNCHES, streak.saturating_add(1).to_string());
    match command.spawn() {
        Ok(_) => {
            diagnose::log_recovery("watchdog: relaunched fresh instance, exiting old one");
            // Skip DLL detach on a wedged process: another thread may hold a
            // lock needed by detach code. Recovery diagnostics are already flushed.
            unsafe {
                let _ = TerminateProcess(GetCurrentProcess(), 0);
            }
            std::process::exit(0);
        }
        Err(error) => {
            diagnose::log_recovery(format!(
                "watchdog: unable to spawn relaunched instance: {error}"
            ));
            RELAUNCH_IN_PROGRESS.store(false, Ordering::Release);
        }
    }
}

/// Ask the UI thread to acknowledge an empty message.
///
/// The hang watchdog runs on its own thread, so `SendMessageTimeoutW` here is
/// answered by the window's owning thread: it comes back at once while the
/// message loop is pumping, and times out once that loop is parked inside a
/// call that never returns.
fn ui_thread_responds(hwnd: HWND) -> bool {
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return true;
        }
        let mut acknowledged = 0usize;
        SendMessageTimeoutW(
            hwnd,
            WM_NULL,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            UI_PROBE_TIMEOUT_MS,
            Some(&mut acknowledged),
        )
        .0 != 0
    }
}

/// Relaunch the widget once its UI thread stops answering messages.
pub(super) fn spawn_ui_hang_watchdog() {
    std::thread::spawn(move || {
        let mut silence: Option<UiSilence> = None;
        loop {
            std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
            // Stepping through the UI thread in a debugger leaves it as silent
            // as a wedged one, and a relaunch would end the debugging session.
            if unsafe { IsDebuggerPresent() }.as_bool() {
                silence = None;
                continue;
            }
            let primary = SendHwnd(CONTROLLER_HWND.load(Ordering::Relaxed)).to_hwnd();
            if primary.is_invalid() || ui_thread_responds(primary) {
                silence = None;
                continue;
            }
            let now = Instant::now();
            let current = silence.get_or_insert_with(|| {
                UiSilence::new(
                    now,
                    hang_relaunch_streak(
                        std::env::var(ENV_HANG_RELAUNCHES).ok().as_deref(),
                        std::env::var(ENV_LAST_RELAUNCH_UNIX).ok().as_deref(),
                        now_unix_secs(),
                    ),
                )
            });
            current.record_probe(now);
            let delay = hang_recovery_delay(current.streak);
            if current.recovery_due(now) {
                let streak = current.streak;
                diagnose::log_recovery(format!(
                    "watchdog: UI thread stopped answering for {}s -> relaunching",
                    delay.as_secs()
                ));
                silence = None;
                relaunch_after_ui_hang(streak);
            }
        }
    });
}

/// An unbroken run of unanswered probes, with the streak it is measured against.
struct UiSilence {
    since: Instant,
    last_probe: Instant,
    silent_probes: u32,
    streak: u32,
}

impl UiSilence {
    fn new(now: Instant, streak: u32) -> Self {
        Self {
            since: now,
            last_probe: now,
            silent_probes: 0,
            streak,
        }
    }

    fn record_probe(&mut self, now: Instant) {
        if now.duration_since(self.last_probe) >= Duration::from_secs(UI_HANG_MAX_PROBE_GAP_SECS) {
            self.since = now;
            self.silent_probes = 0;
        }
        self.last_probe = now;
        self.silent_probes = self.silent_probes.saturating_add(1);
    }

    fn recovery_due(&self, now: Instant) -> bool {
        self.silent_probes >= UI_HANG_MIN_SILENT_PROBES
            && now.duration_since(self.since) >= hang_recovery_delay(self.streak)
    }
}

/// Back-to-back hang relaunches leading up to this process, read from the
/// variables its parent set when relaunching it. A process that has been
/// running past HANG_STREAK_WINDOW_SECS starts a fresh streak.
fn hang_relaunch_streak(
    hang_relaunches: Option<&str>,
    last_relaunch_unix: Option<&str>,
    now_unix: u64,
) -> u32 {
    let started = last_relaunch_unix
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if started == 0 || now_unix.saturating_sub(started) >= HANG_STREAK_WINDOW_SECS {
        return 0;
    }
    hang_relaunches
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// Silence tolerated before a relaunch, doubled for each back-to-back hang.
fn hang_recovery_delay(streak: u32) -> Duration {
    Duration::from_secs(UI_HANG_RECOVERY_SECS << streak.min(HANG_BACKOFF_MAX_DOUBLINGS))
}

/// Detect explorer.exe restarts and recover from them.
///
/// Shell recovery is posted to the controller, which recreates missing surfaces
/// on their owning UI thread without disturbing polling or dashboard state.
pub(super) fn spawn_taskbar_watchdog() {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(TASKBAR_WATCH_INTERVAL_SECS));
        let controller = controller_for(HWND::default());
        if controller.is_invalid() {
            return;
        }
        unsafe {
            let _ = PostMessageW(Some(controller), WM_APP_RECONCILE, WPARAM(0), LPARAM(0));
        }

        static LAST_WATCHDOG_TRAY_RECT: Mutex<Option<RECT>> = Mutex::new(None);
        // Snapshot AppState and release STATE before any Win32 or OCCUPANCY calls.
        let Some(state) = snapshot_state() else {
            continue;
        };
        let target_hwnd = controller_for(state.hwnd.to_hwnd());
        let current_tray_rect = state
            .tray_notify_hwnd
            .map(SendHwnd::to_hwnd)
            .and_then(native_interop::get_window_rect_safe);
        if !target_hwnd.is_invalid() && current_tray_rect.is_some() {
            let mut last_rect = LAST_WATCHDOG_TRAY_RECT
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if rect_changed(*last_rect, current_tray_rect) {
                *last_rect = current_tray_rect;
                unsafe {
                    let _ = PostMessageW(
                        Some(target_hwnd),
                        native_interop::WM_APP_TRAY_REPOSITION,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
        }

        if let Some(action) = taskbar_collision_action(&state) {
            unsafe {
                let _ = PostMessageW(
                    Some(target_hwnd),
                    native_interop::WM_APP_TASKBAR_COLLISION,
                    WPARAM(action),
                    LPARAM(0),
                );
            }
        }
    });
}

pub(super) fn taskbar_collision_action(state: &AppState) -> Option<usize> {
    if state.dragging || state.pending_drag || state.is_switching_window_style {
        return None;
    }
    let taskbar = state.taskbar_hwnd?.to_hwnd();
    let bounds = native_interop::get_taskbar_rect(taskbar)?;
    // Do not interpret an auto-hide slide or reveal strip as a capacity change.
    if !unsafe { IsWindowVisible(taskbar).as_bool() } || !taskbar_collision::taskbar_shown(bounds) {
        return None;
    }
    if state.auto_ejected {
        let target = restored_dock_rect(state, taskbar, bounds)?;
        if taskbar_lock_applies(state) {
            return (!positioning::taskbar_ejection_required(bounds, target, false, true))
                .then_some(0);
        }
        let occupancy = taskbar_collision::cached(taskbar, bounds)?;
        let margin = if state.auto_ejected_for_capacity {
            0
        } else {
            (20.0 * CURRENT_DPI.load(Ordering::Relaxed) as f64 / 96.0).round() as i32
        };
        occupancy.can_restore(target, margin).then_some(0)
    } else if state.embedded
        && state
            .placement_override
            .as_ref()
            .is_none_or(|p| p.nest != "floating")
    {
        let widget = native_interop::get_window_rect_safe(state.hwnd.to_hwnd())?;
        let locked = taskbar_lock_applies(state);
        // A layout pass during a slide can leave a stale child offset. Restore
        // the configured position before deciding the layout cannot fit.
        if positioning::taskbar_ejection_required(bounds, widget, false, locked)
            && restored_dock_rect(state, taskbar, bounds).is_some_and(|target| {
                positioning::taskbar_reposition_required(bounds, widget, target, locked)
            })
        {
            return Some(TASKBAR_COLLISION_REPOSITION);
        }
        // Visibility fallback needs only native geometry; an unavailable or
        // stale accessibility sample must not leave the widget clipped.
        let overlaps_apps = if positioning::taskbar_surface_is_clipped(bounds, widget) || locked {
            false
        } else {
            taskbar_collision::cached(taskbar, bounds)?.overlaps_app_controls(widget)
        };
        positioning::taskbar_ejection_required(bounds, widget, overlaps_apps, locked).then_some(1)
    } else {
        None
    }
}

pub(super) fn suppress_tray_reposition_for(duration: Duration) {
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *until = Some(Instant::now() + duration);
}

pub(super) fn tray_reposition_is_suppressed() -> bool {
    let now = Instant::now();
    let mut until = SUPPRESS_TRAY_REPOSITION_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match *until {
        Some(deadline) if now < deadline => true,
        Some(_) => {
            *until = None;
            false
        }
        None => false,
    }
}

#[cfg(test)]
mod ui_hang_watchdog_tests {
    use super::*;
    use std::sync::mpsc;
    use windows::Win32::System::Threading::GetCurrentThreadId;

    #[test]
    fn recovery_requires_both_the_delay_and_consecutive_probes() {
        let start = Instant::now();
        let mut silence = UiSilence::new(start, 0);
        for seconds in 0..10 {
            let now = start + Duration::from_secs(seconds * 2);
            silence.record_probe(now);
            assert!(!silence.recovery_due(now));
        }
        for seconds in 10..=15 {
            let now = start + Duration::from_secs(seconds * 2);
            silence.record_probe(now);
        }
        assert!(silence.recovery_due(start + Duration::from_secs(30)));

        let mut sparse = UiSilence::new(start, 0);
        for seconds in [0, 9, 18, 27, 36] {
            let now = start + Duration::from_secs(seconds);
            sparse.record_probe(now);
            assert!(!sparse.recovery_due(now));
        }
    }

    #[test]
    fn sleep_breaks_silence_even_after_many_failed_probes() {
        let start = Instant::now();
        let mut silence = UiSilence::new(start, 0);
        for probe in 0..UI_HANG_MIN_SILENT_PROBES {
            silence.record_probe(start + Duration::from_secs(u64::from(probe) * 2));
        }
        let wake = start + Duration::from_secs(600);
        silence.record_probe(wake);
        assert_eq!(silence.silent_probes, 1);
        assert!(!silence.recovery_due(wake));
        for probe in 1..=15 {
            silence.record_probe(wake + Duration::from_secs(probe * 2));
        }
        assert!(silence.recovery_due(wake + Duration::from_secs(30)));
    }

    struct WindowThread {
        hwnd: SendHwnd,
        thread_id: u32,
        release: mpsc::Sender<()>,
        handle: std::thread::JoinHandle<()>,
    }

    /// Own a message-only window on a thread of its own. A pumping thread runs
    /// a message loop until WM_QUIT; a wedged one blocks without ever pumping
    /// until released, which is how a stuck UI thread looks from outside.
    fn spawn_window_thread(pumping: bool) -> WindowThread {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || unsafe {
            let class = native_interop::wide_str("STATIC");
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR::from_raw(class.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
            .expect("create isolated message-only test window");
            let ready = (SendHwnd::from_hwnd(hwnd), GetCurrentThreadId());
            if pumping {
                PostThreadMessageW(ready.1, WM_APP, WPARAM(0), LPARAM(0))
                    .expect("queue the readiness marker");
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_APP {
                        ready_tx.send(ready).expect("hand the test window back");
                    }
                    DispatchMessageW(&msg);
                }
            } else {
                ready_tx.send(ready).expect("hand the test window back");
                let _ = release_rx.recv();
            }
            let _ = DestroyWindow(hwnd);
        });
        let (hwnd, thread_id) = ready_rx.recv().expect("test window created");
        WindowThread {
            hwnd,
            thread_id,
            release,
            handle,
        }
    }

    #[test]
    fn hang_probe_tells_a_pumping_ui_thread_from_a_wedged_one() {
        let pumping = spawn_window_thread(true);
        let wedged = spawn_window_thread(false);

        assert!(ui_thread_responds(pumping.hwnd.to_hwnd()));
        assert!(!ui_thread_responds(wedged.hwnd.to_hwnd()));

        wedged.release.send(()).expect("release the wedged thread");
        wedged.handle.join().expect("wedged thread exits");
        unsafe {
            PostThreadMessageW(pumping.thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                .expect("stop the pumping thread");
        }
        pumping.handle.join().expect("pumping thread exits");
    }

    #[test]
    fn hang_probe_leaves_a_destroyed_window_to_the_surface_watchdog() {
        let gone = spawn_window_thread(false);
        let hwnd = gone.hwnd;
        gone.release.send(()).expect("release the window thread");
        gone.handle.join().expect("window thread exits");

        assert!(ui_thread_responds(hwnd.to_hwnd()));
    }

    #[test]
    fn back_to_back_hang_relaunches_double_the_tolerated_silence() {
        assert_eq!(hang_recovery_delay(0), Duration::from_secs(30));
        assert_eq!(hang_recovery_delay(1), Duration::from_secs(60));
        assert_eq!(hang_recovery_delay(4), Duration::from_secs(480));
        assert_eq!(hang_recovery_delay(9), Duration::from_secs(480));
    }

    #[test]
    fn a_hang_streak_only_carries_over_while_the_replacement_is_fresh() {
        let now = 1_000_000;
        let fresh = (now - 60).to_string();
        let stale = (now - 600).to_string();

        assert_eq!(hang_relaunch_streak(Some("2"), Some(&fresh), now), 2);
        assert_eq!(hang_relaunch_streak(Some("2"), Some(&stale), now), 0);
        assert_eq!(hang_relaunch_streak(None, Some(&fresh), now), 0);
        assert_eq!(hang_relaunch_streak(Some("2"), None, now), 0);
        assert_eq!(hang_relaunch_streak(Some("garbage"), Some(&fresh), now), 0);
    }
}
