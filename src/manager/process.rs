//! Bounded console execution with owned process-tree cleanup.
//!
//! [`run_bounded`] creates and arms an anonymous kill-on-close Job Object
//! *before* spawn, then spawns `command` suspended, assigns the child to the
//! job, and resumes only the child's own primary thread, so descendants
//! cannot escape assignment. Stdout/stderr are drained concurrently while the
//! child runs; both process exit and pipe EOF are bounded by the same
//! monotonic deadline, which starts before any setup/spawn work. On timeout
//! (or any failure after spawn) the owned tree is killed via the job, with a
//! single shared bounded cleanup window. On success the kill-on-close limit
//! is disarmed after child exit plus EOF on both streams so intentional
//! long-lived GUI descendants survive; the disarm result is checked, never
//! assumed.
//!
//! Capture keeps at most `cap + 1` sentinel bytes per stream and returns them
//! *untruncated* so callers detect overflow via `len() > cap`.
//!
//! Never logs raw arguments (prompts/auth material).

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[cfg(not(windows))]
compile_error!(
    "usagehub manager::process runner is Windows-only (requires Job Objects + ToolHelp)"
);

/// Error for [`run_bounded`]. Carries only the caller label plus an OS-level
/// detail string; never raw command arguments.
#[derive(Debug)]
pub enum ConsoleError {
    /// The deadline elapsed before child exit plus EOF on both streams.
    /// The owned process tree was killed (bounded reap).
    TimedOut {
        /// Caller-supplied operation label.
        label: String,
        /// Deadline that was exceeded.
        timeout: Duration,
        /// Safe stage diagnostic: which wait stage blocked, whether the child
        /// exited, and which pipe streams had already reached EOF
        /// (`stage=...; processExited=...; pipeEOF=...`).
        /// Never contains raw argv or captured output.
        detail: String,
    },
    /// Spawn, job assignment, thread resume, drain, or wait failure.
    /// A suspended child that failed setup was terminated and reaped.
    Failed {
        /// Caller-supplied operation label.
        label: String,
        /// OS-level detail (no raw arguments).
        detail: String,
    },
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsoleError::TimedOut {
                label,
                timeout,
                detail,
            } => {
                write!(f, "{label} timed out after {timeout:?} ({detail})")
            }
            ConsoleError::Failed { label, detail } => {
                write!(f, "{label} failed: {detail}")
            }
        }
    }
}

impl std::error::Error for ConsoleError {}

/// Poll interval while waiting for child exit.
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Single shared bound for post-deadline / post-error kill + reap +
/// drain-detach cleanup. Callers share one window per failure; waits are
/// never stacked (no reap-grace plus drain-grace sequences).
const REAP_GRACE: Duration = Duration::from_secs(2);
/// Reader chunk size for concurrent pipe drains (stack buffer, not heap).
const DRAIN_CHUNK: usize = 32 * 1024;

/// Detach this process's inherited standard handles from handle inheritance.
///
/// `std::process::Command` spawns children with `bInheritHandles = TRUE` and
/// no `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (see std's Windows `process.rs`),
/// so every handle marked inheritable in this process is copied into every
/// child, whether or not that child assigned it to stdio. A `--manage`
/// backend started by [`run_bounded`] receives the caller's response pipes as
/// its STD_OUTPUT/STD_ERROR handles (STD_INPUT is NUL), and inherited
/// handles keep their inheritable flag. Any later child (provider CLI,
/// detached GUI descendant) would then hold a writer to those response pipes
/// after this process exits and stall the caller's EOF wait until the deadline.
///
/// Clearing `HANDLE_FLAG_INHERIT` on our own STD handles, before any spawn,
/// stops that propagation without changing the handles, the assigned stdio of
/// future children, or the child-process API. Null/invalid STD handles (normal
/// for a GUI-subsystem process) are skipped. Best effort: a handle that
/// cannot be retagged is not a live handle-table entry we could leak.
///
/// `std::io::{stdin,stdout,stderr}` expose exactly the OS STD handles, which
/// is equivalent to `GetStdHandle(STD_INPUT/OUTPUT/ERROR_HANDLE)` without
/// adding the `Win32_System_Console` feature to the `windows` crate.
pub(crate) fn clear_inherited_standard_handles() {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::{
        SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT,
    };

    for raw in [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ] {
        if raw.is_null() || raw as isize == -1 {
            continue;
        }
        unsafe {
            let _ = SetHandleInformation(HANDLE(raw), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0));
        }
    }
}

/// Safe timeout diagnostic: names the wait stage, whether the child itself
/// exited, and which pipe streams had already reached EOF. Never includes
/// raw arguments or captured bytes.
#[cfg(windows)]
fn timeout_detail(stage: &str, process_exited: bool, stdout_eof: bool, stderr_eof: bool) -> String {
    let pipe_eof = match (stdout_eof, stderr_eof) {
        (true, true) => "stdout+stderr",
        (true, false) => "stdout",
        (false, true) => "stderr",
        (false, false) => "none",
    };
    format!("stage={stage}; processExited={process_exited}; pipeEOF={pipe_eof}")
}

/// Run `command` with `timeout` covering child exit AND pipe EOF.
///
/// Forces stdin to null, stdout/stderr to pipes, and a hidden console.
/// Returns up to `max_capture_bytes + 1` sentinel bytes per stream in
/// [`Output`]; `stdout.len() > max_capture_bytes` (same for stderr) signals
/// that the stream was truncated at the sentinel. `label` identifies the
/// operation in errors; raw arguments are never included in errors or logs.
pub fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    max_capture_bytes: usize,
    label: &str,
) -> Result<Output, ConsoleError> {
    if timeout.is_zero() {
        return Err(ConsoleError::TimedOut {
            label: label.to_string(),
            timeout,
            detail: timeout_detail("pre-dispatch", false, false, false),
        });
    }
    run_bounded_windows(command, timeout, max_capture_bytes, label)
}

#[cfg(windows)]
fn run_bounded_windows(
    command: &mut Command,
    timeout: Duration,
    max_capture_bytes: usize,
    label: &str,
) -> Result<Output, ConsoleError> {
    use std::os::windows::io::AsRawHandle as _;
    use std::os::windows::process::CommandExt as _;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_NO_WINDOW, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    let failed = |detail: String| ConsoleError::Failed {
        label: label.to_string(),
        detail,
    };

    // The monotonic deadline starts before any setup/spawn work. Checked
    // arithmetic: an absurd timeout saturates to a far-future instant instead
    // of panicking.
    let onset = Instant::now();
    let deadline = onset
        .checked_add(timeout)
        .unwrap_or(onset + Duration::from_secs(86400 * 365 * 100));

    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // Suspended spawn: assign to the job before any child code runs, then
    // resume the primary thread. Hidden console: no window flash.
    command.creation_flags(CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0);

    struct JobGuard {
        handle: Option<HANDLE>,
    }
    impl Drop for JobGuard {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                // Armed close kills the owned tree; disarmed close frees it.
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    // CREATE + ARM the anonymous kill-on-close job BEFORE spawn, so a
    // suspended child can never leak through a later setup failure: every
    // failure below this point owns a live child and terminates/reaps it.
    let mut job = {
        let handle = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) };
        let handle = match handle {
            Ok(h) => h,
            Err(e) => return Err(failed(format!("create job: {e}"))),
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let armed = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &raw const limits as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if let Err(e) = armed {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(failed(format!("arm job kill-on-close: {e}")));
        }
        JobGuard {
            handle: Some(handle),
        }
    };

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return Err(failed(format!("spawn: {e}"))),
    };
    let pid = child.id();
    // Borrowed process handle owned by `Child`; never closed here.
    let process_handle = HANDLE(child.as_raw_handle());

    let assigned = unsafe {
        AssignProcessToJobObject(job.handle.expect("job armed before spawn"), process_handle)
    };
    if let Err(e) = assigned {
        // Child is still suspended: terminate it before reporting.
        let _ = child.kill();
        reap_until(&mut child, Instant::now() + REAP_GRACE);
        return Err(failed(format!("assign job: {e}")));
    }

    // Resume the child's own primary thread(s) via ToolHelp + ResumeThread.
    // The job assignment above happened while the process was suspended, so
    // no descendant could escape in between.
    let resumed = unsafe {
        let snapshot = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            Ok(h) => h,
            Err(e) => {
                let _ = child.kill();
                reap_until(&mut child, Instant::now() + REAP_GRACE);
                return Err(failed(format!("thread snapshot: {e}")));
            }
        };
        let mut resumed = 0u32;
        let mut entry = THREADENTRY32::default();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        if Thread32First(snapshot, &raw mut entry).is_ok() {
            loop {
                if entry.th32OwnerProcessID == pid {
                    if let Ok(thread) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                    {
                        if ResumeThread(thread) != u32::MAX {
                            resumed += 1;
                        }
                        let _ = CloseHandle(thread);
                    }
                }
                if Thread32Next(snapshot, &raw mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        resumed
    };
    if resumed == 0 {
        let _ = child.kill();
        reap_until(&mut child, Instant::now() + REAP_GRACE);
        // JobGuard drop closes the armed job, killing the (suspended) tree.
        return Err(failed(
            "resume primary thread: no thread resumed".to_string(),
        ));
    }

    // Concurrent capped drains while the child runs. Each drain keeps at most
    // cap+1 sentinel bytes and keeps reading to EOF so the child never blocks
    // on a full pipe; the deadline below bounds descendants that hold pipes.
    // Thread creation is fallible: on failure the tree is killed, never left
    // running behind a Fake success.
    let (drain_tx, drain_rx) =
        std::sync::mpsc::channel::<(&'static str, Result<Vec<u8>, String>)>();
    if let Err(e) = spawn_drain(
        child.stdout.take(),
        "stdout",
        max_capture_bytes,
        drain_tx.clone(),
    ) {
        let _ = child.kill();
        reap_until(&mut child, Instant::now() + REAP_GRACE);
        return Err(failed(format!("spawn stdout drain: {e}")));
    }
    if let Err(e) = spawn_drain(
        child.stderr.take(),
        "stderr",
        max_capture_bytes,
        drain_tx.clone(),
    ) {
        let _ = child.kill();
        reap_until(&mut child, Instant::now() + REAP_GRACE);
        return Err(failed(format!("spawn stderr drain: {e}")));
    }
    drop(drain_tx);

    // Deadline covers exit AND EOF.
    loop {
        match child.try_wait() {
            Err(e) => {
                kill_tree(job.handle, &mut child);
                reap_until(&mut child, Instant::now() + REAP_GRACE);
                return Err(failed(format!("wait: {e}")));
            }
            Ok(Some(status)) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let mut out: Option<Vec<u8>> = None;
                let mut err: Option<Vec<u8>> = None;
                let wait_start = Instant::now();
                loop {
                    if out.is_some() && err.is_some() {
                        break;
                    }
                    let elapsed = wait_start.elapsed();
                    if elapsed >= remaining {
                        // EOF not reached in time (a descendant holds the
                        // pipes open): the armed job owns the tree.
                        let detail = timeout_detail("pipe-eof", true, out.is_some(), err.is_some());
                        kill_tree(job.handle, &mut child);
                        reap_until(&mut child, Instant::now() + REAP_GRACE);
                        return Err(ConsoleError::TimedOut {
                            label: label.to_string(),
                            timeout,
                            detail,
                        });
                    }
                    // `elapsed < remaining`, so this subtraction cannot
                    // underflow; the recv itself cannot outlive the deadline.
                    match drain_rx.recv_timeout(remaining - elapsed) {
                        Ok(("stdout", Ok(bytes))) => out = Some(bytes),
                        Ok(("stderr", Ok(bytes))) => err = Some(bytes),
                        Ok((_, Err(detail))) => {
                            // A real pipe read failure is a failure, never a
                            // partial-output success.
                            kill_tree(job.handle, &mut child);
                            reap_until(&mut child, Instant::now() + REAP_GRACE);
                            return Err(failed(detail));
                        }
                        Ok(_) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let detail =
                                timeout_detail("pipe-eof", true, out.is_some(), err.is_some());
                            kill_tree(job.handle, &mut child);
                            reap_until(&mut child, Instant::now() + REAP_GRACE);
                            return Err(ConsoleError::TimedOut {
                                label: label.to_string(),
                                timeout,
                                detail,
                            });
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            let detail =
                                timeout_detail("pipe-eof", true, out.is_some(), err.is_some());
                            kill_tree(job.handle, &mut child);
                            reap_until(&mut child, Instant::now() + REAP_GRACE);
                            return Err(failed(format!(
                                "drain channel closed before EOF ({detail})"
                            )));
                        }
                    }
                }
                let out = out.unwrap_or_default();
                let err = err.unwrap_or_default();
                // cap+1 sentinel bytes are returned UNTRUNCATED so callers
                // detect overflow via `len() > max_capture_bytes`.
                let disarmed = unsafe {
                    let quiesced = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                    SetInformationJobObject(
                        job.handle.expect("job alive on success path"),
                        JobObjectExtendedLimitInformation,
                        &raw const quiesced as *const core::ffi::c_void,
                        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    )
                    .is_ok()
                };
                if !disarmed {
                    // The job is still armed: closing it would kill the
                    // innocent GUI descendants this success path exists for.
                    // Never report Ok unless the disarm landed.
                    kill_tree(job.handle, &mut child);
                    reap_until(&mut child, Instant::now() + REAP_GRACE);
                    return Err(failed("disarm job kill-on-close".to_string()));
                }
                // Prevent Drop from touching survivors: close the disarmed job.
                if let Some(h) = job.handle.take() {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                }
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Non-blocking peek at completed drains so the timeout
                    // reports which pipes had already reached EOF while the
                    // child itself was still running.
                    let mut stdout_eof = false;
                    let mut stderr_eof = false;
                    loop {
                        match drain_rx.try_recv() {
                            Ok(("stdout", _)) => stdout_eof = true,
                            Ok(("stderr", _)) => stderr_eof = true,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let detail = timeout_detail("child-exit", false, stdout_eof, stderr_eof);
                    kill_tree(job.handle, &mut child);
                    // One shared bounded cleanup window: the reap and the
                    // drain-detach below share REAP_GRACE instead of stacking
                    // separate graces.
                    let cleanup_end = Instant::now() + REAP_GRACE;
                    reap_until(&mut child, cleanup_end);
                    while Instant::now() < cleanup_end {
                        match drain_rx
                            .recv_timeout(cleanup_end.saturating_duration_since(Instant::now()))
                        {
                            Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                    return Err(ConsoleError::TimedOut {
                        label: label.to_string(),
                        timeout,
                        detail,
                    });
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

/// Kill the owned tree via the job (plus a direct kill fallback). Reaping and
/// drain-detach stay with the caller inside one shared [`REAP_GRACE`] window.
/// Never blocks.
#[cfg(windows)]
fn kill_tree(job: Option<windows::Win32::Foundation::HANDLE>, child: &mut std::process::Child) {
    if let Some(h) = job {
        unsafe {
            let _ = windows::Win32::System::JobObjects::TerminateJobObject(h, 1);
        }
    }
    let _ = child.kill();
}

/// Reap an exited child without blocking past `end`. Never sleeps past the
/// shared cleanup window.
#[cfg(windows)]
fn reap_until(child: &mut std::process::Child, end: Instant) {
    while Instant::now() < end {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Shared capped pipe drain: keeps at most `cap + 1` sentinel bytes while
/// draining to EOF so the child never blocks on a full pipe. The sentinel is
/// returned as-is; callers detect overflow via `len() > cap`.
///
/// A real I/O failure is reported as `Err(detail)` (`Interrupted` is retried,
/// EOF-after-error is never reported as success). Thread creation is fallible
/// so the caller can kill its tree on failure. The scratch buffer lives on
/// the reader stack, not the heap.
fn spawn_drain<R: Read + Send + 'static>(
    pipe: Option<R>,
    name: &'static str,
    cap: usize,
    tx: std::sync::mpsc::Sender<(&'static str, Result<Vec<u8>, String>)>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("usagehub-drain-{name}"))
        .spawn(move || {
            let mut kept = Vec::new();
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; DRAIN_CHUNK];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break, // EOF
                        Ok(n) => {
                            if kept.len() <= cap {
                                let room = cap + 1 - kept.len();
                                kept.extend_from_slice(&chunk[..n.min(room)]);
                            }
                            // Beyond cap+1: discard, keep draining to EOF.
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            let _ = tx.send((name, Err(format!("{name} pipe read: {e}"))));
                            return;
                        }
                    }
                }
            }
            let _ = tx.send((name, Ok(kept)));
        })
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn pid_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) => h,
                Err(_) => return false,
            };
            let mut code = 0u32;
            let alive = match GetExitCodeProcess(handle, &mut code) {
                Ok(()) => code == STILL_ACTIVE.0 as u32,
                Err(_) => false,
            };
            let _ = CloseHandle(handle);
            alive
        }
    }

    fn kill_pid(pid: u32) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }

    fn wait_pid_gone(pid: u32, what: &str) {
        let start = Instant::now();
        loop {
            if !pid_alive(pid) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "{what} pid {pid} is still alive after the kill"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn ping_hung(seconds: u32) -> Command {
        // `ping -n N` waits ~N-1 seconds; deterministic inbox fixture, no network.
        let mut c = Command::new("cmd");
        c.arg("/C");
        c.arg(format!("ping -n {seconds} 127.0.0.1 >NUL"));
        c
    }

    #[test]
    fn hung_process_times_out_and_reaps() {
        let mut cmd = ping_hung(30);
        let start = Instant::now();
        let err = run_bounded(&mut cmd, Duration::from_secs(2), 64 * 1024, "test-hung")
            .expect_err("hung child must time out");
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "reap must stay bounded"
        );
        match err {
            ConsoleError::TimedOut { label, timeout, .. } => {
                assert_eq!(label, "test-hung");
                assert_eq!(timeout, Duration::from_secs(2));
            }
            ConsoleError::Failed { detail, .. } => panic!("expected timeout, got failed: {detail}"),
        }
    }

    #[test]
    fn hung_tree_kills_descendant_by_pid() {
        // The direct child (powershell) spawns a ping grandchild, records the
        // grandchild PID to a file, then hangs. The timeout must kill the
        // whole tree: proven by the recorded PID actually dying, not by
        // elapsed time alone.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!(
            "$p = Start-Process -FilePath ping -ArgumentList '-n','30','127.0.0.1' \
             -WindowStyle Hidden -PassThru; \
             [System.IO.File]::WriteAllText('{}', $p.Id.ToString()); \
             ping -n 30 127.0.0.1 > $null",
            pid_file.display(),
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
        let err = run_bounded(&mut cmd, Duration::from_secs(5), 64 * 1024, "test-tree")
            .expect_err("hung tree must time out");
        assert!(
            matches!(err, ConsoleError::TimedOut { .. }),
            "expected timeout, got {err}"
        );
        let pid_text =
            std::fs::read_to_string(&pid_file).expect("fixture must record the grandchild PID");
        let grandchild: u32 = pid_text
            .trim()
            .parse()
            .expect("PID file must hold a process id");
        assert_ne!(grandchild, 0, "fixture must record a real PID");
        wait_pid_gone(grandchild, "grandchild");
    }

    #[test]
    #[ignore = "detached-spawn helper: launched re-entrantly by successful_root_... via current_exe --exact --ignored"]
    fn detached_stdio_null_spawn_helper() {
        // Re-entrant root child: spawns a truly detached ping with all three
        // stdio handles explicitly NUL so the survivor never inherits the
        // runner pipes, records its PID, and exits at once. Native spawn only:
        // no PowerShell/Start-Process semantics (no console-handle inheritance,
        // no non-interactive wait on the child).
        //
        // Mirror the `--manage` entry policy before any spawn: this root was
        // itself started with the runner's response pipes as its STD handles,
        // and inherited handles stay inheritable. Without clearing
        // HANDLE_FLAG_INHERIT, ping would inherit those writers even with
        // Stdio::null and hold the runner's stdout/stderr open after this
        // process exits, stalling EOF until the deadline.
        clear_inherited_standard_handles();
        let pid_file = std::env::var_os("USAGEHUB_TEST_DETACHED_PID_FILE")
            .expect("helper needs USAGEHUB_TEST_DETACHED_PID_FILE");
        let mut spawn = Command::new("ping");
        spawn.args(["-n", "60", "127.0.0.1"]);
        spawn.stdin(Stdio::null());
        spawn.stdout(Stdio::null());
        spawn.stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            spawn.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no window flash.
        }
        let child = spawn.spawn().expect("helper must spawn ping");
        let pid = child.id();
        std::fs::write(&pid_file, pid.to_string()).expect("helper must record detached PID");
        // Intentionally detach: the survivor must outlive this root so the
        // success-path disarm is observable. Forgetting skips any drop-time
        // semantics; process exit closes our handle while ping keeps running.
        std::mem::forget(child);
    }

    #[test]
    fn successful_root_leaves_detached_stdio_null_child_alive() {
        // The root is this test binary re-entered on the ignored helper above:
        // it spawns ping with stdin/out/err explicitly NUL, records the PID,
        // and exits at once. The helper also clears inherited STD handle
        // flags, so the survivor cannot hold the runner's response pipes open.
        // No PowerShell/Start-Process is involved, so no shell waits on it.
        // Success must disarm kill-on-close so the stdio-null survivor stays
        // alive. The test kills exactly the recorded PID afterwards.
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("detached.pid");
        let exe = std::env::current_exe().expect("current test executable");
        let mut cmd = Command::new(exe);
        cmd.args([
            "--exact",
            "manager::process::tests::detached_stdio_null_spawn_helper",
            "--ignored",
            "--nocapture",
        ]);
        cmd.env("USAGEHUB_TEST_DETACHED_PID_FILE", &pid_file);
        // 10s is ample: the root exits in well under a second and EOF must
        // follow immediately once the survivor no longer inherits the pipes.
        // A timeout here is a real inheritance leak (Debug shows the stage
        // diagnostic: pipe-eof + which stream still lacked EOF).
        let out = match run_bounded(&mut cmd, Duration::from_secs(10), 64 * 1024, "test-detach") {
            Ok(out) => out,
            Err(error) => panic!("detached-spawn root must succeed: {error:?}"),
        };
        assert!(
            out.status.success(),
            "spawn root must exit 0: status={} stdout_len={} stderr={:?}",
            out.status,
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr),
        );
        let pid_text =
            std::fs::read_to_string(&pid_file).expect("fixture must record the detached PID");
        let detached: u32 = pid_text
            .trim()
            .parse()
            .expect("PID file must hold a process id");
        assert_ne!(detached, 0, "fixture must record a real PID");
        assert!(
            pid_alive(detached),
            "disarmed job must leave detached pid {detached} alive"
        );
        kill_pid(detached);
        wait_pid_gone(detached, "detached");
    }

    #[test]
    fn capture_returns_prefix_plus_overflow_sentinel() {
        // Exact byte fixture: N `A`s with no newline, written by the native
        // powershell host itself (`[Console]::Out.Write`), not via a cmd
        // `<nul set /p` loop: `set /p` on EOF stdin sets errorlevel 1, so the
        // cmd loop always exited nonzero. Overflow must surface as exactly
        // cap+1 sentinel bytes holding the stream prefix, so callers detect
        // it via `len() > cap`.
        let total = 4 * 1024 + 7;
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[Console]::Out.Write('A' * {total})"),
        ]);
        let cap = 1024;
        let out = run_bounded(&mut cmd, Duration::from_secs(30), cap, "test-cap")
            .expect("exact-bytes fixture must succeed");
        assert!(
            out.status.success(),
            "fixture must exit 0: status={} stdout_len={} stderr={:?}",
            out.status,
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(
            out.stdout.len(),
            cap + 1,
            "overflow must return the cap+1 sentinel untruncated"
        );
        assert!(
            out.stdout.iter().all(|&b| b == b'A'),
            "sentinel must be the exact stream prefix"
        );
        assert!(out.stderr.is_empty(), "nothing was written to stderr");
    }

    #[test]
    fn capture_short_output_is_byte_exact() {
        // Under-cap output must round-trip byte-exact (the prefix property
        // the sentinel relies on). Same native-host fixture as above: no cmd
        // `set /p` loop, which always exited nonzero on EOF stdin.
        let total = 100;
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("[Console]::Out.Write('A' * {total})"),
        ]);
        let out = run_bounded(&mut cmd, Duration::from_secs(30), 1024, "test-cap-exact")
            .expect("exact-bytes fixture must succeed");
        assert!(
            out.status.success(),
            "fixture must exit 0: status={} stdout_len={} stderr={:?}",
            out.status,
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr),
        );
        assert_eq!(out.stdout.len(), total, "short output must be exact");
        assert!(out.stdout.iter().all(|&b| b == b'A'));
    }

    #[test]
    fn spawn_failure_reports_label_without_raw_args() {
        // A real spawn failure: the secret-looking argument must not surface
        // in the error, only the caller label plus the OS detail.
        let mut cmd = Command::new("usagehub-definitely-missing-binary-xyz");
        cmd.arg("--password=s3cr3t-fixture-value");
        let err = run_bounded(&mut cmd, Duration::from_secs(10), 1024, "test-spawn")
            .expect_err("missing binary must fail");
        let text = format!("{err}");
        assert!(
            !text.contains("s3cr3t-fixture-value"),
            "raw arguments must never surface in errors: {text}"
        );
        assert!(text.contains("test-spawn"), "label must surface: {text}");
        assert!(
            matches!(err, ConsoleError::Failed { .. }),
            "spawn failure is Failed, got {err}"
        );
    }
}
