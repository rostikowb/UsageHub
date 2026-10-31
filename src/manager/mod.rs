//! Multi-account manager: module boundaries and shared core types.
//!
//! `SettingsFile.accounts` is the single source of truth; there is no
//! divergent registry. Every mutation goes through [`state`] (named-mutex
//! serialized, fresh load, mutate, atomic save). Background operations must
//! keep the UI responsive (worker thread) and reload settings on completion.
//!
//! Module boundaries:
//! - [`paths`]: fork data roots, new/adopted/external path validation,
//!   junction/symlink guards. Implemented here.
//! - [`state`]: serialized settings access. Implemented here.
//! - [`skills`]: shared skills-library helpers (validated link targets,
//!   no-traverse link removal). Implemented here.
//! - `lifecycle` (lifecycle slice): create/import/rename/open/focus/stop/status/
//!   CLI env for accounts.
//! - `native` (lifecycle slice): per-account native launch/focus/process helpers.
//! - `credentials` (backup slice): confirmed owned Windows Credential Manager entries.
//! - `backup` (backup slice): encrypted 7-day undo snapshots.
//! - `cleanup` (cleanup slice): scoped preview/apply deletion.
//! - `widget` (widget slice): dynamic account rows for the widget theme engine.
//! - `cli` (integration slice): `--manage` noninteractive JSON command surface.
//! - `migration` (integration slice): legacy Second adoption and old-tool cutover.
//!
//! Authoritative usage rule (user override): exact limits AND resets come from
//! the original usageBar server poller ONLY. No switcher estimates,
//! local-history inference, weekly corrections, or manual reset overrides are
//! permitted in any authoritative usage path.

pub mod backup;
pub mod cleanup;
pub mod cli;
pub mod credentials;
pub mod discovery;
pub mod lifecycle;
pub mod migration;
pub mod native;
pub mod omp_auth;
pub mod paths;
pub mod process;
pub mod skills;
pub mod state;
pub mod widget;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountOwnership, AccountProfile, SourceKind};
use crate::providers::ProviderId;

/// Manager operations report errors as plain strings, matching the rest of
/// the codebase (`Result<_, String>`).
pub type ManagerResult<T> = Result<T, String>;

/// Seven-day encrypted undo retention for account deletion snapshots.
pub const BACKUP_RETENTION_SECS: u64 = 7 * 24 * 3600;

/// Fully resolved view of one account. Built from [`AccountProfile`]; the
/// poller keeps owning credential discovery and server reads.
#[derive(Clone, Debug)]
pub struct AccountContext {
    pub provider: ProviderId,
    pub profile: AccountProfile,
    pub source: SourceKind,
    /// Resolved explicit CLI config dir, if configured.
    pub config_dir: Option<PathBuf>,
    /// Resolved explicit Desktop user-data-dir, if configured.
    pub desktop_dir: Option<PathBuf>,
    /// Resolved credential file, if the source kind needs one.
    /// `None` means provider-default discovery (owned by the poller).
    pub credential_file: Option<PathBuf>,
    pub ownership: AccountOwnership,
}

impl AccountContext {
    /// Resolve explicit paths from a stored profile. Returns an error for
    /// invalid relative paths, `desktop_dir` on non-Claude providers, or
    /// unsupported providers; discovery fallbacks (`codex_auth_path`,
    /// Desktop login, WSL) stay in the poller.
    ///
    /// `SourceKind::ProviderDefault` yields `credential_file: None` (owned by
    /// the poller per the contract). `SourceKind::CredentialFile` requires an
    /// explicit file or config dir; a source-less non-default profile is
    /// rejected here with the profile id so lifecycle/CLI can report it.
    pub fn from_profile(provider: ProviderId, profile: &AccountProfile) -> ManagerResult<Self> {
        if !profile.desktop_dir.trim().is_empty() && provider != ProviderId::Claude {
            return Err(format!(
                "desktop_dir is Claude-only; account '{}' sets it for {provider:?}",
                profile.id
            ));
        }
        let source = profile.source_kind(provider);
        let credential_file = match source {
            // DesktopDir reads config.json + Local State directly and
            // ProviderDefault is resolved by the poller; neither yields a
            // credential file here.
            SourceKind::DesktopDir | SourceKind::ProviderDefault => None,
            SourceKind::CredentialFile => Some(
                profile
                    .credential_path(provider)?
                    .ok_or_else(|| format!("Account '{}' has no credential source", profile.id))?,
            ),
        };
        Ok(Self {
            provider,
            profile: profile.clone(),
            source,
            config_dir: profile.config_dir_resolved()?,
            desktop_dir: profile.desktop_dir_resolved()?,
            credential_file,
            ownership: profile.ownership,
        })
    }

    pub fn is_managed(&self) -> bool {
        self.ownership == AccountOwnership::Managed
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.profile.name, self.profile.id)
    }
}

/// Metadata for one encrypted deletion-undo snapshot. The live backup bytes
/// and manifest are owned by `backup.rs`; this record is what list/verify/
/// restore/purge exchange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupInfo {
    pub backup_id: String,
    pub provider: ProviderId,
    pub account_id: String,
    pub created_unix: u64,
    /// `created_unix + BACKUP_RETENTION_SECS`. Purge only expires past this.
    pub expires_unix: u64,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub manifest_hash: String,
    /// Credential targets included; only confirmed owned targets are stored.
    #[serde(default)]
    pub credential_targets: Vec<String>,
}

impl BackupInfo {
    pub fn expiry_for(created_unix: u64) -> u64 {
        created_unix.saturating_add(BACKUP_RETENTION_SECS)
    }

    pub fn is_expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_unix
    }
}

/// Known user-data scopes for destructive operations. Narrowest first; the UI
/// must select explicitly and external/Default data is opt-in only.
/// `AccountLogout` (sign out: auth material only) is distinct from
/// `AccountLocalData` (logout plus config/history deletion).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupScope {
    /// Regenerable caches only (proven browser-style caches, never
    /// LocalStorage/IndexedDB/history).
    AccountCache,
    /// Sign out: credential files, confirmed owned credential targets, and
    /// session/token state. Config, history, and skills are kept.
    AccountLogout,
    /// Logout plus account-local config/history deletion, covered by a
    /// verified backup.
    AccountLocalData,
    /// Every known user-owned Claude data source for the selected account.
    FullReset,
}

/// What a single cleanup target is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupTargetKind {
    Cache,
    Auth,
    Config,
    History,
    SkillsLink,
    CredentialTarget,
    Directory,
}

/// One path proposed for deletion. Destructive apply requires `owned` plus a
/// verified covering backup (cache-only plans are exempt).
///
/// Link ownership rule: deletion owns the link inode, never the target. A
/// `SkillsLink` proven to be an account link (validated with
/// `skills::validate_skill_link` against the shared-library root) is
/// `owned = true` and is removed with `skills::remove_link_only`, which never
/// traverses the target. Preserved links (kept, not deleted) are excluded
/// from the plan entirely, so they never block destructive apply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupTarget {
    pub path: PathBuf,
    pub kind: CleanupTargetKind,
    /// True when the manager may delete this target: provably owned data, or
    /// a proven account skill link (link inode only). Unknown credential
    /// targets and unvalidated links are never owned.
    pub owned: bool,
    pub size_bytes: u64,
    /// Backup covering this target; required before destructive apply.
    #[serde(default)]
    pub covered_by_backup: Option<String>,
}

/// Previewed deletion plan. `preview` (in `cleanup.rs`) builds this;
/// `apply` refuses to run unless every target is owned and, for non-cache
/// scopes, a verified backup covers it — except the explicit irreversible
/// `--no-backup` path. Preview plans carry `backup_id: None` by design;
/// `apply_cleanup` rederives the plan from the fresh registry and attaches
/// the verified backup internally.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub scope: CleanupScope,
    pub provider: ProviderId,
    pub account_id: String,
    pub targets: Vec<CleanupTarget>,
    pub backup_id: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}
