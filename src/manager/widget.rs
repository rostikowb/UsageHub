//! Dynamic widget account coordination.
//!
//! The bar shows one slot per enabled + pinned [`AccountProfile`]
//! (see `theme_engine::account_layout` for geometry/expansion). This module is
//! the controller-side glue:
//! - [`visible_cards`]: settings-driven slot list with live busy badges,
//! - [`schedule_taskbar_identity_refresh`]: real taskbar identity
//!   maintenance (shortcut/COM stamping via `manager::native`, off the UI
//!   thread, coalesced with a single in-flight flag),
//! - [`schedule_backup_purge`]: 7-day deletion-undo expiry on a worker thread
//!   (startup, resume, and hourly; never blocks the UI),
//! - [`open_account_card`]/[`focus_account_card`]: per-slot open/focus through
//!   `manager::lifecycle` (the lifecycle slice owns process launching).
//!
//! Busyness uses the shared per-account operation guard
//! (`manager::state::AccountOperationGuard::acquire(provider, id, 0)` with
//! immediate drop as the try-probe; `Ok` = idle, `Err` = busy). No separate
//! registry or helper exists. A busy slot keeps a generic
//! «оновлення/операція» badge — the slot is never removed, and a transient
//! poll job is never presented as a deletion.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::accounts::AccountSettings;
use crate::manager::ManagerResult;
use crate::providers::ProviderId;
use crate::theme_engine::account_layout::{self, AccountCard};

/// Enabled + pinned slots in display order, with busy badges overlaid and
/// error/selection state from the latest poller snapshot (`None` keeps stable
/// slots with empty readings).
pub fn visible_cards(
    settings: &AccountSettings,
    data: Option<&crate::models::AppUsageData>,
) -> Vec<AccountCard> {
    let mut cards = account_layout::visible_account_cards(settings, data);
    account_layout::mark_busy_cards(&mut cards, |provider, id| account_busy(provider, id));
    cards
}

/// Try-probe for an in-flight per-account operation (poll job, launch, or
/// deletion). `Ok(guard)` is dropped immediately: idle. `Err` (timeout/busy):
/// busy. Never blocks.
pub fn account_busy(provider: ProviderId, id: &str) -> bool {
    match crate::manager::state::AccountOperationGuard::acquire(provider, id, 0) {
        Ok(_guard) => false,
        Err(_) => true,
    }
}

/// Real taskbar identity maintenance: ensure per-account shortcuts and COM
/// property stamping through `manager::native`, off the UI thread. Calls
/// coalesce behind a single in-flight flag so startup, settings updates,
/// resume, and the periodic timer never pile up workers and never run
/// per-account shells on a tick. The native helper owns snapshot caching and
/// launch-wait batching; this only schedules it.
pub fn schedule_taskbar_identity_refresh(reason: &'static str) {
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    std::thread::Builder::new()
        .name("usagehub-taskbar-identities".into())
        .spawn(move || {
            let outcome =
                std::panic::catch_unwind(|| crate::manager::native::refresh_taskbar_identities());
            IN_FLIGHT.store(false, Ordering::Release);
            match outcome {
                Ok(Ok((ensured, pruned))) => {
                    crate::diagnose::log(format!(
                        "taskbar identities refreshed ({reason}): ensured={ensured} pruned={pruned}"
                    ));
                }
                Ok(Err(error)) => {
                    crate::diagnose::log(format!(
                        "taskbar identity refresh ({reason}) failed: {error}; next trigger retries"
                    ));
                }
                Err(_) => {
                    crate::diagnose::log(format!(
                        "taskbar identity refresh ({reason}) panicked; next trigger retries"
                    ));
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| {
            IN_FLIGHT.store(false, Ordering::Release);
            crate::diagnose::log(format!(
                "taskbar identity refresh ({reason}) thread could not start: {error}"
            ));
        });
}

/// Fire-and-forget 7-day deletion-undo expiry on a worker thread. Purge runs
/// at startup, on resume, and on the hourly controller timer; the UI thread
/// never waits for it.
pub fn schedule_backup_purge() {
    std::thread::Builder::new()
        .name("usagehub-backup-purge".into())
        .spawn(|| {
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_secs())
                .unwrap_or(0);
            match crate::manager::backup::purge_expired(now_unix) {
                Ok(purged) => {
                    if !purged.is_empty() {
                        crate::diagnose::log(format!(
                            "backup purge removed {} expired snapshot(s)",
                            purged.len()
                        ));
                    }
                }
                Err(error) => {
                    crate::diagnose::log(format!("backup purge failed: {error}"));
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| {
            crate::diagnose::log(format!("backup purge thread could not start: {error}"));
        });
}

/// Open an account window for a widget slot: focus the running window when one
/// exists, otherwise launch it. Runs the lifecycle call on the caller's
/// thread; the window controller invokes this from a worker thread.
pub fn open_account_card(provider: ProviderId, id: &str) -> ManagerResult<()> {
    if crate::manager::lifecycle::focus_account_window(provider, id)? {
        return Ok(());
    }
    crate::manager::lifecycle::open_account(provider, id)
}

/// Focus an account window for a widget slot. Returns `true` when a window
/// was brought forward.
pub fn focus_account_card(provider: ProviderId, id: &str) -> ManagerResult<bool> {
    crate::manager::lifecycle::focus_account_window(provider, id)
}

/// Provider key parsed from theme/mouse/menu strings (`"claude"`, `"codex"`).
pub fn provider_from_key(key: &str) -> Option<ProviderId> {
    crate::providers::PROVIDER_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.key.eq_ignore_ascii_case(key.trim()))
        .map(|descriptor| descriptor.id)
}
