//! Scoped account-data deletion: read-only preview plus guarded apply.
//!
//! Four scopes, narrowest first: `AccountCache` (proven regenerable caches
//! only, no backup needed), `AccountLogout` (auth material only; config,
//! history, and skill links are kept), `AccountLocalData` (logout plus
//! account-local config/history deletion), and `FullReset` (every known
//! user-owned data source for the selected account, external Default
//! canonical data only with explicit opt-in).
//!
//! Flow: [`preview_cleanup`] enumerates exact targets with proven ownership
//! and is strictly read-only (no backup, no stop, no delete, no settings or
//! task changes). [`apply_cleanup`] revalidates the plan against a fresh
//! registry, re-derives targets from the resolved [`AccountContext`] (never
//! trusting submitted plan paths or `backup_id`), waits bounded for an
//! in-flight poll/launch via [`state::AccountOperationGuard`], quiesces the
//! account, creates and verifies an encrypted snapshot covering every
//! deletion target, then deletes with a per-target journal.
//! [`apply_cleanup_irreversible`] is the explicit no-backup variant used only
//! for the CLI `--no-backup` flag and a clear GUI confirmation.
//!
//! Safety rules enforced here: unknown junctions/links hard-stop the whole
//! apply before any mutation; shared skill links are removed by inode only
//! ([`skills::remove_link_only`], never traversing the target); credential
//! vault entries go only through the backup-owned credentials helper for
//! confirmed owned targets (no prefix-wide wipe); the fork backups root and
//! the shared skills library are never deletion targets; registry removal
//! happens only after complete owned-data cleanup and preserves retired ids.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountOwnership, AccountProfile};
use crate::manager::{
    paths, skills, state, AccountContext, BackupInfo, CleanupPlan, CleanupScope, CleanupTarget,
    CleanupTargetKind, ManagerResult,
};
use crate::providers::ProviderId;

/// Bounded wait for an in-flight poll or launch on the same account before
/// apply gives up. Other accounts are never blocked: the guard is per
/// account, and the settings mutex is held only for short registry commits.
pub const ACCOUNT_GUARD_TIMEOUT_MS: u32 = 35_000;

/// Proven regenerable browser-style cache directory names. Only these are
/// ever `Cache`-kind targets; LocalStorage, IndexedDB, ServiceWorker, and
/// user history are never cache targets.
const CACHE_DIR_NAMES: &[&str] = &["Cache", "Code Cache", "GPUCache"];

/// CLI `--scope` spellings (single spelling owner for the CLI slice).
pub fn scope_from_cli_name(name: &str) -> ManagerResult<CleanupScope> {
    match name.trim().to_lowercase().as_str() {
        "cache" => Ok(CleanupScope::AccountCache),
        "logout" => Ok(CleanupScope::AccountLogout),
        "delete" => Ok(CleanupScope::AccountLocalData),
        "reset" => Ok(CleanupScope::FullReset),
        _ => Err(format!(
            "Unknown cleanup scope '{name}'; expected cache|logout|delete|reset"
        )),
    }
}

/// What happened to one planned target during apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupAction {
    /// A regular file was removed.
    Deleted,
    /// A validated account skill link was removed by inode only; the shared
    /// library target was never traversed.
    Unlinked,
    /// A directory tree was removed after a link pre-scan proved it
    /// link-free (except validated skill links, removed first by inode).
    RemovedDirectory,
    /// A planned structural link preserved in place (never deleted, never
    /// traversed); used for migration-linked canonical skills junctions.
    Preserved,
    /// Known OAuth token keys were redacted from a desktop config file;
    /// every other key was preserved and the file was kept.
    Scrubbed,
    /// The target was already absent; a no-op, never a failure.
    NoopMissing,
}

/// Per-target outcome. `ok == false` on any entry makes the report
/// unsuccessful; already-recorded entries stay exact so a retry resumes
/// idempotently (completed targets are now absent and report no-ops).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupTargetResult {
    pub path: PathBuf,
    pub kind: CleanupTargetKind,
    pub action: CleanupAction,
    pub bytes_reclaimed: u64,
    pub ok: bool,
    pub message: String,
}

/// Outcome of [`apply_cleanup`]/[`apply_cleanup_irreversible`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupReport {
    pub scope: CleanupScope,
    pub provider: ProviderId,
    pub account_id: String,
    /// Verified encrypted snapshot covering the deleted targets. `None` for
    /// pure regenerable-cache plans and for the explicit irreversible path.
    pub backup_id: Option<String>,
    pub results: Vec<CleanupTargetResult>,
    pub warnings: Vec<String>,
    /// True only when the profile entry was actually removed from the
    /// registry (managed accounts, delete/reset scopes, after all owned data
    /// was cleaned). Retired ids are always preserved.
    pub removed_from_registry: bool,
    pub success: bool,
}

/// Enumerate exact targets with proven ownership. Strictly read-only: no
/// backup, no process stop, no deletion, no settings or task changes.
pub fn preview_cleanup(ctx: &AccountContext, scope: CleanupScope) -> ManagerResult<CleanupPlan> {
    let (targets, warnings) = derive_targets(ctx, scope)?;
    Ok(CleanupPlan {
        scope,
        provider: ctx.provider,
        account_id: ctx.profile.id.clone(),
        targets,
        backup_id: None,
        warnings,
    })
}

/// Guarded apply with an encrypted snapshot: revalidates against a fresh
/// registry, holds the per-account guard across quiesce/snapshot/verify/
/// delete, creates and verifies a snapshot covering every deletion target
/// before any credential/file/registry mutation, then deletes with a
/// per-target journal. Pure regenerable-cache plans need no backup.
pub fn apply_cleanup(plan: &CleanupPlan) -> ManagerResult<CleanupReport> {
    apply_impl(plan, true)
}

/// Explicit no-backup variant. Only the CLI `--no-backup` flag (plus confirm)
/// and a clear GUI confirmation may use this; the journal records that the
/// caller proceeded without an encrypted snapshot.
pub fn apply_cleanup_irreversible(plan: &CleanupPlan) -> ManagerResult<CleanupReport> {
    apply_impl(plan, false)
}

fn apply_impl(plan: &CleanupPlan, with_backup: bool) -> ManagerResult<CleanupReport> {
    // Bounded wait for an in-flight poll/launch on THIS account first, then
    // hold the guard through revalidation, stop, snapshot, verify, and
    // delete. The global settings mutex is used only for the short registry
    // commit at the end. Resolving after the wait keeps the freshest view.
    let _account_guard = state::AccountOperationGuard::acquire(
        plan.provider,
        &plan.account_id,
        ACCOUNT_GUARD_TIMEOUT_MS,
    )
    .map_err(|error| {
        format!(
            "Cannot start cleanup for '{}': {error}. Retry when the account is idle",
            plan.account_id
        )
    })?;

    // Fresh-registry revalidation. Submitted plan paths and `backup_id` are
    // never trusted; scope/provider/account are confirmed against the live
    // registry and every target below is re-derived from `ctx`.
    let fresh = resolve_context(plan.provider, &plan.account_id)?;

    // Graceful quiesce of the selected account only (lifecycle takes no
    // account guard around stop by design). Ok without a verified idle state
    // is NOT quiesced: abort before snapshot or any mutation.
    let stop =
        super::lifecycle::stop_account_processes(plan.provider, &plan.account_id).map_err(
            |error| {
                format!(
                    "Unable to quiesce account '{}' processes: {error}. Cleanup aborted before any change",
                    plan.account_id
                )
            },
        )?;
    if stop.attribution_unknown || !stop.remaining_pids.is_empty() || !stop.verified_idle {
        let why = if stop.attribution_unknown {
            "attribution unknown"
        } else {
            "processes possibly still running"
        };
        return Err(format!(
            "Account '{}' is not verified idle ({}); cleanup aborted before any change",
            plan.account_id, why
        ));
    }

    let mut warnings = vec![
        "Account processes quiesced; polling and launches on this account were blocked for the operation"
            .to_string(),
    ];
    let (mut derived, mut derive_warnings) = derive_targets(&fresh, plan.scope)?;
    warnings.append(&mut derive_warnings);

    // Preview confirmation: every freshly derived target must have been in
    // the confirmed preview. Submitted extras are ignored (never trusted);
    // a derived target the user never confirmed aborts the apply as stale.
    let submitted: BTreeSet<String> = plan.targets.iter().map(|t| norm_key(&t.path)).collect();
    for target in &derived {
        if !submitted.contains(&norm_key(&target.path)) {
            return Err(format!(
                "Cleanup plan is stale: '{}' was not in the confirmed preview. Re-run preview and confirm again; nothing was changed",
                target.path.display()
            ));
        }
    }

    // Unknown-junction hard stop BEFORE any mutation or snapshot, so a
    // refused tree never leaves an orphan verified backup behind.
    prescan_no_surprise_links(&derived)?;

    // Snapshot coverage: every deletion target must be covered and verified
    // before any mutation. Pure regenerable-cache plans are exempt. Vault
    // entries ride in the snapshot via the credentials capture and are then
    // proven against the authenticated attestation (see
    // `verify_credential_coverage`); with no file targets (vault-only logout,
    // empty delete, or a registry retry after assets are gone) the full
    // account snapshot covers vault entries plus registry metadata so the
    // 7-day undo still holds.
    let needs_backup = with_backup
        && plan.scope != CleanupScope::AccountCache
        && (!derived.is_empty()
            || matches!(
                plan.scope,
                CleanupScope::AccountLocalData | CleanupScope::FullReset
            ));
    let backup_id = if needs_backup {
        // Vault targets recorded by the authenticated attestation; assigned
        // by both snapshot branches below and consulted before any mutation.
        let credential_coverage: BTreeSet<String>;
        let mut cover: Vec<PathBuf> = Vec::new();
        for target in derived
            .iter()
            .filter(|t| t.kind != CleanupTargetKind::CredentialTarget)
        {
            if std::fs::symlink_metadata(&target.path).is_ok() {
                cover.push(target.path.clone());
            } else {
                warnings.push(format!(
                    "Target '{}' vanished before the snapshot; it will delete as a no-op",
                    target.path.display()
                ));
            }
        }
        // Desktop logout preserves Local State but the snapshot must still
        // cover it: key rotation in either file changes the poller identity,
        // and restore needs both.
        if plan.scope == CleanupScope::AccountLogout {
            if let Some(desktop) = fresh.desktop_dir.clone() {
                let local_state = desktop.join("Local State");
                if std::fs::symlink_metadata(&local_state).is_ok()
                    && !cover.iter().any(|p| norm_key(p) == norm_key(&local_state))
                {
                    cover.push(local_state);
                    warnings.push(
                        "Local State is snapshotted for restore but never deleted by logout"
                            .to_string(),
                    );
                }
            }
        }
        let info: BackupInfo = if cover.is_empty() {
            let created = super::backup::create_backup(&fresh).map_err(|error| {
                format!("Encrypted snapshot failed, cleanup aborted before any change: {error}")
            })?;
            // The metadata/vault-only snapshot is authenticated here, and its
            // recorded vault targets are the coverage proof for the gate
            // below: one verify pass, reused for both jobs.
            let attestation = super::backup::coverage_attestation(&created.backup_id).map_err(
                |error| {
                    format!(
                        "Encrypted snapshot '{}' failed verification, cleanup aborted before any change: {error}",
                        created.backup_id
                    )
                },
            )?;
            credential_coverage = attestation.credential_targets.into_iter().collect();
            created
        } else {
            // Canonicalize every requested root (resolve-once spelling), then
            // prove coverage from the attestation: every requested root must
            // be present, or nothing is deleted.
            let mut canonical_cover = Vec::new();
            for root in &cover {
                canonical_cover.push(super::backup::canonical_requested_root(root).map_err(
                    |error| format!("Encrypted snapshot refused '{}': {error}", root.display()),
                )?);
            }
            let created = super::backup::create_backup_for_paths(&fresh, &canonical_cover)
                .map_err(|error| {
                    format!("Encrypted snapshot failed, cleanup aborted before any change: {error}")
                })?;
            let attestation = super::backup::coverage_attestation(&created.backup_id).map_err(
                |error| {
                    format!(
                        "Snapshot coverage unverifiable for '{}', cleanup aborted before any change: {error}",
                        created.backup_id
                    )
                },
            )?;
            for root in &canonical_cover {
                if !attestation
                    .roots
                    .iter()
                    .any(|r| norm_key(r) == norm_key(root))
                {
                    return Err(format!(
                        "Snapshot '{}' does not cover '{}'; cleanup aborted before any change",
                        created.backup_id,
                        root.display()
                    ));
                }
            }
            credential_coverage = attestation.credential_targets.into_iter().collect();
            created
        };
        // Both branches were authenticated exactly once by
        // coverage_attestation above; no second verify pass.
        warnings.push(format!(
            "Encrypted snapshot {} created and verified before any deletion",
            info.backup_id
        ));
        // Credential coverage gate: `delete_owned` removes every confirmed
        // owned vault entry that exists, but capture skips an existing
        // empty-blob entry, so a present target can sit outside the
        // authenticated snapshot. Refuse before any mutation unless each
        // currently present planned target is recorded there; absent targets
        // still delete as idempotent no-ops. The explicit irreversible
        // --no-backup path has no snapshot and stays unchanged.
        verify_credential_coverage(&fresh, &derived, &info.backup_id, &credential_coverage)?;
        Some(info.backup_id)
    } else if !with_backup {
        warnings.push(
            "Proceeded WITHOUT an encrypted snapshot by explicit irreversible request (--no-backup plus confirmation)"
                .to_string(),
        );
        None
    } else {
        warnings.push("Pure regenerable-cache plan: no snapshot needed".to_string());
        None
    };
    for target in &mut derived {
        target.covered_by_backup = backup_id.clone();
    }

    // Destructive gate: owned coverage plus a verified backup for every
    // non-cache scope unless explicitly irreversible.
    if !derived.is_empty() {
        if derived.iter().any(|t| !t.owned) {
            return Err(
                "Refusing cleanup: the plan contains unowned targets. Nothing was changed"
                    .to_string(),
            );
        }
        if with_backup && plan.scope != CleanupScope::AccountCache && backup_id.is_none() {
            return Err(
                "Refusing cleanup: no verified snapshot covers the targets. Nothing was changed"
                    .to_string(),
            );
        }
    }

    // Unknown-junction hard stop BEFORE any mutation.
    prescan_no_surprise_links(&derived)?;

    // Execute in passes: vault entries, skill-link inodes, files, then trees.
    let mut results = Vec::new();
    let mut aborted = false;
    if !exec_credential_targets(&fresh, &derived, &mut results, &mut warnings) {
        aborted = true;
    }
    if !aborted {
        // Protected structural links (validated skills junctions under
        // canonical roots) are preserved in place, never unlinked: the tree
        // walk skips them and the prescan already expects them.
        let canonical_paths = canonical_data_roots();
        for target in derived
            .iter()
            .filter(|t| t.kind == CleanupTargetKind::SkillsLink)
        {
            if canonical_paths
                .iter()
                .any(|c| paths::is_within(c, &target.path))
            {
                results.push(CleanupTargetResult {
                    path: target.path.clone(),
                    kind: target.kind,
                    action: CleanupAction::Preserved,
                    bytes_reclaimed: 0,
                    ok: true,
                    message: "Structural skills link preserved in place; shared target untouched"
                        .to_string(),
                });
                continue;
            }
            match skills::remove_link_only(&target.path) {
                Ok(()) => results.push(ok_result(target, CleanupAction::Unlinked)),
                Err(error) => {
                    results.push(fail_result(target, error));
                    aborted = true;
                    break;
                }
            }
        }
    }
    if !aborted {
        let scrub_path = desktop_scrub_path(&fresh);
        for target in derived.iter().filter(|t| {
            !matches!(
                t.kind,
                CleanupTargetKind::CredentialTarget
                    | CleanupTargetKind::SkillsLink
                    | CleanupTargetKind::Directory
                    | CleanupTargetKind::Config
            )
        }) {
            if scrub_path
                .as_ref()
                .is_some_and(|p| norm_key(p) == norm_key(&target.path))
            {
                // Desktop logout: redact only the proven OAuth token keys
                // and keep the file with every unrelated pref.
                match scrub_desktop_auth_keys(&target.path) {
                    Ok(action) => results.push(CleanupTargetResult {
                        path: target.path.clone(),
                        kind: target.kind,
                        action,
                        bytes_reclaimed: 0,
                        ok: true,
                        message: "OAuth token keys redacted; unrelated prefs preserved".to_string(),
                    }),
                    Err(error) => {
                        results.push(fail_result(target, error));
                        aborted = true;
                        break;
                    }
                }
                continue;
            }
            match delete_file_or_small_tree(target) {
                Ok(action) => results.push(ok_result(target, action)),
                Err(error) => {
                    results.push(fail_result(target, error));
                    aborted = true;
                    break;
                }
            }
        }
    }
    if !aborted {
        let clear_roots = canonical_clear_roots();
        for target in derived
            .iter()
            .filter(|t| t.kind == CleanupTargetKind::Directory)
        {
            let keep_root = clear_roots.contains(&norm_key(&target.path));
            let action = if keep_root {
                clear_contents_keep_root(&target.path)
            } else {
                delete_dir_tree(&target.path)
            };
            match action {
                Ok(result_action) => {
                    let mut result = ok_result(target, result_action);
                    if keep_root {
                        result.message =
                            "Contents cleared; root preserved for app launch".to_string();
                    }
                    results.push(result);
                }
                Err(error) => {
                    results.push(fail_result(target, error));
                    aborted = true;
                    break;
                }
            }
        }
    }
    if aborted {
        warnings.push(
            "Deletion aborted on first failure; completed entries stay exact and retry is idempotent"
                .to_string(),
        );
    }

    if !aborted {
        // Owned launcher artifacts go last, immediately before the final
        // registry commit, so a partial failure leaves launchers for the
        // data that is still there. No plaintext nickname survives in the
        // owned shortcuts root after a successful delete.
        for target in derived
            .iter()
            .filter(|t| t.kind == CleanupTargetKind::Config)
        {
            match delete_file_or_small_tree(target) {
                Ok(action) => results.push(ok_result(target, action)),
                Err(error) => {
                    results.push(fail_result(target, error));
                    aborted = true;
                    break;
                }
            }
        }
    }

    // Registry removal only after complete owned-data cleanup. Retired ids
    // are preserved (only the profile entry is removed). External accounts
    // keep their entry: reset wipes opted-in data, not the account slot.
    let data_ok = results.iter().all(|r| r.ok);
    let registry_required = matches!(
        plan.scope,
        CleanupScope::AccountLocalData | CleanupScope::FullReset
    ) && data_ok
        && fresh.is_managed();
    let mut removed_from_registry = false;
    if registry_required {
        match remove_profile_from_registry(plan.provider, &plan.account_id) {
            Ok(()) => removed_from_registry = true,
            Err(error) => warnings.push(format!(
                "Data deleted but registry removal failed (retry is safe): {error}"
            )),
        }
    } else if matches!(
        plan.scope,
        CleanupScope::AccountLocalData | CleanupScope::FullReset
    ) && data_ok
    {
        warnings.push(
            "External account entry preserved in the registry; only the opted-in data was wiped"
                .to_string(),
        );
    }

    let success = data_ok && (!registry_required || removed_from_registry);
    Ok(CleanupReport {
        scope: plan.scope,
        provider: plan.provider,
        account_id: plan.account_id.clone(),
        backup_id,
        results,
        warnings,
        removed_from_registry,
        success,
    })
}

/// Resolve and revalidate the account against a fresh registry load.
fn resolve_context(provider: ProviderId, account_id: &str) -> ManagerResult<AccountContext> {
    let settings = state::load_fresh();
    let accounts = settings
        .accounts
        .get(provider)
        .ok_or_else(|| "This provider does not support account profiles".to_string())?;
    let profile = accounts
        .find(account_id)
        .ok_or_else(|| format!("Unknown account '{account_id}'"))?
        .clone();
    AccountContext::from_profile(provider, &profile)
}

fn remove_profile_from_registry(provider: ProviderId, account_id: &str) -> ManagerResult<()> {
    state::mutate_settings(|settings| {
        let list = match provider {
            ProviderId::Claude => &mut settings.accounts.claude,
            ProviderId::Codex => &mut settings.accounts.codex,
            _ => return Err("This provider does not support account profiles".to_string()),
        };
        let position = list
            .find(account_id)
            .and_then(|_| list.profiles.iter().position(|p| p.id == account_id))
            .ok_or_else(|| format!("Unknown account '{account_id}'"))?;
        list.profiles.remove(position);
        Ok(())
    })?;
    Ok(())
}

fn ok_result(target: &CleanupTarget, action: CleanupAction) -> CleanupTargetResult {
    CleanupTargetResult {
        path: target.path.clone(),
        kind: target.kind,
        action,
        bytes_reclaimed: target.size_bytes,
        ok: true,
        message: String::new(),
    }
}

/// `ok: false` carries the failure; the action field is only meaningful when
/// `ok` is true (`NoopMissing` there means "already absent").
fn fail_result(target: &CleanupTarget, error: String) -> CleanupTargetResult {
    CleanupTargetResult {
        path: target.path.clone(),
        kind: target.kind,
        action: CleanupAction::NoopMissing,
        bytes_reclaimed: 0,
        ok: false,
        message: error,
    }
}

/// Gate before any mutation: every planned vault target that currently exists
/// must be recorded in the authenticated snapshot, or the whole cleanup is
/// refused. Capture and deletion share the profile target list, but capture
/// skips an existing empty-blob entry while `delete_owned` still removes it,
/// so presence has to be proven against the attestation instead of assumed.
/// Targets already covered are skipped without a vault read; an absent target
/// stays an idempotent no-op. A vault read error aborts (never a silent
/// "absent"): losing an entry to an unreadable vault would be unrecoverable.
fn verify_credential_coverage(
    ctx: &AccountContext,
    derived: &[CleanupTarget],
    backup_id: &str,
    covered: &BTreeSet<String>,
) -> ManagerResult<()> {
    for target in derived
        .iter()
        .filter(|t| t.kind == CleanupTargetKind::CredentialTarget)
    {
        let Some(entry) = ctx
            .profile
            .credential_targets
            .iter()
            .find(|entry| norm_key(Path::new(entry)) == norm_key(&target.path))
        else {
            // Not bound to the profile: `delete_owned` never touches it.
            continue;
        };
        // Capture records the trimmed target exactly; compare the same form.
        let name = entry.trim();
        if name.is_empty() || covered.contains(name) {
            continue;
        }
        match super::credentials::credential_exists(name) {
            Ok(false) => {}
            Ok(true) => {
                return Err(format!(
                    "Snapshot '{backup_id}' does not cover credential '{name}'; cleanup aborted before any change"
                ))
            }
            Err(error) => {
                return Err(format!(
                    "Unable to confirm credential '{name}' is covered by snapshot '{backup_id}', cleanup aborted before any change: {error}"
                ))
            }
        }
    }
    Ok(())
}

/// Delete all confirmed owned vault entries in one helper call, then map one
/// journal entry per planned target. Absent entries are no-ops, never
/// failures. Returns false when the vault call itself failed (caller aborts).
fn exec_credential_targets(
    ctx: &AccountContext,
    derived: &[CleanupTarget],
    results: &mut Vec<CleanupTargetResult>,
    warnings: &mut Vec<String>,
) -> bool {
    let planned: Vec<&CleanupTarget> = derived
        .iter()
        .filter(|t| t.kind == CleanupTargetKind::CredentialTarget)
        .collect();
    if planned.is_empty() {
        return true;
    }
    match super::credentials::delete_owned(ctx) {
        Ok(deleted) => {
            let gone: BTreeSet<&str> = deleted.iter().map(String::as_str).collect();
            for target in planned {
                // Credential targets are vault names, not filesystem paths;
                // match by the exact confirmed target string.
                let owned_name = ctx
                    .profile
                    .credential_targets
                    .iter()
                    .find(|entry| norm_key(Path::new(entry)) == norm_key(&target.path));
                let deleted_hit = owned_name
                    .map(|entry| gone.contains(entry.as_str()))
                    .unwrap_or(false);
                if deleted_hit {
                    results.push(ok_result(target, CleanupAction::Deleted));
                } else {
                    results.push(CleanupTargetResult {
                        path: target.path.clone(),
                        kind: target.kind,
                        action: CleanupAction::NoopMissing,
                        bytes_reclaimed: 0,
                        ok: true,
                        message: "Credential entry already absent; no-op".to_string(),
                    });
                }
            }
            warnings.push(format!(
                "Credential vault: {} confirmed entries deleted, remainder already absent",
                deleted.len()
            ));
            true
        }
        Err(error) => {
            for target in planned {
                results.push(fail_result(
                    target,
                    format!("Credential vault deletion failed: {error}"),
                ));
            }
            false
        }
    }
}

/// Remove one non-directory target. Cache-kind targets that turn out to be
/// directories are removed as small trees. Missing files are no-ops.
fn delete_file_or_small_tree(target: &CleanupTarget) -> Result<CleanupAction, String> {
    let meta = match std::fs::symlink_metadata(&target.path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CleanupAction::NoopMissing)
        }
        Err(error) => {
            return Err(format!(
                "Unable to stat '{}': {error}",
                target.path.display()
            ))
        }
    };
    if paths::is_reparse_point(&target.path) {
        return Err(format!(
            "Refusing '{}': unexpected link; resolve manually then retry",
            target.path.display()
        ));
    }
    if meta.is_dir() {
        return delete_dir_tree(&target.path);
    }
    std::fs::remove_file(&target.path).map_err(|error| lock_or_plain(&target.path, error))?;
    Ok(CleanupAction::Deleted)
}

fn lock_or_plain(path: &Path, error: std::io::Error) -> String {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ResourceBusy
    ) || error
        .raw_os_error()
        .is_some_and(|code| code == 32 || code == 33)
    {
        format!(
            "Occupied lock on '{}': {error}. Close the owning app and retry",
            path.display()
        )
    } else {
        format!("Unable to delete '{}': {error}", path.display())
    }
}

/// Remove a directory tree that the pre-scan proved link-free. Any link
/// appearing mid-delete (TOCTOU) aborts the tree immediately. A missing root
/// is a no-op.
fn delete_dir_tree(path: &Path) -> Result<CleanupAction, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CleanupAction::NoopMissing)
        }
        Err(error) => return Err(format!("Unable to stat '{}': {error}", path.display())),
    };
    if paths::is_reparse_point(path) {
        return Err(format!(
            "Refusing '{}': unexpected link; resolve manually then retry",
            path.display()
        ));
    }
    if !meta.is_dir() {
        std::fs::remove_file(path).map_err(|error| lock_or_plain(path, error))?;
        return Ok(CleanupAction::Deleted);
    }
    remove_tree_contents(path)?;
    std::fs::remove_dir(path).map_err(|error| lock_or_plain(path, error))?;
    Ok(CleanupAction::RemovedDirectory)
}

fn remove_tree_contents(root: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(root).map_err(|error| lock_or_plain(root, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| lock_or_plain(root, error))?;
        let path = entry.path();
        if paths::is_reparse_point(&entry.path()) {
            // Protected structural links (validated skills junctions
            // preserved in place) are skipped; anything else aborts before
            // it can be traversed.
            if skills::validate_skill_link(&path, &shared_roots()).is_ok() {
                continue;
            }
            return Err(format!(
                "Refusing '{}': link appeared during delete; retry after resolving it",
                path.display()
            ));
        }
        let kind = entry
            .file_type()
            .map_err(|error| lock_or_plain(&path, error))?;
        if kind.is_dir() {
            remove_tree_contents(&path)?;
            std::fs::remove_dir(&path).map_err(|error| lock_or_plain(&path, error))?;
        } else {
            std::fs::remove_file(&path).map_err(|error| lock_or_plain(&path, error))?;
        }
    }
    Ok(())
}

/// Hard stop on any unexpected reparse point, before ANY mutation.
/// Validated account skill links (planned `SkillsLink` targets) are the only
/// expected links; vault entries have no filesystem form.
fn prescan_no_surprise_links(targets: &[CleanupTarget]) -> ManagerResult<()> {
    let expected_links: BTreeSet<String> = targets
        .iter()
        .filter(|t| t.kind == CleanupTargetKind::SkillsLink)
        .map(|t| norm_key(&t.path))
        .collect();
    let mut bad = Vec::new();
    for target in targets {
        if target.kind == CleanupTargetKind::CredentialTarget {
            continue;
        }
        if target.kind == CleanupTargetKind::SkillsLink {
            if std::fs::symlink_metadata(&target.path).is_ok()
                && skills::validate_skill_link(&target.path, &shared_roots()).is_err()
            {
                bad.push(format!(
                    "'{}': account link no longer validates against the shared library",
                    target.path.display()
                ));
            }
            continue;
        }
        if let Some(point) = paths::first_reparse_point(&target.path) {
            bad.push(format!(
                "'{}': unexpected link at '{}'",
                target.path.display(),
                point.display()
            ));
            continue;
        }
        if target.kind == CleanupTargetKind::Directory
            && std::fs::symlink_metadata(&target.path).is_ok()
        {
            collect_descendant_links(&target.path, &expected_links, &mut bad);
        }
    }
    if !bad.is_empty() {
        return Err(format!(
            "Refusing cleanup: unexpected links (resolve manually, then retry). No data was changed: {}",
            bad.join("; ")
        ));
    }
    Ok(())
}

fn collect_descendant_links(root: &Path, expected: &BTreeSet<String>, bad: &mut Vec<String>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if paths::is_reparse_point(&entry.path()) {
            if !expected.contains(&norm_key(&path)) {
                bad.push(format!("'{}': unexpected link", path.display()));
            }
            continue;
        }
        if kind.is_dir() {
            collect_descendant_links(&path, expected, bad);
        }
    }
}

// ---------------------------------------------------------------------------
// Target derivation (read-only)
// ---------------------------------------------------------------------------

fn derive_targets(
    ctx: &AccountContext,
    scope: CleanupScope,
) -> ManagerResult<(Vec<CleanupTarget>, Vec<String>)> {
    // External Default data is only ever resolved for an explicit FullReset;
    // every other scope sees an empty plan for that account.
    if scope != CleanupScope::FullReset && is_external_default(ctx) {
        return Ok((
            Vec::new(),
            vec![
                "External Default account: only an explicit FullReset covers external data"
                    .to_string(),
            ],
        ));
    }
    let mut targets = Vec::new();
    let mut warnings = Vec::new();
    match scope {
        CleanupScope::AccountCache => collect_cache_targets(ctx, &mut targets, &mut warnings)?,
        CleanupScope::AccountLogout => {
            collect_logout_targets(ctx, &mut targets, &mut warnings)?;
        }
        CleanupScope::AccountLocalData => {
            collect_logout_targets(ctx, &mut targets, &mut warnings)?;
            collect_local_targets(ctx, &mut targets, &mut warnings)?;
        }
        CleanupScope::FullReset => {
            collect_logout_targets(ctx, &mut targets, &mut warnings)?;
            collect_local_targets(ctx, &mut targets, &mut warnings)?;
            collect_reset_targets(ctx, &mut targets, &mut warnings)?;
        }
    }
    // Owned launcher artifacts for Managed delete/reset: exact-owned-id
    // shortcut/icon files enumerated by lifecycle (read-only). They are
    // snapshotted like other files and deleted last, immediately before the
    // final registry commit, so no plaintext nickname survives in the owned
    // shortcuts root.
    if matches!(
        scope,
        CleanupScope::AccountLocalData | CleanupScope::FullReset
    ) && ctx.is_managed()
    {
        match super::native::owned_account_artifact_paths(ctx) {
            Ok(artifacts) => {
                let mut found = 0;
                for artifact in artifacts {
                    if std::fs::symlink_metadata(&artifact).is_err() {
                        continue;
                    }
                    if paths::is_reparse_point(&artifact) {
                        warnings.push(format!(
                            "Owned artifact '{}' is a link; left in place, apply will refuse it",
                            artifact.display()
                        ));
                        continue;
                    }
                    let size_bytes = std::fs::metadata(&artifact).map(|m| m.len()).unwrap_or(0);
                    targets.push(CleanupTarget {
                        path: artifact,
                        kind: CleanupTargetKind::Config,
                        owned: true,
                        size_bytes,
                        covered_by_backup: None,
                    });
                    found += 1;
                }
                if found > 0 {
                    warnings.push(
                        "Owned launcher artifacts are removed last, just before the registry commit; the snapshot covers their restore"
                            .to_string(),
                    );
                }
            }
            Err(error) => {
                return Err(format!(
                    "Unable to enumerate owned launcher artifacts: {error}. Nothing was changed"
                ))
            }
        }
    }

    finalize_targets(&mut targets, &mut warnings);
    // Snapshot ceiling hints (mirror backup caps): warn in preview when a
    // tree or file this large would abort apply-with-backup.
    for target in &targets {
        if target.kind == CleanupTargetKind::Directory
            && target.size_bytes > SNAPSHOT_TREE_SIZE_HINT
        {
            warnings.push(format!(
                "Tree '{}' exceeds the snapshot ceiling; apply with backup would abort there — shrink it or use the explicit irreversible path",
                target.path.display()
            ));
        }
        if target.kind != CleanupTargetKind::Directory
            && target.kind != CleanupTargetKind::CredentialTarget
            && target.size_bytes > SNAPSHOT_FILE_SIZE_HINT
        {
            warnings.push(format!(
                "File '{}' exceeds the snapshot per-file ceiling; apply with backup would abort there — shrink it or use the explicit irreversible path",
                target.path.display()
            ));
        }
    }
    if matches!(
        scope,
        CleanupScope::AccountLocalData | CleanupScope::FullReset
    ) {
        warnings.append(&mut reject_shared_source(ctx, &targets)?);
    }
    Ok((targets, warnings))
}

fn collect_cache_targets(
    ctx: &AccountContext,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) -> ManagerResult<()> {
    let mut bases = Vec::new();
    if let Ok(dir) = paths::managed_account_dir(ctx.provider, &ctx.profile.id) {
        bases.push(dir);
    }
    bases.extend(ctx.config_dir.clone());
    bases.extend(ctx.desktop_dir.clone());
    let mut seen_base = BTreeSet::new();
    bases.retain(|b| seen_base.insert(norm_key(b)));
    let mut found = 0;
    for base in bases {
        for name in CACHE_DIR_NAMES {
            let candidate = base.join(name);
            if std::fs::symlink_metadata(&candidate).is_err() {
                continue;
            }
            if paths::first_reparse_point(&candidate).is_some() {
                warnings.push(format!(
                    "Cache path '{}' is a link; apply will refuse until it is resolved",
                    candidate.display()
                ));
            }
            targets.push(CleanupTarget {
                path: candidate.clone(),
                kind: CleanupTargetKind::Cache,
                owned: true,
                size_bytes: dir_size(&candidate),
                covered_by_backup: None,
            });
            found += 1;
        }
    }
    if found == 0 {
        warnings.push("No regenerable cache directories found; nothing to clear".to_string());
    }
    Ok(())
}

fn collect_logout_targets(
    ctx: &AccountContext,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) -> ManagerResult<()> {
    if let Some(file) = ctx.credential_file.clone() {
        if std::fs::symlink_metadata(&file).is_ok() {
            let size_bytes = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            targets.push(CleanupTarget {
                path: file,
                kind: CleanupTargetKind::Auth,
                owned: true,
                size_bytes,
                covered_by_backup: None,
            });
        } else {
            warnings.push("Credential file absent; nothing to sign out there".to_string());
        }
    }
    for entry in &ctx.profile.credential_targets {
        targets.push(CleanupTarget {
            // Vault names are not filesystem paths; the path field carries
            // the exact confirmed target string for journal matching.
            path: PathBuf::from(entry),
            kind: CleanupTargetKind::CredentialTarget,
            owned: true,
            size_bytes: 0,
            covered_by_backup: None,
        });
    }
    // Desktop logout redacts only the two proven OAuth token cache keys in
    // config.json (auth-redaction, never a file wipe); the bound identity
    // `lastKnownAccountUuid` is preserved so the reader stays pinned to
    // NoCredentials. Local State is snapshotted for restore but never
    // touched. The proven Second-layout cookie stores are cleared; every
    // other Network file, pref, and history item is kept.
    if let Some(desktop) = ctx.desktop_dir.clone() {
        if let Some(config) = desktop_scrub_path(ctx) {
            let size_bytes = std::fs::metadata(&config).map(|m| m.len()).unwrap_or(0);
            targets.push(CleanupTarget {
                path: config,
                kind: CleanupTargetKind::Auth,
                owned: true,
                size_bytes,
                covered_by_backup: None,
            });
            warnings.push(
                "Desktop config.json: only the OAuth token cache keys are redacted by logout; identity, prefs, history, and skills are kept"
                    .to_string(),
            );
        }
        let mut cookies_found = 0;
        for name in [
            "Network/Cookies",
            "Network/Cookies-journal",
            "Cookies",
            "Cookies-journal",
        ] {
            let cookie = desktop.join(name);
            if std::fs::symlink_metadata(&cookie).is_err() {
                continue;
            }
            if paths::is_reparse_point(&cookie) {
                warnings.push(format!(
                    "Cookie store '{}' is a link; left in place, apply will refuse it",
                    cookie.display()
                ));
                continue;
            }
            let size_bytes = std::fs::metadata(&cookie).map(|m| m.len()).unwrap_or(0);
            targets.push(CleanupTarget {
                path: cookie,
                kind: CleanupTargetKind::Auth,
                owned: true,
                size_bytes,
                covered_by_backup: None,
            });
            cookies_found += 1;
        }
        if cookies_found > 0 {
            warnings.push(
                "Cookie session stores cleared; transport security, persistent state, prefs, and history are kept"
                    .to_string(),
            );
        }
    }
    warnings.push("Configuration, history, and skill links are kept by logout".to_string());
    Ok(())
}

fn collect_local_targets(
    ctx: &AccountContext,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) -> ManagerResult<()> {
    // Whole managed profile directory (the fork-owned initial location).
    let managed = paths::managed_account_dir(ctx.provider, &ctx.profile.id).ok();
    if let Some(dir) = managed.clone() {
        if std::fs::symlink_metadata(&dir).is_ok() {
            targets.push(CleanupTarget {
                path: dir,
                kind: CleanupTargetKind::Directory,
                owned: true,
                size_bytes: 0,
                covered_by_backup: None,
            });
            if let Some(last) = targets.last_mut() {
                last.size_bytes = dir_size(&last.path.clone());
            }
        }
    }
    collect_skill_link_targets(ctx, targets, warnings)?;
    // Authoritative deletable root from lifecycle (import-time ownership:
    // adopted Second trees stay Managed in place, roots/symlinks refused at
    // import). Additional registered bases go through the same validation
    // below; anything unowned is reported, never deleted.
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(root) = super::lifecycle::owned_deletion_root(ctx)? {
        bases.push(root);
    }
    for base in ctx.config_dir.iter().chain(ctx.desktop_dir.iter()) {
        if !bases.iter().any(|b| norm_key(b) == norm_key(base)) {
            bases.push(base.clone());
        }
    }
    for base in &bases {
        if managed
            .as_ref()
            .is_some_and(|m| norm_key(m) == norm_key(base))
        {
            continue;
        }
        if std::fs::symlink_metadata(base).is_err() {
            continue;
        }
        if !base.is_dir() {
            // A file root (e.g. a file-only credential) is covered by the
            // logout-phase Auth target; anything else is reported.
            let is_cred_file = ctx
                .credential_file
                .as_ref()
                .is_some_and(|f| norm_key(f) == norm_key(base));
            if !is_cred_file {
                warnings.push(format!(
                    "Adopted path '{}' is not a directory; left in place",
                    base.display()
                ));
            }
            continue;
        }
        if !ctx.is_managed() {
            warnings.push(format!(
                "Adopted path '{}' is left in place; remove it manually if intended",
                base.display()
            ));
            continue;
        }
        match validate_adopted_tree(ctx, base) {
            Ok(()) => {
                let size = dir_size(base);
                targets.push(CleanupTarget {
                    path: base.clone(),
                    kind: CleanupTargetKind::Directory,
                    owned: true,
                    size_bytes: size,
                    covered_by_backup: None,
                });
            }
            Err(reason) => warnings.push(format!(
                "Adopted path '{}' left in place: {reason}",
                base.display()
            )),
        }
    }
    Ok(())
}

/// Proven desktop auth keys redacted on logout inside
/// `<desktop_dir>/config.json`: exactly the two OAuth token caches (V2
/// preferred, legacy fallback). The bound identity `lastKnownAccountUuid` is
/// deliberately PRESERVED: with caches emptied, the kept binding pins the
/// reader to NoCredentials, while blanking it would drop to legacy behavior
/// where unknown leftover keys could be picked up. Every other key
/// (preferences, history pointers, dxt allowlist/boot cache, …) is preserved
/// by value.
const DESKTOP_TOKEN_KEYS: &[&str] = &["oauth:tokenCacheV2", "oauth:tokenCache"];

/// Informational snapshot ceiling mirrors (backup caps): warn in preview when
/// a single tree (64 GiB total) or file (16 GiB) exceeds them, since
/// apply-with-backup would abort there.
const SNAPSHOT_TREE_SIZE_HINT: u64 = 64 * 1024 * 1024 * 1024;
const SNAPSHOT_FILE_SIZE_HINT: u64 = 16 * 1024 * 1024 * 1024;

/// The desktop config whose token keys logout redacts, if present and not
/// itself a link. Single source of truth for preview and apply.
fn desktop_scrub_path(ctx: &AccountContext) -> Option<PathBuf> {
    let config = ctx.desktop_dir.clone()?.join("config.json");
    if std::fs::symlink_metadata(&config).is_ok()
        && paths::first_reparse_point(&config).as_deref() != Some(config.as_path())
    {
        Some(config)
    } else {
        None
    }
}

/// Atomically redact only the proven OAuth token keys from a desktop
/// config.json, preserving every other key. Verifies post-write that no
/// token key retains a value; any failure aborts with no success claim, so
/// a retained secret can never be labeled a logout.
fn scrub_desktop_auth_keys(config: &Path) -> ManagerResult<CleanupAction> {
    let text = std::fs::read_to_string(config)
        .map_err(|error| format!("Unable to read '{}': {error}", config.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
        format!(
            "Refusing to scrub '{}': not JSON ({error}); nothing was changed",
            config.display()
        )
    })?;
    let map = value.as_object_mut().ok_or_else(|| {
        format!(
            "Refusing to scrub '{}': top level is not an object",
            config.display()
        )
    })?;
    let mut changed = false;
    for key in DESKTOP_TOKEN_KEYS {
        match map.get(*key) {
            Some(value) if value.as_str().is_some_and(|s| s.is_empty()) => {}
            Some(_) => {
                map.insert((*key).to_string(), serde_json::Value::String(String::new()));
                changed = true;
            }
            None => {}
        }
    }
    if !changed {
        return Ok(CleanupAction::NoopMissing);
    }
    let tmp = config.with_extension("json.scrub-tmp");
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    if let Err(error) = std::fs::write(&tmp, rendered) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "Unable to stage scrubbed '{}': {error}",
            config.display()
        ));
    }
    if let Err(error) = std::fs::rename(&tmp, config) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "Unable to atomically replace '{}': {error}",
            config.display()
        ));
    }
    let check: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config)
            .map_err(|error| format!("Unable to verify '{}': {error}", config.display()))?,
    )
    .map_err(|error| {
        format!(
            "Scrub verification failed for '{}': {error}",
            config.display()
        )
    })?;
    for key in DESKTOP_TOKEN_KEYS {
        if check
            .get(*key)
            .is_some_and(|value| !value.as_str().is_some_and(|s| s.is_empty()))
        {
            return Err(format!(
                "Scrub verification failed for '{}': token key '{key}' still holds a value",
                config.display()
            ));
        }
    }
    Ok(CleanupAction::Scrubbed)
}

/// Refuse whole-tree deletion when another registered profile references the
/// same or an overlapping directory source. Credential files match by
/// equality only; a credential file nested inside a wiped tree is
/// snapshotted with it and reported (not refused), since the snapshot covers
/// it and the owning account only needs re-login. Runs for delete/reset
/// scopes with Directory targets, in preview and apply alike.
fn reject_shared_source(
    ctx: &AccountContext,
    targets: &[CleanupTarget],
) -> ManagerResult<Vec<String>> {
    let mut extra_warnings = Vec::new();
    if !targets
        .iter()
        .any(|t| t.kind == CleanupTargetKind::Directory)
    {
        return Ok(extra_warnings);
    }
    let mut mine_dirs = data_dir_roots(ctx.provider, &ctx.profile);
    // Canonical reset targets are account data too: a managed adopted tree
    // nested inside (or equal to) canonical data must refuse the reset.
    for target in targets
        .iter()
        .filter(|t| t.kind == CleanupTargetKind::Directory)
    {
        if !mine_dirs
            .iter()
            .any(|m| norm_key(m) == norm_key(&target.path))
        {
            mine_dirs.push(target.path.clone());
        }
    }
    let mine_files = data_file_roots(ctx.provider, &ctx.profile);
    let settings = state::load_fresh();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let Some(list) = settings.accounts.get(provider) else {
            continue;
        };
        for other in &list.profiles {
            if provider == ctx.provider && other.id == ctx.profile.id {
                continue;
            }
            for theirs in data_dir_roots(provider, other) {
                if mine_dirs.iter().any(|m| roots_alias(m, &theirs)) {
                    return Err(format!(
                        "Refusing: account '{}' references the same data source (alias of '{}'); resolve the sharing before deleting either account",
                        other.id,
                        theirs.display()
                    ));
                }
            }
            for theirs in data_file_roots(provider, other) {
                if mine_files.iter().any(|m| norm_key(m) == norm_key(&theirs)) {
                    return Err(format!(
                        "Refusing: account '{}' shares the credential file '{}'; resolve the sharing before deleting either account",
                        other.id,
                        theirs.display()
                    ));
                }
                for mine in mine_dirs
                    .iter()
                    .filter(|m| norm_key(m) != norm_key(&theirs))
                {
                    if paths::is_within(mine, &theirs) {
                        extra_warnings.push(format!(
                            "Account '{}' references credential file '{}' inside the wiped tree; it is covered by the snapshot, but that account will need re-login",
                            other.id,
                            theirs.display()
                        ));
                        break;
                    }
                }
            }
        }
    }
    Ok(extra_warnings)
}

/// True for directory relationships: same, ancestor, or descendant
/// (case-insensitive, slash/NT-prefix normalized). Files use plain equality.
fn roots_alias(a: &Path, b: &Path) -> bool {
    norm_key(a) == norm_key(b) || paths::is_within(a, b) || paths::is_within(b, a)
}

/// Explicit directory roots for alias comparison: the managed dir plus every
/// registered explicit directory.
fn data_dir_roots(provider: ProviderId, profile: &AccountProfile) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = paths::managed_account_dir(provider, &profile.id) {
        roots.push(dir);
    }
    for raw in [&profile.config_dir, &profile.desktop_dir] {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(expanded) = crate::accounts::expand_path(Path::new(trimmed)) {
            roots.push(expanded);
        }
    }
    roots
}

/// Explicit credential files for alias comparison, resolved when possible.
/// File aliases only ever match by equality since files contain nothing.
fn data_file_roots(provider: ProviderId, profile: &AccountProfile) -> Vec<PathBuf> {
    if let Ok(ctx) = AccountContext::from_profile(provider, profile) {
        return ctx.credential_file.into_iter().collect();
    }
    if profile.credentials_path.trim().is_empty() {
        return Vec::new();
    }
    crate::accounts::expand_path(Path::new(profile.credentials_path.trim()))
        .into_iter()
        .collect()
}

fn collect_skill_link_targets(
    ctx: &AccountContext,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) -> ManagerResult<()> {
    let mut bases = Vec::new();
    if let Ok(dir) = skills::account_skills_dir(ctx.provider, &ctx.profile.id) {
        bases.push(dir);
    }
    for base in ctx.config_dir.iter().chain(ctx.desktop_dir.iter()) {
        bases.push(base.join("skills"));
    }
    let roots = shared_roots();
    let mut seen = BTreeSet::new();
    for base in bases {
        // The same skills dir can be listed twice (desktop_dir doubling as
        // the managed root, or desktop_dir == config_dir); the junction
        // branch below pushes unconditionally, so dedupe here — otherwise
        // the second unlink hits a missing path and aborts the apply.
        if !seen.insert(norm_key(&base)) {
            continue;
        }
        // The account skills dir itself is usually a junction into the
        // shared library (lifecycle creates it so). Plan the inode itself
        // and never enumerate through it: enumeration would misreport
        // library children as account targets and hide the real link.
        if paths::first_reparse_point(&base).as_deref() == Some(base.as_path()) {
            match skills::validate_skill_link(&base, &roots) {
                Ok(_) => targets.push(CleanupTarget {
                    path: base.clone(),
                    kind: CleanupTargetKind::SkillsLink,
                    owned: true,
                    size_bytes: std::fs::symlink_metadata(&base)
                        .map(|m| m.len())
                        .unwrap_or(0),
                    covered_by_backup: None,
                }),
                Err(error) => warnings.push(format!(
                    "Account skills entry '{}' does not validate ({}); left in place and apply will refuse it",
                    base.display(),
                    error
                )),
            }
            continue;
        }
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !seen.insert(norm_key(&path)) {
                continue;
            }
            match skills::validate_skill_link(&path, &roots) {
                Ok(_) => targets.push(CleanupTarget {
                    path: path.clone(),
                    kind: CleanupTargetKind::SkillsLink,
                    owned: true,
                    size_bytes: std::fs::symlink_metadata(&path)
                        .map(|m| m.len())
                        .unwrap_or(0),
                    covered_by_backup: None,
                }),
                Err(_) => warnings.push(format!(
                    "Unrecognized link '{}' is excluded from the plan and left in place",
                    path.display()
                )),
            }
        }
    }
    Ok(())
}

/// Plan a migration-linked structural skills junction under a canonical root
/// (`<root>/skills`). Validated links are protected structural entries: the
/// unlink pass and the tree walk preserve them in place and the prescan
/// expects them, so the shared target is never traversed. Anything else
/// reparse-like is left out so the prescan aborts on it (unknown links
/// abort).
fn collect_canonical_skill_link(
    dir: &Path,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) {
    let link = dir.join("skills");
    if std::fs::symlink_metadata(&link).is_err() {
        return;
    }
    if !paths::is_reparse_point(&link) {
        return;
    }
    match skills::validate_skill_link(&link, &shared_roots()) {
        Ok(_) => targets.push(CleanupTarget {
            path: link,
            kind: CleanupTargetKind::SkillsLink,
            owned: true,
            size_bytes: 0,
            covered_by_backup: None,
        }),
        Err(error) => warnings.push(format!(
            "Canonical skills entry '{}' does not validate ({}); left in place and apply will refuse it",
            link.display(),
            error
        )),
    }
}

fn collect_reset_targets(
    ctx: &AccountContext,
    targets: &mut Vec<CleanupTarget>,
    warnings: &mut Vec<String>,
) -> ManagerResult<()> {
    if !is_external_default(ctx) {
        warnings.push(
            "Canonical external data belongs to the external Default account and is not included for this account"
                .to_string(),
        );
        return Ok(());
    }
    match ctx.provider {
        ProviderId::Claude => {
            if let Some(cli) = claude_cli_data_dir() {
                if std::fs::symlink_metadata(&cli).is_ok() {
                    scan_legacy_leftovers(&cli, warnings);
                    let size = dir_size(&cli);
                    targets.push(CleanupTarget {
                        path: cli,
                        kind: CleanupTargetKind::Directory,
                        owned: true,
                        size_bytes: size,
                        covered_by_backup: None,
                    });
                } else {
                    warnings.push("Canonical CLI data absent; nothing to reset there".to_string());
                }
            }
            let layout = desktop_layout();
            if !layout.problems.is_empty() {
                return Err(format!(
                    "Refusing FullReset: unknown canonical alias (resolve it manually, then retry). No data was changed: {}",
                    layout.problems.join("; ")
                ));
            }
            if layout.physical.is_empty() {
                warnings.push("No known Desktop data directories found".to_string());
            }
            for (alias, physical) in &layout.aliases {
                warnings.push(format!(
                    "Canonical alias '{}' resolves to '{}'; clearing the physical target only, alias preserved",
                    alias.display(),
                    physical.display()
                ));
            }
            for dir in layout.physical {
                scan_legacy_leftovers(&dir, warnings);
                let size = dir_size(&dir);
                targets.push(CleanupTarget {
                    path: dir,
                    kind: CleanupTargetKind::Directory,
                    owned: true,
                    size_bytes: size,
                    covered_by_backup: None,
                });
            }
        }
        ProviderId::Codex => {
            if let Some(home) = codex_data_dir() {
                if std::fs::symlink_metadata(&home).is_ok() {
                    scan_legacy_leftovers(&home, warnings);
                    let size = dir_size(&home);
                    targets.push(CleanupTarget {
                        path: home,
                        kind: CleanupTargetKind::Directory,
                        owned: true,
                        size_bytes: size,
                        covered_by_backup: None,
                    });
                } else {
                    warnings
                        .push("Canonical Codex data absent; nothing to reset there".to_string());
                }
            }
        }
        _ => warnings.push(
            "No known canonical layout for this provider; only account-local data is covered"
                .to_string(),
        ),
    }
    // Migration-linked structural skills junctions under canonical roots.
    // The account-scoped enumeration never sees these (external Default has
    // no explicit dirs), so collect them here as protected structural links.
    {
        let clear = canonical_clear_roots();
        let canonical_dirs: Vec<PathBuf> = targets
            .iter()
            .filter(|t| {
                t.kind == CleanupTargetKind::Directory && clear.contains(&norm_key(&t.path))
            })
            .map(|t| t.path.clone())
            .collect();
        for dir in canonical_dirs {
            collect_canonical_skill_link(&dir, &mut *targets, &mut *warnings);
        }
    }

    warnings.push(
        "Canonical data directories are cleared in place (roots preserved so official apps still launch); canonical aliases themselves are never deleted"
            .to_string(),
    );

    if let Some(lib) = skills::shared_skills_dir() {
        warnings.push(format!(
            "Shared skills library '{}' is preserved",
            lib.display()
        ));
    }
    warnings.push("Other providers' data and runtimes are preserved".to_string());
    if let Some(backups) = paths::fork_backups_dir() {
        warnings.push(format!(
            "Existing encrypted snapshots under '{}' are preserved; purge them explicitly via backup purge",
            backups.display()
        ));
    }
    warnings.push(
        "Machine-wide and forensic traces are never touched (event logs, prefetch, system crash dumps, other users' data)".to_string(),
    );
    warnings.push(
        "Browser profiles are never wiped by reset; no DNS cache flush or service changes"
            .to_string(),
    );
    Ok(())
}

/// Defense in depth: the fork backups root and the shared skills library can
/// never be deletion targets, even if derivation ever overlaps them.
fn finalize_targets(targets: &mut Vec<CleanupTarget>, warnings: &mut Vec<String>) {
    let backups = paths::fork_backups_dir();
    let library = skills::shared_skills_dir();
    targets.retain(|t| {
        let protected = backups
            .as_ref()
            .is_some_and(|root| paths::is_within(root, &t.path))
            || library
                .as_ref()
                .is_some_and(|root| paths::is_within(root, &t.path));
        if protected {
            warnings.push(format!(
                "Protected path '{}' can never be a deletion target",
                t.path.display()
            ));
        }
        !protected
    });
    // Files covered by a whole-directory target need no separate entry.
    let dirs: Vec<PathBuf> = targets
        .iter()
        .filter(|t| t.kind == CleanupTargetKind::Directory)
        .map(|t| t.path.clone())
        .collect();
    // Files covered by a whole-directory target need no separate entry.
    // Validated skill links are kept: they are removed by inode first, and
    // the pre-scan expects them as planned targets.
    targets.retain(|t| {
        t.kind == CleanupTargetKind::Directory
            || t.kind == CleanupTargetKind::SkillsLink
            || !dirs
                .iter()
                .any(|d| paths::is_within(d, &t.path) && norm_key(d) != norm_key(&t.path))
    });
}

fn is_external_default(ctx: &AccountContext) -> bool {
    ctx.profile.id.eq_ignore_ascii_case("default") && ctx.ownership == AccountOwnership::External
}

/// Known Claude data markers. An adopted tree must carry at least one to
/// qualify for whole-directory deletion; marker-less adopted paths are left
/// in place and reported.
const CLI_DATA_MARKERS: &[&str] = &[
    ".credentials.json",
    "auth.json",
    ".claude.json",
    "projects",
    "todos",
    "sessions",
    "file-history",
    "history.json",
    "plugins",
];
const DESKTOP_DATA_MARKERS: &[&str] = &["config.json", "Local State", "Preferences"];

/// Validate a Managed adopted data tree for whole-directory deletion: the
/// exact registered path, absolute, not itself a link, not a filesystem
/// root, not containing a protected root (never a broad parent wipe or an
/// unregistered path), carrying at least one Claude data marker, and never
/// equal to canonical external data (wipe that only via an explicit
/// FullReset of the external Default account).
///
/// The marker-presence check alone is skipped when the durable registry
/// proves this exact path is the current Managed record's own root (fresh
/// no-open profiles have no markers yet; a partial failure may have removed
/// the auth marker while other owned data survived). Every other gate above
/// still applies, so an unregistered, drifted, or non-owned path can never
/// ride the exemption.
fn validate_adopted_tree(ctx: &AccountContext, path: &Path) -> ManagerResult<()> {
    if !path.is_absolute() {
        return Err("not an absolute path".to_string());
    }
    if paths::first_reparse_point(path).as_deref() == Some(path) {
        return Err("the adopted root itself is a link".to_string());
    }
    if path.parent().is_none() {
        return Err("refusing a filesystem root".to_string());
    }
    for protected in protected_roots() {
        if paths::is_within(path, &protected) {
            return Err(format!(
                "the tree contains the protected root '{}'",
                protected.display()
            ));
        }
    }
    let markers = if ctx.desktop_dir.as_deref() == Some(path) {
        DESKTOP_DATA_MARKERS
    } else {
        CLI_DATA_MARKERS
    };
    let has_marker = markers.iter().any(|marker| {
        let candidate = path.join(marker);
        std::fs::symlink_metadata(&candidate).is_ok()
            && paths::first_reparse_point(&candidate).as_deref() != Some(&candidate)
    });
    if !has_marker && !super::lifecycle::deletion_root_marker_exempt(ctx, path) {
        return Err("no Claude data markers found".to_string());
    }
    for canonical in canonical_data_roots() {
        if norm_key(&canonical) == norm_key(path) {
            return Err(format!(
                "overlaps canonical external data '{}'; wipe it only via an explicit FullReset of the external Default account",
                canonical.display()
            ));
        }
    }
    Ok(())
}

/// Roots a whole-directory delete must never swallow: user profile dirs, the
/// fork roots, the backups root, and the shared skills library. Canonical
/// external data roots are rejected separately with an explicit error in
/// [`validate_adopted_tree`] (wipe those only via FullReset on the external
/// Default account).
fn protected_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(crate::known_folders::home_dir());
    roots.extend(crate::known_folders::roaming_app_data_dir());
    roots.extend(crate::known_folders::local_app_data_dir());
    roots.extend(paths::fork_config_dir());
    roots.extend(paths::fork_local_dir());
    roots.extend(paths::profiles_root());
    roots.extend(paths::fork_backups_dir());
    roots.extend(skills::shared_skills_dir());
    roots
}

/// Canonical external data roots: CLI, Desktop physical layouts, Codex.
fn canonical_data_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    roots.extend(claude_cli_data_dir());
    roots.extend(claude_desktop_data_dirs());
    roots.extend(codex_data_dir());
    roots
}

fn shared_roots() -> Vec<PathBuf> {
    skills::shared_skills_dir().into_iter().collect()
}

fn dir_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if paths::is_reparse_point(&path) {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// Case-insensitive lexical key over both slash styles and Win32/NT prefixes,
/// mirroring the canonical comparison used by the path guards.
fn norm_key(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('/', "\\").to_lowercase();
    if let Some(rest) = text
        .strip_prefix("\\\\?\\unc\\")
        .or_else(|| text.strip_prefix("\\??\\unc\\"))
    {
        text = format!("\\\\{rest}");
    } else if let Some(rest) = text
        .strip_prefix("\\\\?\\")
        .or_else(|| text.strip_prefix("\\??\\"))
    {
        text = rest.to_string();
    }
    if text.len() > 3 {
        text.trim_end_matches('\\').to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Canonical external layouts (reset scope, external Default only)
// ---------------------------------------------------------------------------

/// Test-only override for the canonical base dirs (roaming, local, home).
/// Production always uses the real known folders. Thread-local so parallel
/// tests never race; the guard clears on drop.
#[cfg(test)]
thread_local! {
    static CANONICAL_TEST_BASES: std::cell::RefCell<Option<(PathBuf, PathBuf, PathBuf)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn canonical_test_guard(roaming: PathBuf, local: PathBuf, home: PathBuf) -> CanonicalTestGuard {
    CANONICAL_TEST_BASES.with(|c| *c.borrow_mut() = Some((roaming, local, home)));
    CanonicalTestGuard
}
#[cfg(test)]
struct CanonicalTestGuard;
#[cfg(test)]
impl Drop for CanonicalTestGuard {
    fn drop(&mut self) {
        CANONICAL_TEST_BASES.with(|c| *c.borrow_mut() = None);
    }
}

#[cfg(test)]
fn test_canonical_bases() -> Option<(PathBuf, PathBuf, PathBuf)> {
    CANONICAL_TEST_BASES.with(|c| c.borrow().clone())
}

/// Canonical base dirs: real known folders in production, thread-local test
/// fakes under cfg(test) when a fixture sets them.
fn canonical_base_dirs() -> (Option<PathBuf>, Option<PathBuf>, Option<PathBuf>) {
    #[cfg(test)]
    {
        if let Some(bases) = test_canonical_bases() {
            return (Some(bases.0), Some(bases.1), Some(bases.2));
        }
    }
    (
        crate::known_folders::roaming_app_data_dir(),
        crate::known_folders::local_app_data_dir(),
        crate::known_folders::home_dir(),
    )
}

/// How the canonical `%APPDATA%/Claude` entry resolved.
enum AliasResolve {
    /// Nothing there.
    Absent,
    /// An ordinary directory: use as-is.
    Ordinary,
    /// A known alias resolved exactly once to a validated physical data root.
    KnownAlias { target: PathBuf },
    /// An alias that cannot be validated; the reset aborts.
    Unknown(String),
}

/// Resolve the canonical Desktop entry once: ordinary dirs pass through,
/// missing entries are absent, and a link must resolve in exactly one hop to
/// an existing directory carrying the desktop identity file. Anything else
/// (dangling, nested, non-dir, no identity) is unknown and aborts the reset.
fn resolve_known_alias(alias: &Path) -> AliasResolve {
    if std::fs::symlink_metadata(alias).is_err() {
        return AliasResolve::Absent;
    }
    if !paths::is_reparse_point(alias) {
        return AliasResolve::Ordinary;
    }
    let raw = match paths::read_link_target(alias) {
        Ok(target) => target,
        Err(error) => {
            return AliasResolve::Unknown(format!(
                "cannot read canonical alias '{}': {error}",
                alias.display()
            ))
        }
    };
    let resolved = if raw.is_absolute() {
        raw
    } else {
        alias.parent().map(|p| p.join(&raw)).unwrap_or(raw)
    };
    if paths::is_reparse_point(&resolved) {
        return AliasResolve::Unknown(format!(
            "canonical alias '{}' resolves to another link (nested aliases unsupported)",
            alias.display()
        ));
    }
    if !resolved.is_dir() {
        return AliasResolve::Unknown(format!(
            "canonical alias '{}' points at '{}', which is not a directory",
            alias.display(),
            resolved.display()
        ));
    }
    if !resolved.join("config.json").exists() {
        return AliasResolve::Unknown(format!(
            "canonical alias '{}' points at '{}', which carries no desktop identity (config.json)",
            alias.display(),
            resolved.display()
        ));
    }
    AliasResolve::KnownAlias { target: resolved }
}

/// Resolved canonical Desktop layout: physical data dirs plus the known
/// aliases that point at them (each resolved exactly once). Unknown aliases
/// are collected as fatal problems for the caller to abort on.
struct DesktopLayout {
    physical: Vec<PathBuf>,
    aliases: Vec<(PathBuf, PathBuf)>,
    problems: Vec<String>,
}

fn desktop_layout() -> DesktopLayout {
    let (roaming, local, _) = canonical_base_dirs();
    let mut layout = DesktopLayout {
        physical: Vec::new(),
        aliases: Vec::new(),
        problems: Vec::new(),
    };
    if let Some(roaming) = roaming {
        let current = roaming.join("Claude");
        match resolve_known_alias(&current) {
            AliasResolve::Absent => {}
            AliasResolve::Ordinary => layout.physical.push(current),
            AliasResolve::KnownAlias { target } => {
                layout.aliases.push((current, target.clone()));
                layout.physical.push(target);
            }
            AliasResolve::Unknown(reason) => layout.problems.push(reason),
        }
    }
    if let Some(local) = local {
        for dir in publisher_dirs(&local.join("Claude-Data"), "claude_") {
            if paths::is_reparse_point(&dir) {
                layout.problems.push(format!(
                    "Unexpected link at known data dir '{}'; resolve it manually",
                    dir.display()
                ));
                continue;
            }
            layout.physical.push(dir);
        }
        for package in publisher_dirs(&local.join("Packages"), "Claude_") {
            if paths::is_reparse_point(&package) {
                layout.problems.push(format!(
                    "Unexpected link at package dir '{}'; resolve it manually",
                    package.display()
                ));
                continue;
            }
            let candidate = package.join("LocalCache").join("Roaming").join("Claude");
            if std::fs::symlink_metadata(&candidate).is_ok() {
                if paths::is_reparse_point(&candidate) {
                    layout.problems.push(format!(
                        "Unexpected link at known data dir '{}'; resolve it manually",
                        candidate.display()
                    ));
                    continue;
                }
                layout.physical.push(candidate);
            }
        }
    }
    // Dedupe: an alias physical root is usually also enumerated.
    let mut seen = BTreeSet::new();
    layout.physical.retain(|p| seen.insert(norm_key(p)));
    layout
}

/// Canonical roots cleared in place (contents removed, root preserved) rather
/// than removed: wiping the root itself would strand canonical aliases and
/// break official app launch.
fn canonical_clear_roots() -> BTreeSet<String> {
    canonical_data_roots()
        .into_iter()
        .map(|p| norm_key(&p))
        .collect()
}

/// Clear a canonical root's contents while preserving the root itself (a
/// canonical alias is never a target, so it is untouched). Any reparse point
/// inside aborts, as does a root swapped to a link mid-operation; the root
/// is recreated through the reparse-refusing guard if it went missing.
fn clear_contents_keep_root(path: &Path) -> ManagerResult<CleanupAction> {
    if paths::is_reparse_point(path) {
        return Err(format!(
            "Refusing '{}': canonical root became a link; resolve it manually then retry",
            path.display()
        ));
    }
    remove_tree_contents(path)?;
    if std::fs::symlink_metadata(path).is_err() {
        paths::ensure_dir(path).map_err(|error| {
            format!(
                "Unable to recreate canonical root '{}': {error}",
                path.display()
            )
        })?;
    }
    Ok(CleanupAction::RemovedDirectory)
}

fn claude_cli_data_dir() -> Option<PathBuf> {
    canonical_base_dirs().2.map(|home| home.join(".claude"))
}

fn codex_data_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some((_, _, home)) = test_canonical_bases() {
            return Some(home.join(".codex"));
        }
    }
    if let Some(dir) = crate::accounts::environment_directory(ProviderId::Codex) {
        return Some(dir);
    }
    canonical_base_dirs().2.map(|home| home.join(".codex"))
}

/// Known Desktop data layouts: the current `%APPDATA%/Claude` install, older
/// publisher installs under `%LOCALAPPDATA%/Claude-Data`, and MSIX package
/// installs. Mirrors the poller discovery without importing it.
fn claude_desktop_data_dirs() -> Vec<PathBuf> {
    desktop_layout().physical
}

fn publisher_dirs(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix(prefix).is_some_and(|tail| {
                    tail.len() == 13
                        && tail
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
            })
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
}

/// Report legacy plaintext backup generations found at the top level of a
/// canonical root. They are never acted on here; migration owns the explicit
/// encrypt/purge/leave choice.
fn scan_legacy_leftovers(root: &Path, warnings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains("backup") || name.ends_with(".bak") || name.ends_with(".backup") {
            warnings.push(format!(
                "Legacy backup '{}' left in place; choose encrypt/purge/leave via migration",
                entry.path().display()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountProfile;
    use tempfile::TempDir;

    fn managed_profile(id: &str, config_dir: &Path) -> AccountProfile {
        AccountProfile {
            id: id.into(),
            name: "Fixture".into(),
            config_dir: config_dir.to_string_lossy().into(),
            credentials_path: String::new(),
            enabled: true,
            desktop_dir: String::new(),
            ownership: AccountOwnership::Managed,
            ..AccountProfile::default()
        }
    }

    fn ctx_for(profile: &AccountProfile) -> AccountContext {
        AccountContext::from_profile(ProviderId::Claude, profile).unwrap()
    }

    #[test]
    fn scope_names_map_to_cli_spellings() {
        assert_eq!(
            scope_from_cli_name("cache").unwrap(),
            CleanupScope::AccountCache
        );
        assert_eq!(
            scope_from_cli_name("logout").unwrap(),
            CleanupScope::AccountLogout
        );
        assert_eq!(
            scope_from_cli_name("delete").unwrap(),
            CleanupScope::AccountLocalData
        );
        assert_eq!(
            scope_from_cli_name("reset").unwrap(),
            CleanupScope::FullReset
        );
        assert!(scope_from_cli_name("wipe").is_err());
    }

    #[test]
    fn cache_preview_lists_only_regenerable_caches() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("config");
        for dir in ["Cache", "Code Cache", "GPUCache"] {
            std::fs::create_dir_all(config.join(dir)).unwrap();
        }
        std::fs::create_dir_all(config.join("LocalStorage")).unwrap();
        std::fs::write(config.join("history.json"), "{}").unwrap();
        let ctx = ctx_for(&managed_profile("account_9", &config));
        let plan = preview_cleanup(&ctx, CleanupScope::AccountCache).unwrap();
        assert_eq!(plan.targets.len(), 3);
        assert!(plan
            .targets
            .iter()
            .all(|t| t.kind == CleanupTargetKind::Cache && t.owned));
        assert!(plan.backup_id.is_none());
        // Cache-only plans are destructive-ready by design (regenerable data
        // needs no snapshot); readiness itself is not a cache boundary.
        assert!(plan
            .targets
            .iter()
            .all(|t| !norm_key(&t.path).contains("localstorage")
                && !norm_key(&t.path).contains("history")));
    }

    #[test]
    fn logout_keeps_config_history_and_skill_links() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("config");
        std::fs::create_dir_all(config.join("skills")).unwrap();
        std::fs::write(config.join(".credentials.json"), "{}").unwrap();
        std::fs::write(config.join("history.json"), "{}").unwrap();
        let ctx = ctx_for(&managed_profile("account_9", &config));
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLogout).unwrap();
        assert!(plan.targets.iter().all(|t| matches!(
            t.kind,
            CleanupTargetKind::Auth | CleanupTargetKind::CredentialTarget
        )));
        assert!(plan
            .targets
            .iter()
            .any(|t| t.kind == CleanupTargetKind::Auth));
        assert!(plan.backup_id.is_none());
    }

    #[test]
    fn unvalidated_skill_links_are_never_owned() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("config");
        // A regular file where a validated account link would live: it can
        // never validate, so it must never become an owned target. The
        // realistic CredentialFile layout (`.credentials.json` marker) keeps
        // the adopted root itself valid so the policy under test is reached.
        std::fs::create_dir_all(config.join("skills")).unwrap();
        std::fs::write(config.join(".credentials.json"), "{}").unwrap();
        std::fs::write(config.join("skills").join("foreign"), "x").unwrap();
        let ctx = ctx_for(&managed_profile("account_9", &config));
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        assert!(plan
            .targets
            .iter()
            .all(|t| t.kind != CleanupTargetKind::SkillsLink));
        assert!(plan.warnings.iter().any(|w| w.contains("foreign")));
        assert!(skills::remove_link_only(&config.join("skills").join("foreign")).is_err());
    }

    #[test]
    fn full_reset_for_managed_never_targets_backups_or_canonical() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let ctx = ctx_for(&managed_profile("account_9", &config));
        let plan = preview_cleanup(&ctx, CleanupScope::FullReset).unwrap();
        if let Some(backups) = paths::fork_backups_dir() {
            assert!(plan
                .targets
                .iter()
                .all(|t| !paths::is_within(&backups, &t.path)));
        }
        if let Some(home_claude) = claude_cli_data_dir() {
            assert!(plan.targets.iter().all(|t| {
                norm_key(&t.path) != norm_key(&home_claude)
                    && !paths::is_within(&home_claude, &t.path)
            }));
        }
        assert!(plan.warnings.iter().any(|w| w.contains("external Default")));
    }

    #[test]
    fn managed_adopted_tree_outside_roots_is_wholly_targeted() {
        let root = TempDir::new().unwrap();
        // An adopted Second-style tree: outside the fork profiles root,
        // carrying real Claude markers, with an untouched sibling next door.
        let adopted = root.path().join("second-data");
        std::fs::create_dir_all(adopted.join("projects")).unwrap();
        std::fs::write(adopted.join(".credentials.json"), "{}").unwrap();
        let sibling = root.path().join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), "x").unwrap();
        if let Some(profiles) = paths::profiles_root() {
            assert!(!paths::is_within(&profiles, &adopted));
        }
        let ctx = ctx_for(&managed_profile("account_7", &adopted));
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        // The entire adopted tree is one owned Directory target ...
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::Directory
                && t.owned
                && norm_key(&t.path) == norm_key(&adopted)
        }));
        // ... while the sibling and every shared root stay untargeted.
        assert!(plan.targets.iter().all(|t| {
            !paths::is_within(&sibling, &t.path) && norm_key(&t.path) != norm_key(&sibling)
        }));
        if let Some(lib) = skills::shared_skills_dir() {
            assert!(plan
                .targets
                .iter()
                .all(|t| !paths::is_within(&lib, &t.path)));
        }
        assert!(!plan.warnings.iter().any(|w| w.contains("left in place")));
    }

    #[test]
    fn fresh_managed_desktop_without_markers_backs_up_and_deletes() {
        use crate::manager::{native, state};
        // Hermetic process table for the quiesce probe: no real processes.
        native::set_test_snapshot(Vec::new());
        native::allow_test_signals();
        // Fresh no-open Desktop profile: the exact managed narrow root with
        // no `config.json` / `Local State` markers yet. It is ours by
        // construction, so preview must plan it without marker evidence.
        let id = "account_22";
        let managed = paths::managed_account_dir(ProviderId::Claude, id).unwrap();
        std::fs::create_dir_all(&managed).unwrap();
        std::fs::write(managed.join("first-run.txt"), "fresh").unwrap();
        let sibling = managed.parent().unwrap().join("sibling-keep-22");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), "keep").unwrap();
        state::mutate_settings(|settings| {
            if settings.accounts.claude.find(id).is_none() {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: id.into(),
                    name: "FreshDesktop".into(),
                    desktop_dir: managed.to_string_lossy().into(),
                    ownership: AccountOwnership::Managed,
                    ..AccountProfile::default()
                });
            }
            settings.accounts.claude.used_ids.insert(id.into());
            Ok::<_, String>(())
        })
        .unwrap();
        let ctx = AccountContext::from_profile(
            ProviderId::Claude,
            &state::load_fresh()
                .accounts
                .claude
                .find(id)
                .unwrap()
                .clone(),
        )
        .unwrap();
        // The exact managed Desktop root is the deletable root even with no
        // markers on disk.
        assert_eq!(
            crate::manager::lifecycle::owned_deletion_root(&ctx).unwrap(),
            Some(managed.clone())
        );
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        assert!(!plan.warnings.iter().any(|w| w.contains("left in place")));
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::Directory
                && t.owned
                && norm_key(&t.path) == norm_key(&managed)
        }));
        // An unrelated sibling under the same profiles parent is never a target.
        assert!(plan.targets.iter().all(|t| {
            !paths::is_within(&sibling, &t.path) && norm_key(&t.path) != norm_key(&sibling)
        }));
        let report = apply_cleanup(&plan).unwrap();
        assert!(report.success);
        let backup_id = report.backup_id.clone().expect("snapshot required");
        assert!(!managed.exists(), "fresh root deleted");
        assert!(sibling.join("keep.txt").exists(), "sibling preserved");
        assert!(state::load_fresh().accounts.claude.find(id).is_none());
        assert!(state::load_fresh().accounts.claude.used_ids.contains(id));
        // The verified snapshot covers the fresh root: undo restores it
        // byte-for-byte, then the fixture root is removed.
        crate::manager::backup::restore_backup(&backup_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(managed.join("first-run.txt")).unwrap(),
            "fresh"
        );
        std::fs::remove_dir_all(&managed).ok();
        std::fs::remove_dir_all(&sibling).ok();
    }

    #[test]
    fn adopted_desktop_partial_retry_stays_deletable_and_spares_sibling() {
        use crate::manager::{native, state};
        // Hermetic process table for the quiesce probe: no real processes.
        native::set_test_snapshot(Vec::new());
        native::allow_test_signals();
        let root = TempDir::new().unwrap();
        let adopted = root.path().join("desktop-data");
        std::fs::create_dir_all(adopted.join("projects")).unwrap();
        std::fs::write(
            adopted.join("config.json"),
            r#"{"oauth:tokenCacheV2":"SECRET","theme":"dark"}"#,
        )
        .unwrap();
        let lib = skills::ensure_shared_skills_dir().unwrap();
        let target = lib.join("fixture_desktop_retry");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("skill.md"), "shared").unwrap();
        let link = adopted.join("skills");
        mklink_junction(&link, &target);
        let sibling = root.path().join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("keep.txt"), "keep").unwrap();
        let id = "account_23";
        state::mutate_settings(|settings| {
            if settings.accounts.claude.find(id).is_none() {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: id.into(),
                    name: "DesktopSecond".into(),
                    desktop_dir: adopted.to_string_lossy().into(),
                    ownership: AccountOwnership::Managed,
                    ..AccountProfile::default()
                });
            }
            settings.accounts.claude.used_ids.insert(id.into());
            Ok::<_, String>(())
        })
        .unwrap();
        let ctx = AccountContext::from_profile(
            ProviderId::Claude,
            &state::load_fresh()
                .accounts
                .claude
                .find(id)
                .unwrap()
                .clone(),
        )
        .unwrap();
        // An exclusively held file forces a partial failure mid-delete while
        // the rest of the tree (including the Desktop auth marker) is
        // removed: the retry below must stay deletable without marker files.
        let stuck = adopted.join("projects").join("locked.txt");
        std::fs::write(&stuck, "x").unwrap();
        #[cfg(windows)]
        let _held = hold_deny_delete(&stuck);
        #[cfg(not(windows))]
        let _held = {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(adopted.join("projects"))
                .unwrap()
                .permissions();
            perms.set_mode(0o555);
            std::fs::set_permissions(adopted.join("projects"), perms).unwrap();
            adopted.join("projects")
        };
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        let report = apply_cleanup(&plan).unwrap();
        assert!(!report.success);
        assert!(report.results.iter().any(|r| !r.ok));
        assert!(
            target.join("skill.md").exists(),
            "shared library target must survive"
        );
        assert!(sibling.join("keep.txt").exists(), "sibling must survive");
        assert!(state::load_fresh().accounts.claude.find(id).is_some());
        // Retry: release the lock, re-preview, apply → full success. The
        // first attempt removed the auth marker, so this only passes because
        // the durable registry (not a marker file) proves the surviving root
        // is still ours.
        #[cfg(windows)]
        drop(_held);
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(adopted.join("projects"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(adopted.join("projects"), perms).unwrap();
        }
        let plan2 = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        let report2 = apply_cleanup(&plan2).unwrap();
        assert!(report2.success);
        assert!(report2.backup_id.is_some());
        assert!(!adopted.exists());
        assert!(target.join("skill.md").exists());
        assert!(sibling.join("keep.txt").exists());
        assert!(state::load_fresh().accounts.claude.find(id).is_none());
        assert!(state::load_fresh().accounts.claude.used_ids.contains(id));
    }

    #[test]
    fn registry_removal_drops_profile_but_keeps_used_ids() {
        use crate::manager::state;
        let id = "account_9";
        state::mutate_settings(|settings| {
            if settings.accounts.claude.find(id).is_none() {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: id.into(),
                    name: "Doomed".into(),
                    ownership: AccountOwnership::Managed,
                    ..AccountProfile::default()
                });
            }
            settings.accounts.claude.used_ids.insert(id.into());
            Ok::<_, String>(())
        })
        .unwrap();
        remove_profile_from_registry(ProviderId::Claude, id).unwrap();
        let fresh = state::load_fresh();
        assert!(fresh.accounts.claude.find(id).is_none());
        assert!(fresh.accounts.claude.used_ids.contains(id));
    }

    fn mklink_junction(link: &Path, target: &Path) {
        let link_s = link.to_string_lossy().into_owned();
        let target_s = target.to_string_lossy().into_owned();
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "mklink", "/J", link_s.as_str(), target_s.as_str()]);
        let output = crate::manager::process::run_bounded(
            &mut command,
            std::time::Duration::from_secs(10),
            64 * 1024,
            "test.mklink",
        )
        .expect("failed to spawn mklink");
        assert!(
            output.status.success(),
            "mklink /J must succeed in the test env"
        );
    }

    #[cfg(windows)]
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const std::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
    }

    #[cfg(windows)]
    const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

    /// Hold `path` open with read+write sharing but WITHOUT delete sharing.
    /// Reads (snapshot) still succeed while `remove_file`/`remove_dir` fail
    /// with a sharing violation, deterministically modeling an app-held lock.
    #[cfg(windows)]
    fn hold_deny_delete(path: &Path) -> std::fs::File {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // GENERIC_READ, FILE_SHARE_READ|FILE_SHARE_WRITE, OPEN_EXISTING.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x8000_0000,
                3,
                std::ptr::null(),
                3,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(
            handle != INVALID_HANDLE_VALUE,
            "deny-delete holder must open '{}'",
            path.display()
        );
        unsafe { std::fs::File::from_raw_handle(handle) }
    }

    #[test]
    fn desktop_logout_redacts_only_token_keys() {
        let root = TempDir::new().unwrap();
        let desktop = root.path().join("desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        let config = desktop.join("config.json");
        std::fs::write(
            &config,
            r#"{"oauth:tokenCacheV2":"SECRET1","oauth:tokenCache":"SECRET2","lastKnownAccountUuid":"UUID-1","theme":"dark","mcpServers":{"x":{}}}"#,
        )
        .unwrap();
        std::fs::write(desktop.join("Local State"), "{}").unwrap();
        let action = scrub_desktop_auth_keys(&config).unwrap();
        assert_eq!(action, CleanupAction::Scrubbed);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(
            after["oauth:tokenCacheV2"],
            serde_json::Value::String(String::new())
        );
        assert_eq!(
            after["oauth:tokenCache"],
            serde_json::Value::String(String::new())
        );
        assert_eq!(
            after["lastKnownAccountUuid"],
            serde_json::Value::String("UUID-1".to_string())
        );
        assert_eq!(after["theme"].as_str(), Some("dark"));
        assert!(after["mcpServers"]["x"].is_object());
        // Idempotent: a second scrub is a no-op success.
        assert_eq!(
            scrub_desktop_auth_keys(&config).unwrap(),
            CleanupAction::NoopMissing
        );
    }

    #[test]
    fn desktop_logout_clears_cookies_and_redacts_config() {
        let root = TempDir::new().unwrap();
        let desktop = root.path().join("desktop-app");
        std::fs::create_dir_all(desktop.join("Network")).unwrap();
        std::fs::write(desktop.join("Network").join("Cookies"), "cookie-bytes").unwrap();
        std::fs::write(desktop.join("Network").join("Cookies-journal"), "journal").unwrap();
        std::fs::write(desktop.join("Network").join("TransportSecurity"), "{}").unwrap();
        std::fs::write(
            desktop.join("config.json"),
            r#"{"oauth:tokenCacheV2":"S","lastKnownAccountUuid":"U","theme":"dark"}"#,
        )
        .unwrap();
        std::fs::write(desktop.join("Local State"), "{}").unwrap();
        let profile = AccountProfile {
            id: "account_5".into(),
            name: "Desktop".into(),
            desktop_dir: desktop.to_string_lossy().into(),
            ownership: AccountOwnership::Managed,
            ..AccountProfile::default()
        };
        let ctx = AccountContext::from_profile(ProviderId::Claude, &profile).unwrap();
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLogout).unwrap();
        let auth: Vec<String> = plan
            .targets
            .iter()
            .filter(|t| t.kind == CleanupTargetKind::Auth)
            .map(|t| norm_key(&t.path))
            .collect();
        assert!(auth.iter().any(|p| p.ends_with("network\\cookies")));
        assert!(auth.iter().any(|p| p.ends_with("network\\cookies-journal")));
        assert!(auth.iter().any(|p| p.ends_with("config.json")));
        assert!(plan.targets.iter().all(|t| {
            let key = norm_key(&t.path);
            !key.ends_with("transportsecurity") && !key.ends_with("local state")
        }));
        assert!(plan.warnings.iter().any(|w| w.contains("redact")));
        assert!(plan.backup_id.is_none());
    }

    #[test]
    fn account_skills_junction_is_planned_by_inode() {
        let root = TempDir::new().unwrap();
        let config = root.path().join("adopted");
        std::fs::create_dir_all(config.join("projects")).unwrap();
        std::fs::write(config.join(".credentials.json"), "{}").unwrap();
        let lib = skills::ensure_shared_skills_dir().unwrap();
        let target = lib.join("fixture_skill");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("skill.md"), "shared").unwrap();
        let link = config.join("skills");
        mklink_junction(&link, &target);
        let ctx = ctx_for(&managed_profile("account_7", &config));
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::SkillsLink
                && t.owned
                && norm_key(&t.path) == norm_key(&link)
        }));
        prescan_no_surprise_links(&plan.targets).unwrap();
        // The shared library target itself is never planned.
        assert!(plan
            .targets
            .iter()
            .all(|t| !paths::is_within(&target, &t.path)));
    }

    #[test]
    fn apply_preserves_junction_target_and_retries_partial_failure() {
        use crate::manager::{native, state};
        // Hermetic process table for the quiesce probe: no real processes.
        native::set_test_snapshot(Vec::new());
        native::allow_test_signals();
        let root = TempDir::new().unwrap();
        let adopted = root.path().join("second-data");
        std::fs::create_dir_all(adopted.join("projects")).unwrap();
        std::fs::write(adopted.join(".credentials.json"), "{}").unwrap();
        let lib = skills::ensure_shared_skills_dir().unwrap();
        let target = lib.join("fixture_preserve");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("skill.md"), "shared").unwrap();
        let link = adopted.join("skills");
        mklink_junction(&link, &target);
        let id = "account_6";
        state::mutate_settings(|settings| {
            if settings.accounts.claude.find(id).is_none() {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: id.into(),
                    name: "Second".into(),
                    config_dir: adopted.to_string_lossy().into(),
                    ownership: AccountOwnership::Managed,
                    ..AccountProfile::default()
                });
            }
            settings.accounts.claude.used_ids.insert(id.into());
            Ok::<_, String>(())
        })
        .unwrap();
        let ctx = AccountContext::from_profile(
            ProviderId::Claude,
            &state::load_fresh()
                .accounts
                .claude
                .find(id)
                .unwrap()
                .clone(),
        )
        .unwrap();
        // Attempt 1: an exclusively held file forces a partial failure
        // mid-delete. Readonly is not a lock on Windows (delete succeeds),
        // so hold the file with read/write sharing but no delete sharing:
        // the snapshot can still read it while deletion fails with a
        // sharing violation and the first-error/retry journal is exercised.
        let stuck = adopted.join("projects").join("locked.txt");
        std::fs::write(&stuck, "x").unwrap();
        #[cfg(windows)]
        let _held = hold_deny_delete(&stuck);
        #[cfg(not(windows))]
        let _held = {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(adopted.join("projects"))
                .unwrap()
                .permissions();
            perms.set_mode(0o555);
            std::fs::set_permissions(adopted.join("projects"), perms).unwrap();
            // Token dropped on retry; restores write below.
            adopted.join("projects")
        };
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        assert!(plan
            .targets
            .iter()
            .any(|t| t.kind == CleanupTargetKind::SkillsLink));
        let report = apply_cleanup(&plan).unwrap();
        assert!(!report.success);
        assert!(report.results.iter().any(|r| !r.ok));
        assert!(
            target.join("skill.md").exists(),
            "shared library target must survive"
        );
        assert!(state::load_fresh().accounts.claude.find(id).is_some());
        // Retry: release the lock, re-preview, apply → full success.
        #[cfg(windows)]
        drop(_held);
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(adopted.join("projects"))
                .unwrap()
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(adopted.join("projects"), perms).unwrap();
        }
        let plan2 = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        let report2 = apply_cleanup(&plan2).unwrap();
        assert!(report2.success);
        assert!(report2.backup_id.is_some());
        assert!(!adopted.exists());
        assert!(target.join("skill.md").exists());
        assert!(state::load_fresh().accounts.claude.find(id).is_none());
        assert!(state::load_fresh().accounts.claude.used_ids.contains(id));
    }

    #[test]
    fn fake_default_alias_reset_preserves_structure_and_restores() {
        use crate::manager::{native, state};
        // Hermetic process table for the quiesce probe: no real processes.
        native::set_test_snapshot(Vec::new());
        native::allow_test_signals();
        let root = TempDir::new().unwrap();
        let fake_roaming = root.path().join("roaming");
        let fake_local = root.path().join("local");
        let fake_home = root.path().join("home");
        let physical = fake_local.join("Claude-Data").join("claude_pzs8sxrjxfjjc");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(
            physical.join("config.json"),
            r#"{"oauth:tokenCacheV2":"SECRET","lastKnownAccountUuid":"UUID-9","theme":"dark"}"#,
        )
        .unwrap();
        std::fs::write(physical.join("history.json"), "{}").unwrap();
        let alias = fake_roaming.join("Claude");
        std::fs::create_dir_all(&fake_roaming).unwrap();
        mklink_junction(&alias, &physical);
        let lib = skills::ensure_shared_skills_dir().unwrap();
        std::fs::create_dir_all(lib.join("keep")).unwrap();
        std::fs::write(lib.join("keep").join("s.md"), "x").unwrap();
        let other = root.path().join("other-account");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("data.json"), "{}").unwrap();

        // Migration-linked structural skills junctions (post-cutover layout):
        // one under the canonical CLI home, one under the physical Desktop root.
        let cli_home = fake_home.join(".claude");
        std::fs::create_dir_all(&cli_home).unwrap();
        std::fs::write(cli_home.join("history.json"), "{}").unwrap();
        std::fs::write(cli_home.join("config.json"), r#"{"theme":"dark"}"#).unwrap();
        mklink_junction(&cli_home.join("skills"), &lib.join("keep"));
        mklink_junction(&physical.join("skills"), &lib.join("keep"));
        let _guard = canonical_test_guard(fake_roaming, fake_local, fake_home);
        state::mutate_settings(|settings| {
            if let Some(p) = settings.accounts.claude.find_mut("default") {
                p.ownership = AccountOwnership::External;
                p.config_dir.clear();
                p.credentials_path.clear();
                p.desktop_dir.clear();
                p.credential_targets.clear();
            } else {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: "default".into(),
                    name: "Default".into(),
                    ownership: AccountOwnership::External,
                    ..AccountProfile::default()
                });
            }
            Ok::<_, String>(())
        })
        .unwrap();
        let profile = state::load_fresh()
            .accounts
            .claude
            .find("default")
            .unwrap()
            .clone();
        let ctx = AccountContext::from_profile(ProviderId::Claude, &profile).unwrap();
        let plan = preview_cleanup(&ctx, CleanupScope::FullReset).unwrap();
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::Directory && norm_key(&t.path) == norm_key(&physical)
        }));
        assert!(plan.warnings.iter().any(|w| w.contains("alias")));

        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::SkillsLink
                && t.owned
                && norm_key(&t.path) == norm_key(&cli_home.join("skills"))
        }));
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::SkillsLink
                && t.owned
                && norm_key(&t.path) == norm_key(&physical.join("skills"))
        }));
        let report = apply_cleanup(&plan).unwrap();
        assert!(report.success);
        assert!(report.backup_id.is_some());
        assert!(
            !physical.join("config.json").exists(),
            "auth must be cleared"
        );
        assert!(
            !physical.join("history.json").exists(),
            "history must be cleared"
        );
        assert!(physical.is_dir(), "structural root preserved");
        let remaining: Vec<PathBuf> = physical
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(
            remaining,
            vec![physical.join("skills")],
            "only the structural link remains"
        );
        assert!(paths::is_reparse_point(&alias), "alias junction preserved");

        assert!(
            paths::is_reparse_point(&cli_home.join("skills")),
            "cli skills junction preserved"
        );
        assert!(
            paths::is_reparse_point(&physical.join("skills")),
            "desktop skills junction preserved"
        );
        let cli_remaining: Vec<PathBuf> = cli_home
            .read_dir()
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(cli_remaining, vec![cli_home.join("skills")]);
        assert!(lib.join("keep").join("s.md").exists());
        assert!(other.join("data.json").exists());
        assert!(
            state::load_fresh()
                .accounts
                .claude
                .find("default")
                .is_some(),
            "external entry preserved"
        );

        // Undo: the verified snapshot restores auth and history byte-for-byte,
        // including under the preserved structural links.
        let backup_id = report.backup_id.clone().expect("snapshot required");
        crate::manager::backup::restore_backup(&backup_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(physical.join("config.json")).unwrap(),
            r#"{"oauth:tokenCacheV2":"SECRET","lastKnownAccountUuid":"UUID-9","theme":"dark"}"#
        );
        assert!(physical.join("history.json").exists());
        assert!(cli_home.join("history.json").exists());
        assert!(paths::is_reparse_point(&physical.join("skills")));
        assert!(paths::is_reparse_point(&cli_home.join("skills")));
    }

    #[test]
    fn managed_delete_removes_owned_launchers_last_and_restores() {
        use crate::manager::state;
        let root = TempDir::new().unwrap();
        let id = "account_8";
        // Adopted data so the delete has a Directory target too.
        let adopted = root.path().join("adopted-8");
        std::fs::create_dir_all(adopted.join("projects")).unwrap();
        std::fs::write(adopted.join(".credentials.json"), "{}").unwrap();
        // Owned launcher artifacts at the exact lifecycle spellings, under
        // the fixture shortcuts/config roots (never the real Start Menu).
        let shortcuts = crate::manager::native::shortcuts_root().unwrap();
        std::fs::create_dir_all(&shortcuts).unwrap();
        let lnk = shortcuts.join(format!("UsageHub Claude - {id}.lnk"));
        std::fs::write(&lnk, "fake-lnk-bytes").unwrap();
        let icons = crate::app_settings::app_data_directory().join("icons");
        std::fs::create_dir_all(&icons).unwrap();
        let ico = icons.join(format!("claude-{id}-ff0000.ico"));
        std::fs::write(&ico, "fake-ico-bytes").unwrap();
        state::mutate_settings(|settings| {
            if settings.accounts.claude.find(id).is_none() {
                settings.accounts.claude.profiles.push(AccountProfile {
                    id: id.into(),
                    name: "Launcher".into(),
                    config_dir: adopted.to_string_lossy().into(),
                    color: "#ff0000".into(),
                    ownership: AccountOwnership::Managed,
                    ..AccountProfile::default()
                });
            }
            settings.accounts.claude.used_ids.insert(id.into());
            Ok::<_, String>(())
        })
        .unwrap();
        let ctx = AccountContext::from_profile(
            ProviderId::Claude,
            &state::load_fresh()
                .accounts
                .claude
                .find(id)
                .unwrap()
                .clone(),
        )
        .unwrap();
        crate::manager::native::set_test_snapshot(Vec::new());
        crate::manager::native::allow_test_signals();
        let plan = preview_cleanup(&ctx, CleanupScope::AccountLocalData).unwrap();
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::Config && norm_key(&t.path) == norm_key(&lnk)
        }));
        assert!(plan.targets.iter().any(|t| {
            t.kind == CleanupTargetKind::Config && norm_key(&t.path) == norm_key(&ico)
        }));
        let report = apply_cleanup(&plan).unwrap();
        assert!(report.success);
        assert!(report.backup_id.is_some());
        assert!(!lnk.exists(), "owned shortcut removed");
        assert!(!ico.exists(), "owned icon removed");
        assert!(!adopted.exists());
        assert!(state::load_fresh().accounts.claude.find(id).is_none());
        assert!(state::load_fresh().accounts.claude.used_ids.contains(id));
        // Undo restores the exact bytes.
        let backup_id = report.backup_id.clone().expect("snapshot required");
        crate::manager::backup::restore_backup(&backup_id).unwrap();
        assert_eq!(std::fs::read(&lnk).unwrap(), b"fake-lnk-bytes");
        assert_eq!(std::fs::read(&ico).unwrap(), b"fake-ico-bytes");
    }
}
