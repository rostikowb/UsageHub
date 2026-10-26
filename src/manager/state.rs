//! Serialized settings access for manager operations.
//!
//! Protocol: acquire the current-user named mutex, fresh-load settings,
//! mutate, atomic-save. Background UI operations run on a worker thread (the
//! UI stays responsive) and the UI reloads settings when the worker finishes.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

use crate::app_settings::{load_settings, save_settings_unlocked, SettingsFile};
use crate::manager::ManagerResult;
use crate::native_interop::wide_str;

use crate::providers::ProviderId;
/// Current-user settings-operation mutex. `Local\` is session-scoped, so one
/// user's concurrent processes serialize while other logon sessions stay
/// independent.
pub const SETTINGS_MUTEX_NAME: &str = "Local\\UsageHubSettingsGuard";
/// How long a manager operation waits for the settings mutex (5s).
pub const SETTINGS_MUTEX_TIMEOUT_MS: u32 = 5_000;

/// RAII guard holding the settings-operation mutex.
pub struct SettingsOperationGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

impl SettingsOperationGuard {
    pub fn acquire(timeout_ms: u32) -> ManagerResult<Self> {
        let name = wide_str(SETTINGS_MUTEX_NAME);
        let handle = unsafe { CreateMutexW(None, false, PCWSTR::from_raw(name.as_ptr())) }
            .map_err(|error| format!("Unable to create the settings guard: {error}"))?;
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            _ => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Err("Timed out waiting for the settings guard".into())
            }
        }
    }
}

impl Drop for SettingsOperationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Fresh-load settings (no cache). Call after any background operation
/// completes so the UI observes the latest state.
pub fn load_fresh() -> SettingsFile {
    load_settings()
}

/// Run `f` against freshly loaded settings under the operation mutex and
/// atomically save. Returns the closure's value on success.
pub fn mutate_settings<R>(
    f: impl FnOnce(&mut SettingsFile) -> ManagerResult<R>,
) -> ManagerResult<R> {
    mutate_settings_timeout(SETTINGS_MUTEX_TIMEOUT_MS, f)
}

/// [`mutate_settings`] with an explicit mutex wait budget.
pub fn mutate_settings_timeout<R>(
    timeout_ms: u32,
    f: impl FnOnce(&mut SettingsFile) -> ManagerResult<R>,
) -> ManagerResult<R> {
    let _guard = SettingsOperationGuard::acquire(timeout_ms)?;
    let mut settings = load_settings();
    let result = f(&mut settings)?;
    save_settings_unlocked(&settings)?;
    Ok(result)
}

/// Per-account operation serialization for manager mutations.
///
/// The settings mutex serializes registry commits only; this guard serializes
/// whole-account operations. Cleanup holds it across quiesce, snapshot,
/// verify, and delete; the poller probes it with a zero timeout around each
/// account HTTP job and skips busy accounts; lifecycle holds it around open,
/// focus, and rename, but never around stop because cleanup calls stop while
/// already owning this guard.
///
/// Names are deterministic per provider/stable account id (`used_ids` are
/// never reused, so a retired id's guard can never retarget a new account).
/// `Local\` keeps logon sessions independent, matching the settings guard.
pub struct AccountOperationGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

impl AccountOperationGuard {
    /// Acquire the guard for one account, waiting at most `timeout_ms`.
    /// A zero timeout only probes: it succeeds when the account is idle and
    /// fails immediately when another operation owns it.
    pub fn acquire(provider: ProviderId, id: &str, timeout_ms: u32) -> ManagerResult<Self> {
        let safe: String = id
            .bytes()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || byte == b'_' {
                    byte as char
                } else {
                    '_'
                }
            })
            .collect();
        let name = format!(
            "Local\\UsageHubAccount_{}_{}",
            provider.descriptor().key,
            safe
        );
        let wide = wide_str(&name);
        let handle = unsafe { CreateMutexW(None, false, PCWSTR::from_raw(wide.as_ptr())) }
            .map_err(|error| format!("Unable to create the account guard: {error}"))?;
        match unsafe { WaitForSingleObject(handle, timeout_ms) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self { handle }),
            _ => {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Err(format!(
                    "Account '{id}' is busy; another operation is in progress"
                ))
            }
        }
    }
}

impl Drop for AccountOperationGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_guard_try_acquire_probes_busyness() {
        let provider = crate::providers::ProviderId::Claude;
        let held = AccountOperationGuard::acquire(provider, "account_1", 5_000).unwrap();
        // Windows named mutexes are recursive for the owning thread by design
        // (nested backup/cleanup re-takes the same account guard), so
        // contention must be probed from a different thread: a zero-timeout
        // probe fails while another thread owns the account ...
        let busy = std::thread::spawn(move || {
            AccountOperationGuard::acquire(provider, "account_1", 0).is_err()
        })
        .join()
        .unwrap();
        assert!(busy);
        // ... but a different account is unaffected.
        let other = AccountOperationGuard::acquire(provider, "account_2", 0).unwrap();
        drop(held);
        drop(other);
        // Released: probing succeeds again.
        let _idle = AccountOperationGuard::acquire(provider, "account_1", 0).unwrap();
    }
}
