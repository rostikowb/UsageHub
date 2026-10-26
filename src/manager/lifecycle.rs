//! Managed account lifecycle: create / import / rename / open / focus /
//! stop / status / CLI environment.
//!
//! Rules enforced here:
//! - `SettingsFile.accounts` is the only registry. Every mutation goes
//!   through `manager::state` (named-mutex serialized, fresh load, mutate,
//!   atomic save). IDs are allocated from the append-only `used_ids` set and
//!   never reused, so retired theme bindings can never retarget a new
//!   account. Rename is display-only (`ProviderAccounts::rename`).
//! - New profiles live under the owned root
//!   (`paths::managed_account_dir`); adopted legacy paths (e.g. Second) are
//!   validated absolute-only and never moved. Rename never touches dirs,
//!   auth material, or credential targets.
//! - `open`/`focus`/`rename` acquire the per-account operation guard
//!   (`state::AccountOperationGuard`, landed by the cleanup slice) so a
//!   poll/token read or a cleanup deletion in flight blocks them instead of
//!   racing. `stop` deliberately takes NO account guard: cleanup calls it
//!   while already holding that guard.
//! - Status is read-only. Stop/focus only ever signal processes whose command
//!   line carries this account's marker (see `native`); there is no global
//!   kill. Forced termination is a separate explicit entry point.
//! - No auth/config/history sharing between accounts: `account_cli_env`
//!   returns directory roots plus the nonessential-traffic opt-out only.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::accounts::{AccountOwnership, AccountProfile, ProviderAccounts};
use crate::manager::{paths, state, AccountContext, ManagerResult};
use crate::providers::ProviderId;

// Re-exported so CLI/GUI forward them opaquely without reaching into `native`.
pub use crate::manager::native::{AccountStatus, StopReport};

/// Per-account operation-guard wait budget for open/focus/rename.
pub const ACCOUNT_OP_TIMEOUT_MS: u32 = 5_000;

/// Creation options for a new managed account.
///
/// New profiles live under the owned root; explicit `desktop_dir` /
/// `config_dir` overrides must be absolute and are adopted in place.
/// Managed creates are pinned by default so a fresh account is visible in
/// the account-aware widget (which filters enabled+pinned) without an
/// extra pin step; pass `pinned: false` explicitly to opt out, and the user
/// can unpin later. The CLI spells `--no-open` by simply skipping
/// `open_account` after a successful create, so no flag is needed here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewAccountOpts {
    pub desktop_dir: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub ownership: AccountOwnership,
    pub pinned: bool,
}

impl Default for NewAccountOpts {
    fn default() -> Self {
        Self {
            desktop_dir: None,
            config_dir: None,
            ownership: AccountOwnership::Managed,
            pinned: true,
        }
    }
}

/// Import options for adopting an existing profile directory in place.
/// `name` overrides the display name; `None` keeps the directory name.
/// Adoption pins by default (same widget-visibility reason as create);
/// pass `pinned: false` explicitly to opt out.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportOpts {
    pub name: Option<String>,
    pub pinned: bool,
}

impl Default for ImportOpts {
    fn default() -> Self {
        Self {
            name: None,
            pinned: true,
        }
    }
}

impl NewAccountOpts {
    fn resolved_ownership(&self) -> AccountOwnership {
        // `AccountOwnership::default()` is `External`; an explicit create
        // without ownership always means a new managed profile.
        match self.ownership {
            AccountOwnership::External => AccountOwnership::Managed,
            owned => owned,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry helpers (synchronous, guard-aware callers only)
// ---------------------------------------------------------------------------

fn supported(provider: ProviderId) -> ManagerResult<()> {
    match provider {
        ProviderId::Claude | ProviderId::Codex => Ok(()),
        _ => Err("This provider does not support account profiles".into()),
    }
}

fn provider_accounts_mut(
    settings: &mut crate::app_settings::SettingsFile,
    provider: ProviderId,
) -> ManagerResult<&mut ProviderAccounts> {
    match provider {
        ProviderId::Claude => Ok(&mut settings.accounts.claude),
        ProviderId::Codex => Ok(&mut settings.accounts.codex),
        _ => Err("This provider does not support account profiles".into()),
    }
}

fn find_profile(provider: ProviderId, id: &str) -> ManagerResult<AccountProfile> {
    let settings = state::load_fresh();
    let accounts = settings
        .accounts
        .get(provider)
        .ok_or_else(|| "This provider does not support account profiles".to_string())?;
    accounts
        .find(id)
        .cloned()
        .ok_or_else(|| format!("Unknown account '{id}'"))
}

fn load_context(provider: ProviderId, id: &str) -> ManagerResult<AccountContext> {
    let profile = find_profile(provider, id)?;
    AccountContext::from_profile(provider, &profile)
}

/// Mirror of `ProviderAccounts`' allocator: lowest free `account_N` drawn
/// from the append-only `used_ids` set. Retired IDs stay reserved, so they
/// can never be reissued to a new profile.
fn allocate_id(accounts: &mut ProviderAccounts) -> String {
    let mut number = 1;
    loop {
        let id = format!("account_{number}");
        if !accounts.used_ids.contains(&id) {
            accounts.used_ids.insert(id.clone());
            return id;
        }
        number += 1;
    }
}

fn validate_name(name: &str) -> ManagerResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Account name must not be empty".into());
    }
    Ok(trimmed.to_string())
}

/// Allocate the next id and atomically claim its managed base. Creation
/// itself (`create_dir`, not check-then-create) is the claim: a base that
/// already holds orphan user-data (lost settings, failed save) is skipped
/// with its id left retired in `used_ids`, so a new name can never inherit
/// old auth. An existing *empty* shell is reused harmlessly.
fn claim_fresh_managed_dir(
    settings: &mut crate::app_settings::SettingsFile,
    provider: ProviderId,
) -> ManagerResult<(String, PathBuf)> {
    for _ in 0..1000 {
        let id = {
            let accounts = provider_accounts_mut(settings, provider)?;
            allocate_id(accounts)
        };
        let base = paths::managed_account_dir(provider, &id)?;
        if try_claim_fresh_dir(&base)? {
            return Ok((id, base));
        }
    }
    Err("Unable to allocate a fresh profile directory".into())
}

/// Atomically claim `dir` as a fresh base: `true` when created here or an
/// empty shell, `false` when orphan user-data already occupies it.
fn try_claim_fresh_dir(dir: &Path) -> ManagerResult<bool> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create '{}': {error}", parent.display()))?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let empty = std::fs::read_dir(dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false);
            Ok(empty)
        }
        Err(error) => Err(format!("Unable to create '{}': {error}", dir.display())),
    }
}

/// Refuse explicit create dirs that already hold auth material: a create is
/// fresh by definition, so a logged-in tree must go through import (which
/// records the adoption) instead of silently inheriting old auth into a
/// new name.
fn refuse_inherited_auth(
    provider: ProviderId,
    desktop_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> ManagerResult<()> {
    if let Some(desktop) = desktop_dir {
        if desktop.join("config.json").is_file() && desktop.join("Local State").is_file() {
            return Err(
                "This desktop tree already holds a login; adopt it with import instead of create"
                    .into(),
            );
        }
        if desktop
            .join("claude-config")
            .join(expected_credential_file(provider))
            .is_file()
        {
            return Err(
                "This desktop tree already holds CLI auth; adopt it with import instead of create"
                    .into(),
            );
        }
    }
    if let Some(config) = config_dir {
        if config.join(expected_credential_file(provider)).is_file() {
            return Err(
                "This config dir already holds auth; adopt it with import instead of create".into(),
            );
        }
    }
    Ok(())
}

fn validate_absolute_opt(path: Option<PathBuf>, field: &str) -> ManagerResult<Option<PathBuf>> {
    match path {
        Some(path) => {
            if path.as_os_str().is_empty() {
                return Ok(None);
            }
            paths::require_absolute(&path)
                .map(Some)
                .map_err(|_| format!("{field} must be an absolute path or ~/path"))
        }
        None => Ok(None),
    }
}

/// Base data dir for a profile: explicit desktop dir wins, then explicit
/// config dir, then the owned managed root.
fn profile_base_dir(
    provider: ProviderId,
    id: &str,
    desktop_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> ManagerResult<PathBuf> {
    if let Some(dir) = desktop_dir {
        return paths::require_absolute(dir);
    }
    if let Some(dir) = config_dir {
        return paths::require_absolute(dir);
    }
    paths::managed_account_dir(provider, id)
}

/// Narrow profile markers that prove a directory really is a provider
/// profile root (and not a home dir, app-data dir, or sibling account).
fn required_marker(provider: ProviderId, desktop: bool) -> &'static str {
    if desktop {
        "config.json"
    } else {
        match provider {
            ProviderId::Claude => ".credentials.json",
            _ => "auth.json",
        }
    }
}

fn expected_credential_file(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => ".credentials.json",
        _ => "auth.json",
    }
}

/// Canonical spelling / overlap checks live in `native` (single shared
/// implementation used by attribution, validation, and the deletion root).
use crate::manager::native::{canonical_spelling, overlaps};

/// Canonical provider-Default roots: adopting them would hand cleanup the
/// shared default tree. Covers home CLI dirs, credential files, explicit env
/// overrides, the roaming desktop dir, and MSIX publisher caches. Shared with
/// the restore ownership gate (same virtual-source resolution, no token reads).
pub(crate) fn canonical_default_roots(provider: ProviderId) -> Vec<String> {
    let mut roots = Vec::new();
    let mut push = |path: Option<PathBuf>| {
        if let Some(path) = path {
            roots.push(canonical_spelling(&path));
        }
    };
    if let Some(home) = crate::known_folders::home_dir() {
        let folder = match provider {
            ProviderId::Claude => ".claude",
            _ => ".codex",
        };
        push(Some(home.join(folder)));
    }
    push(crate::accounts::default_credential_path(provider));
    push(crate::accounts::environment_directory(provider));
    if provider == ProviderId::Claude {
        if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
            push(Some(roaming.join("Claude")));
        }
        if let Some(local) = crate::known_folders::local_app_data_dir() {
            if let Ok(entries) = std::fs::read_dir(local.join("Packages")) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let publisher = name.strip_prefix("Claude_").unwrap_or_default();
                    if publisher.len() == 13
                        && publisher
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    {
                        push(Some(
                            entry
                                .path()
                                .join("LocalCache")
                                .join("Roaming")
                                .join("Claude"),
                        ));
                    }
                }
            }
        }
    }
    roots
}

/// Broad roots that may never be adopted: equal or *ancestor* matches are
/// refused (a parent of home would hand over the whole tree). Descendants
/// stay eligible — that is where real profiles (`~/.claude-work`, Second
/// under LocalAppData) live.
fn broad_parent_roots() -> Vec<String> {
    let mut roots = Vec::new();
    for dir in [
        crate::known_folders::home_dir(),
        crate::known_folders::roaming_app_data_dir(),
        crate::known_folders::local_app_data_dir(),
    ] {
        if let Some(dir) = dir {
            roots.push(canonical_spelling(&dir));
        }
    }
    roots
}

/// Fork-managed roots: adopting at, above, or below any of these would
/// capture backups, the shared library, or the whole fork tree.
fn fork_managed_roots() -> Vec<String> {
    let mut roots = Vec::new();
    for dir in [
        crate::manager::paths::fork_config_dir(),
        crate::manager::paths::fork_local_dir(),
        crate::manager::paths::profiles_root(),
        crate::manager::paths::fork_backups_dir(),
        crate::manager::skills::shared_skills_dir(),
    ] {
        if let Some(dir) = dir {
            roots.push(canonical_spelling(&dir));
        }
    }
    roots
}

/// Explicit roots of every OTHER registered account (both providers):
/// single ownership cannot share auth/config roots, so equal, ancestor, and
/// descendant overlaps are all refused. Only skills are shared.
fn sibling_roots(
    settings: &crate::app_settings::SettingsFile,
    provider: ProviderId,
    self_id: Option<&str>,
) -> Vec<String> {
    let mut roots = Vec::new();
    for probe in [ProviderId::Claude, ProviderId::Codex] {
        let Some(accounts) = settings.accounts.get(probe) else {
            continue;
        };
        for profile in &accounts.profiles {
            if probe == provider && self_id.is_some_and(|id| id == profile.id) {
                continue;
            }
            for field in [
                &profile.desktop_dir,
                &profile.config_dir,
                &profile.credentials_path,
            ] {
                if !field.trim().is_empty() {
                    roots.push(canonical_spelling(Path::new(field.trim())));
                }
            }
        }
    }
    roots
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdoptedKind {
    DesktopDir,
    ConfigDir,
    CredentialFile,
}

/// Fail-closed gate for any user-supplied profile root (create explicit
/// dirs, import adoption, deletion-root revalidation). `require_markers`
/// proves an existing non-empty dir really is a profile root; the owned
/// managed default skips it (fresh, created by us).
fn validate_adoptable_root(
    settings: &crate::app_settings::SettingsFile,
    provider: ProviderId,
    self_id: Option<&str>,
    candidate: &Path,
    kind: AdoptedKind,
    require_markers: bool,
) -> ManagerResult<PathBuf> {
    let absolute = paths::require_absolute(candidate)?;
    if absolute
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Err("Refusing to adopt a filesystem root".into());
    }
    if std::fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.is_symlink()) {
        return Err(format!(
            "Refusing to adopt a link '{}': adopt its target instead",
            absolute.display()
        ));
    }
    let canon = canonical_spelling(&absolute);
    for root in broad_parent_roots() {
        if canon == root || root.starts_with(&format!("{canon}\\")) {
            return Err(format!(
                "Refusing broad parent root '{}': adopt a narrow profile directory instead",
                absolute.display()
            ));
        }
    }
    for root in fork_managed_roots() {
        if overlaps(&canon, &root) {
            return Err(format!(
                "Refusing fork-managed root '{}': it belongs to UsageHub itself",
                absolute.display()
            ));
        }
    }
    for root in canonical_default_roots(provider) {
        if overlaps(&canon, &root) {
            return Err(format!(
                "Refusing canonical Default root '{}': it is already registered as the external Default",
                absolute.display()
            ));
        }
    }
    for root in sibling_roots(settings, provider, self_id) {
        if overlaps(&canon, &root) {
            return Err(format!(
                "Refusing '{}': it overlaps another registered account's auth/config root (single ownership; only skills are shared)",
                absolute.display()
            ));
        }
    }
    match kind {
        AdoptedKind::CredentialFile => {
            let name = absolute
                .file_name()
                .map(|stem| stem.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name != expected_credential_file(provider) {
                return Err(format!(
                    "Refusing '{}': not a recognized {} credential file",
                    absolute.display(),
                    expected_credential_file(provider)
                ));
            }
        }
        AdoptedKind::DesktopDir | AdoptedKind::ConfigDir => {
            if require_markers && absolute.is_dir() {
                let non_empty = std::fs::read_dir(&absolute)
                    .map(|mut entries| entries.any(|_| true))
                    .unwrap_or(false);
                if non_empty {
                    let marker = required_marker(provider, kind == AdoptedKind::DesktopDir);
                    if !absolute.join(marker).is_file() {
                        return Err(format!(
                            "Refusing '{}': no recognized profile marker ({})",
                            absolute.display(),
                            marker
                        ));
                    }
                }
            }
        }
    }
    Ok(absolute)
}

// ---------------------------------------------------------------------------
// Create / import / rename
// ---------------------------------------------------------------------------

/// Create a new managed account under the owned root (or an explicit
/// absolute base), wire its `skills` junction(s), and register it. Returns
/// the stored profile. Does not launch anything; call `open_account` after.
/// A default Claude create is a Desktop account (`desktop_dir` = owned
/// base, nested isolated `claude-config`); other providers default to a
/// plain CLI config root.
pub fn create_account(
    provider: ProviderId,
    name: &str,
    opts: NewAccountOpts,
) -> ManagerResult<AccountProfile> {
    supported(provider)?;
    let display = validate_name(name)?;
    if opts.desktop_dir.is_some() && provider != ProviderId::Claude {
        return Err("desktop_dir is Claude-only".into());
    }
    let ownership = opts.resolved_ownership();
    let pinned = opts.pinned;
    let desktop_dir = validate_absolute_opt(opts.desktop_dir, "desktop_dir")?;
    let config_dir = validate_absolute_opt(opts.config_dir, "config_dir")?;

    state::mutate_settings(|settings| {
        // Normalize once so every check below reads the committed view.
        // The borrow ends immediately: validation and FS work below only
        // ever hold shared borrows while a mutation is still pending.
        provider_accounts_mut(settings, provider)?.normalize();
        let owned_default = desktop_dir.is_none() && config_dir.is_none();
        // Fail-closed data-loss gate BEFORE any filesystem or registry
        // write. Explicit dirs must be narrow recognized profile roots
        // (markers required when they already hold data).
        if let Some(dir) = desktop_dir.as_deref() {
            validate_adoptable_root(settings, provider, None, dir, AdoptedKind::DesktopDir, true)?;
            refuse_inherited_auth(provider, Some(dir), None)?;
        }
        if let Some(dir) = config_dir.as_deref() {
            validate_adoptable_root(settings, provider, None, dir, AdoptedKind::ConfigDir, true)?;
            refuse_inherited_auth(provider, None, Some(dir))?;
        }
        // Allocate an id and claim its base exclusively. A fresh managed
        // base is never silently reused: when settings were lost (or a
        // save failed) and orphan user-data still sits on disk, the id
        // stays retired in `used_ids` and allocation moves on, so a new
        // name can never inherit old auth. Directory creation itself is
        // the atomic claim (`create_dir`, not check-then-create).
        let (id, base) = if owned_default {
            claim_fresh_managed_dir(settings, provider)?
        } else {
            let id = {
                let accounts = provider_accounts_mut(settings, provider)?;
                allocate_id(accounts)
            };
            let base =
                profile_base_dir(provider, &id, desktop_dir.as_deref(), config_dir.as_deref())?;
            (id, base)
        };
        if owned_default {
            // Our own fresh root under the profiles tree: exempt from the
            // fork-managed overlap check (it is inside it by construction),
            // but it still cannot collide with another account's root.
            let canon = canonical_spelling(&base);
            for root in sibling_roots(settings, provider, None) {
                if overlaps(&canon, &root) {
                    return Err(format!(
                        "Refusing '{}': it overlaps another registered account's auth/config root",
                        base.display()
                    ));
                }
            }
        }
        // Filesystem next, still inside the same serialized transaction:
        // a crash here must not leave a registered account with no dirs.
        // A Desktop account keeps CLI history/config isolated in a nested
        // `<desktop>/claude-config` dir (the real Second layout); the
        // desktop tree itself is never restructured, pre-existing nested
        // config is retained as-is. Default CLAUDE creates ARE Desktop
        // accounts: an empty `desktop_dir` would make the poller read a
        // nonexistent `.credentials.json` after a Desktop login, so the
        // owned base itself becomes the desktop dir with a nested isolated
        // CLI config. Other providers have no desktop app: their owned
        // default stays a plain CLI config root (`CredentialFile`).
        let owned_desktop = owned_default && provider == ProviderId::Claude;
        let nested_cli_config = desktop_dir
            .as_deref()
            .map(|desktop| desktop.join("claude-config"))
            .or_else(|| owned_desktop.then(|| base.join("claude-config")));
        paths::ensure_dir(&base)?;
        if let Some(nested) = nested_cli_config.as_deref() {
            paths::ensure_dir(nested)?;
            let _ = crate::manager::skills::ensure_account_skills_junction(nested)?;
        }
        paths::ensure_dir(&base.join("tmp"))?;
        let _ = crate::manager::skills::ensure_account_skills_junction(&base)?;
        let desktop_stored = desktop_dir
            .as_ref()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                if owned_desktop {
                    base.to_string_lossy().to_string()
                } else {
                    String::new()
                }
            });
        let profile = AccountProfile {
            id: id.clone(),
            name: display.clone(),
            config_dir: config_dir
                .as_ref()
                .map(|dir| dir.to_string_lossy().to_string())
                .unwrap_or_else(|| {
                    nested_cli_config
                        .as_deref()
                        .map(|nested| nested.to_string_lossy().to_string())
                        .unwrap_or_else(|| base.to_string_lossy().to_string())
                }),
            credentials_path: String::new(),
            enabled: true,
            desktop_dir: desktop_stored,
            ownership,
            pinned,
            sort_order: 0,
            color: String::new(),
            label: String::new(),
            credential_targets: Vec::new(),
        };
        // Reject sources the poller cannot serve before persisting.
        AccountContext::from_profile(provider, &profile)?;
        provider_accounts_mut(settings, provider)?
            .profiles
            .push(profile.clone());
        Ok(profile)
    })
}

/// Adopt an existing profile directory in place. It is never moved: a
/// directory containing `config.json` is adopted as `desktop_dir` (Claude
/// only), any other directory as `config_dir`, and a file as
/// `credentials_path`. The full fail-closed gate
/// ([`validate_adoptable_root`]) runs on the committed registry view:
/// filesystem roots, links, broad parents (home/app-data and their
/// ancestors), fork-managed trees, canonical Default roots, any
/// equal/ancestor/descendant overlap with another registered account's
/// auth/config root, and missing narrow markers are all refused with no
/// filesystem or registry write. Adoption is always `Managed`, which is
/// what authorizes cleanup to fully delete the adopted root later
/// (see [`owned_deletion_root`]); external profiles stay read-only by never
/// taking this path. Existing user skills are preserved (junction only when
/// no `skills` entry exists). The `Second` exact directory stays eligible.
pub fn import_account(
    provider: ProviderId,
    path: &Path,
    opts: ImportOpts,
) -> ManagerResult<AccountProfile> {
    supported(provider)?;
    let absolute = paths::require_absolute(path)?;
    if absolute
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Err("Refusing to adopt a filesystem root".into());
    }
    let metadata = std::fs::symlink_metadata(&absolute)
        .map_err(|_| format!("Import path does not exist: '{}'", absolute.display()))?;
    if metadata.is_symlink() {
        return Err(format!(
            "Refusing to adopt a link '{}': adopt its target instead",
            absolute.display()
        ));
    }
    let name = match opts.name {
        Some(name) => validate_name(&name)?,
        None => absolute
            .file_name()
            .map(|stem| stem.to_string_lossy().to_string())
            .filter(|stem| !stem.trim().is_empty())
            .ok_or_else(|| "Unable to derive an account name; pass one explicitly".to_string())?,
    };
    let is_desktop_data = metadata.is_dir() && absolute.join("config.json").is_file();
    if is_desktop_data && provider != ProviderId::Claude {
        return Err("desktop_dir is Claude-only".into());
    }
    // Real Second layout: the adopted desktop tree keeps Electron data at
    // its root while CLI history/config lives in a nested `claude-config`
    // subdir (sessions, .claude.json, projects, file history). The profile
    // records both: desktop_dir for the app launch, config_dir for the
    // isolated CLI env — one account, both histories isolated, and the old
    // Second config is retained untouched.
    let nested_cli_config = is_desktop_data.then(|| absolute.join("claude-config"));
    let (desktop_dir, config_dir, credentials_path) = if is_desktop_data {
        (
            absolute.to_string_lossy().to_string(),
            nested_cli_config
                .as_deref()
                .map(|nested| nested.to_string_lossy().to_string())
                .unwrap_or_default(),
            String::new(),
        )
    } else if metadata.is_dir() {
        (
            String::new(),
            absolute.to_string_lossy().to_string(),
            String::new(),
        )
    } else {
        (
            String::new(),
            String::new(),
            absolute.to_string_lossy().to_string(),
        )
    };

    state::mutate_settings(|settings| {
        // Normalize once; the borrow ends immediately so the fail-closed
        // gate below only ever holds shared borrows while mutating later.
        provider_accounts_mut(settings, provider)?.normalize();
        // Fail-closed data-loss gate on the registry view being committed:
        // narrow markers, no broad/canonical/sibling overlap. Markers are
        // required here (adopted trees already hold data by definition).
        let kind = if is_desktop_data {
            AdoptedKind::DesktopDir
        } else if metadata.is_dir() {
            AdoptedKind::ConfigDir
        } else {
            AdoptedKind::CredentialFile
        };
        if !metadata.is_dir() && !absolute.is_file() {
            return Err(format!(
                "Import path does not exist: '{}'",
                absolute.display()
            ));
        }
        validate_adoptable_root(settings, provider, None, &absolute, kind, true)?;
        let pinned = opts.pinned;
        let id = {
            let accounts = provider_accounts_mut(settings, provider)?;
            allocate_id(accounts)
        };
        let profile = AccountProfile {
            id: id.clone(),
            name: name.clone(),
            config_dir,
            credentials_path,
            enabled: true,
            desktop_dir,
            ownership: AccountOwnership::Managed,
            pinned,
            sort_order: 0,
            color: String::new(),
            label: String::new(),
            credential_targets: Vec::new(),
        };
        AccountContext::from_profile(provider, &profile)?;
        // Adopt in place: never move. Ensure a skills junction only when the
        // adopted tree has no `skills` entry; a real user dir is preserved.
        // For Desktop adoptions, ensure the nested `claude-config` CLI dir
        // exists (created empty when absent; pre-existing sessions, projects
        // and history are retained untouched) and link shared skills into
        // BOTH the desktop base and the nested CLI config, so the actual
        // CLI discovers the shared library from its own config home.
        if let Some(nested) = nested_cli_config.as_deref() {
            paths::ensure_dir(nested)?;
            let _ = crate::manager::skills::ensure_account_skills_junction(nested)?;
        }
        let base = if metadata.is_dir() {
            absolute.clone()
        } else {
            absolute
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| absolute.clone())
        };
        let _ = crate::manager::skills::ensure_account_skills_junction(&base)?;
        provider_accounts_mut(settings, provider)?
            .profiles
            .push(profile.clone());
        Ok(profile)
    })
}

/// Display-only rename. The stable `id` is preserved, so theme bindings,
/// backup directories, shortcuts, and taskbar identities keep targeting the
/// same account. Dirs, auth material, and credential targets are untouched.
pub fn rename_account(provider: ProviderId, id: &str, new_name: &str) -> ManagerResult<()> {
    supported(provider)?;
    let display = validate_name(new_name)?;
    let _op = state::AccountOperationGuard::acquire(provider, id, ACCOUNT_OP_TIMEOUT_MS)?;
    state::mutate_settings(|settings| {
        let accounts = provider_accounts_mut(settings, provider)?;
        accounts
            .rename(id, &display)
            .map_err(|error| error.to_string())
    })
}

// ---------------------------------------------------------------------------
// Open / focus / stop / status / CLI env
// ---------------------------------------------------------------------------

/// Launch the official app as a separate window for this account, then
/// stamp the first window once it exists (bounded wait, best-effort, so a
/// single open already shows grouped props/icons with no second click).
/// Holds the per-account operation guard so an in-flight poll or cleanup
/// deletion blocks the launch instead of racing it. Callers run this off
/// the UI thread (worker/CLI), so the bounded wait never freezes the UI.
pub fn open_account(provider: ProviderId, id: &str) -> ManagerResult<()> {
    supported(provider)?;
    let _op = state::AccountOperationGuard::acquire(provider, id, ACCOUNT_OP_TIMEOUT_MS)?;
    let ctx = load_context(provider, id)?;
    crate::manager::native::launch_and_stamp_account(&ctx, &[])?;
    Ok(())
}

/// Focus this account's existing window instead of opening a duplicate.
/// Returns `true` when a window was brought forward.
pub fn focus_account_window(provider: ProviderId, id: &str) -> ManagerResult<bool> {
    supported(provider)?;
    let _op = state::AccountOperationGuard::acquire(provider, id, ACCOUNT_OP_TIMEOUT_MS)?;
    let ctx = load_context(provider, id)?;
    crate::manager::native::focus_account_window(&ctx)
}

/// Gracefully stop this account's processes: close only its own windows,
/// wait, then verify locks are released. Takes NO operation guard — cleanup
/// calls this while already holding that guard.
pub fn stop_account_processes(provider: ProviderId, id: &str) -> ManagerResult<StopReport> {
    supported(provider)?;
    let ctx = load_context(provider, id)?;
    crate::manager::native::stop_account_graceful(&ctx)
}

/// Forced stop for one account: terminates only its matched PIDs without a
/// graceful close. Explicit user action only (`--force`); never invoked
/// implicitly by cleanup, the controller, or the widget.
pub fn force_stop_account_processes(provider: ProviderId, id: &str) -> ManagerResult<StopReport> {
    supported(provider)?;
    let ctx = load_context(provider, id)?;
    crate::manager::native::stop_account_force(&ctx)
}

/// Read-only process/window status for one account. Never equates a hidden
/// (tray) window with a stopped account: `running` reflects live PIDs only.
pub fn account_status(provider: ProviderId, id: &str) -> ManagerResult<AccountStatus> {
    supported(provider)?;
    let ctx = load_context(provider, id)?;
    crate::manager::native::account_status_for(&ctx)
}

/// Exact owned narrow root: `<profiles>/<provider>/<id>`. Ours by
/// construction for both Desktop and CLI layouts (a Desktop create nests its
/// isolated `claude-config` underneath, a CLI create uses the dir directly).
fn is_exact_managed_root(provider: ProviderId, id: &str, candidate: &Path) -> bool {
    paths::managed_account_dir(provider, id)
        .map(|owned| canonical_spelling(&owned) == canonical_spelling(candidate))
        .unwrap_or(false)
}

/// Registry proof that `ctx` is the current stored Managed record: the fresh
/// profile exists, is still Managed, and its source fields are unchanged.
/// Only then may the deletion gates skip the marker-presence check, so a
/// surviving owned root stays deletable after a partial failure removed its
/// auth marker. A stale or unregistered context (prospective adoption,
/// drifted registry, legacy migration before claim) gets no bypass and must
/// still carry valid Desktop/CLI markers.
fn fresh_managed_source_matches(
    settings: &crate::app_settings::SettingsFile,
    ctx: &AccountContext,
) -> bool {
    let Some(accounts) = settings.accounts.get(ctx.provider) else {
        return false;
    };
    let Some(fresh) = accounts.find(&ctx.profile.id) else {
        return false;
    };
    fresh.ownership == AccountOwnership::Managed
        && ctx.ownership == AccountOwnership::Managed
        && fresh.desktop_dir.trim() == ctx.profile.desktop_dir.trim()
        && fresh.config_dir.trim() == ctx.profile.config_dir.trim()
        && fresh.credentials_path.trim() == ctx.profile.credentials_path.trim()
}

/// Whether `path` is a registry-proven deletable root for `ctx` whose marker
/// presence alone may be skipped. Every other gate (absolute, link, broad
/// parent, canonical Default, sibling overlap, library/fork, unknown reparse)
/// still applies at the call site; this only answers "the durable registry,
/// not a stray marker file, proves this exact path is ours".
pub(crate) fn deletion_root_marker_exempt(ctx: &AccountContext, path: &Path) -> bool {
    if ctx.ownership != AccountOwnership::Managed {
        return false;
    }
    let settings = state::load_fresh();
    if !fresh_managed_source_matches(&settings, ctx) {
        return false;
    }
    if is_exact_managed_root(ctx.provider, &ctx.profile.id, path) {
        return true;
    }
    for base in ctx.config_dir.iter().chain(ctx.desktop_dir.iter()) {
        if canonical_spelling(base) == canonical_spelling(path) {
            return true;
        }
    }
    false
}

/// Validate the exact owned narrow root `<profiles>/<provider>/<id>`:
/// absolute, not itself a link, not a filesystem root, never a broad parent
/// of a user root, never canonical external data, and never overlapping
/// another account's auth/config root. Marker presence and fork-overlap are
/// skipped: the narrow root lives under the fork profiles root by design and
/// may not carry markers before first open (fresh no-open Desktop).
fn validate_exact_managed_root(
    settings: &crate::app_settings::SettingsFile,
    ctx: &AccountContext,
    candidate: &Path,
) -> ManagerResult<PathBuf> {
    let absolute = paths::require_absolute(candidate)?;
    if absolute
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Err("Refusing to adopt a filesystem root".into());
    }
    if std::fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.is_symlink()) {
        return Err(format!(
            "Refusing to adopt a link '{}': adopt its target instead",
            absolute.display()
        ));
    }
    let canon = canonical_spelling(&absolute);
    for root in broad_parent_roots() {
        if canon == root || root.starts_with(&format!("{canon}\\")) {
            return Err(format!(
                "Refusing broad parent root '{}': adopt a narrow profile directory instead",
                absolute.display()
            ));
        }
    }
    for root in canonical_default_roots(ctx.provider) {
        if overlaps(&canon, &root) {
            return Err(format!(
                "Refusing canonical Default root '{}': it is already registered as the external Default",
                absolute.display()
            ));
        }
    }
    for root in sibling_roots(settings, ctx.provider, Some(ctx.profile.id.as_str())) {
        if overlaps(&canon, &root) {
            return Err(format!(
                "Refusing '{}': it overlaps another registered account's auth/config root (single ownership; only skills are shared)",
                absolute.display()
            ));
        }
    }
    Ok(absolute)
}

/// Exact filesystem root cleanup may fully delete for this account.
///
/// - Managed + explicit `desktop_dir` (e.g. an adopted Second tree): that
///   dir. Managed + explicit `config_dir`: that dir. Managed + explicit
///   credential file only: that file. Managed owned-root: the managed
///   account dir. External profiles: `None` (read-only; a full reset must
///   opt in through cleanup's own explicit flow, never through this).
///
/// Fail-closed: an explicit root is re-validated here (markers, broad /
/// canonical / sibling overlap, normalized spellings). A tampered or
/// drifted registry entry errors instead of handing cleanup a deletable
/// sibling/Default tree. The exact owned narrow root
/// (`<profiles>/<provider>/<id>`, Desktop or CLI) is ours by construction:
/// it skips marker and fork-overlap checks but still cannot collide with
/// another account, so a fresh no-open Desktop profile without `config.json`
/// still backs up and deletes. A registered Managed adopted root whose
/// current context still matches the fresh record skips marker-presence
/// only (every other gate stays); prospective or drifted contexts must
/// still carry valid Desktop/CLI markers.
pub fn owned_deletion_root(ctx: &AccountContext) -> ManagerResult<Option<PathBuf>> {
    if ctx.ownership != AccountOwnership::Managed {
        return Ok(None);
    }
    let settings = state::load_fresh();
    let self_id = Some(ctx.profile.id.as_str());
    // Registry proof gates every marker skip below: a surviving owned root
    // stays deletable after a partial failure removed its auth marker, while
    // prospective or drifted contexts must still carry valid markers.
    let proven = fresh_managed_source_matches(&settings, ctx);
    if let Some(dir) = &ctx.desktop_dir {
        let candidate = Path::new(dir);
        if is_exact_managed_root(ctx.provider, &ctx.profile.id, candidate) {
            return Ok(Some(validate_exact_managed_root(
                &settings, ctx, candidate,
            )?));
        }
        let root = validate_adoptable_root(
            &settings,
            ctx.provider,
            self_id,
            candidate,
            AdoptedKind::DesktopDir,
            !proven,
        )?;
        return Ok(Some(root));
    }
    if let Some(dir) = &ctx.config_dir {
        let candidate = Path::new(dir);
        if is_exact_managed_root(ctx.provider, &ctx.profile.id, candidate) {
            return Ok(Some(validate_exact_managed_root(
                &settings, ctx, candidate,
            )?));
        }
        let root = validate_adoptable_root(
            &settings,
            ctx.provider,
            self_id,
            candidate,
            AdoptedKind::ConfigDir,
            !proven,
        )?;
        return Ok(Some(root));
    }
    if ctx.profile.credentials_path.trim().is_empty() {
        return Ok(Some(paths::managed_account_dir(
            ctx.provider,
            &ctx.profile.id,
        )?));
    }
    let file = ctx
        .credential_file
        .clone()
        .ok_or_else(|| format!("Account '{}' has no credential source", ctx.profile.id))?;
    validate_adoptable_root(
        &settings,
        ctx.provider,
        self_id,
        &file,
        AdoptedKind::CredentialFile,
        true,
    )?;
    Ok(Some(file))
}

/// Explicit child environment for running the provider CLI as this account:
/// directory roots plus the nonessential-traffic opt-out. Never tokens,
/// credential files, or auth material; no global env mutation.
pub fn account_cli_env(ctx: &AccountContext) -> ManagerResult<Vec<(String, String)>> {
    crate::manager::native::account_child_env(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_opts() -> NewAccountOpts {
        NewAccountOpts {
            desktop_dir: None,
            config_dir: None,
            ownership: AccountOwnership::Managed,
            pinned: true,
        }
    }

    #[test]
    fn create_then_rename_keeps_identity_dirs_and_auth() {
        let profile = create_account(ProviderId::Claude, "Work", test_opts())
            .expect("create managed account");
        assert!(profile.enabled);
        assert_eq!(profile.ownership, AccountOwnership::Managed);
        let before_base = profile.config_dir.clone();
        assert!(!before_base.is_empty());
        assert!(Path::new(&before_base).is_dir());

        rename_account(ProviderId::Claude, &profile.id, "Work Renamed")
            .expect("display-only rename");
        let after = find_profile(ProviderId::Claude, &profile.id).expect("same id");
        assert_eq!(after.id, profile.id);
        assert_eq!(after.name, "Work Renamed");
        // Dirs, auth material, and targets are untouched by rename.
        assert_eq!(after.config_dir, before_base);
        assert_eq!(after.desktop_dir, profile.desktop_dir);
        assert_eq!(after.credentials_path, profile.credentials_path);
        assert_eq!(after.credential_targets, profile.credential_targets);
    }

    #[test]
    fn allocated_ids_are_never_reused() {
        let first = create_account(ProviderId::Codex, "One", test_opts()).expect("first");
        let second = create_account(ProviderId::Codex, "Two", test_opts()).expect("second");
        assert_ne!(first.id, second.id);
        // Simulate deletion: drop the profile but keep the retired id, the
        // way cleanup does (normalize is append-only over `used_ids`).
        state::mutate_settings(|settings| {
            let accounts = provider_accounts_mut(settings, ProviderId::Codex)?;
            accounts.profiles.retain(|profile| profile.id != first.id);
            Ok(())
        })
        .expect("retire first profile");
        let third = create_account(ProviderId::Codex, "Three", test_opts()).expect("third");
        assert_ne!(third.id, first.id);
        assert_ne!(third.id, second.id);
        assert!(find_profile(ProviderId::Codex, &first.id).is_err());
    }

    #[test]
    fn import_rejects_relative_and_missing_paths() {
        assert!(import_account(
            ProviderId::Claude,
            Path::new("relative/path"),
            ImportOpts {
                name: Some("X".into()),
                ..Default::default()
            },
        )
        .is_err());
        assert!(import_account(
            ProviderId::Claude,
            Path::new("C:\\UsageHub\\definitely-missing-dir-xyz"),
            ImportOpts {
                name: Some("X".into()),
                ..Default::default()
            },
        )
        .is_err());
    }

    #[test]
    fn cli_env_points_at_isolated_dirs_without_tokens() {
        let profile = create_account(ProviderId::Claude, "Env", test_opts()).expect("create");
        let ctx = load_context(ProviderId::Claude, &profile.id).expect("context");
        let env = account_cli_env(&ctx).expect("cli env");
        let config = env
            .iter()
            .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            .expect("config dir override");
        assert!(config.1.contains(&profile.id));
        assert!(env.iter().any(|(key, _)| key == "TEMP"));
        assert!(env.iter().any(|(key, _)| key == "TMP"));
        for (key, value) in &env {
            assert!(!key.to_lowercase().contains("token"));
            assert!(!value.to_lowercase().contains("token"));
        }
    }

    #[test]
    fn codex_default_stays_cli_config_without_desktop() {
        let profile = create_account(ProviderId::Codex, "Codex CLI", test_opts()).expect("create");
        assert!(profile.enabled && profile.pinned);
        assert!(profile.desktop_dir.is_empty());
        assert!(!profile.config_dir.is_empty());
        let ctx = load_context(ProviderId::Codex, &profile.id).expect("context");
        assert_eq!(ctx.source, crate::accounts::SourceKind::CredentialFile);
        let env = account_cli_env(&ctx).expect("cli env");
        assert!(env.iter().any(|(key, _)| key == "CODEX_HOME"));
        assert_eq!(
            owned_deletion_root(&ctx).expect("root"),
            Some(PathBuf::from(&profile.config_dir))
        );
    }

    #[test]
    fn deletion_root_covers_managed_and_spares_external() {
        // Owned-root create: the managed account dir is the deletable root.
        let owned = create_account(ProviderId::Claude, "Owned", test_opts()).expect("create");
        let owned_ctx = load_context(ProviderId::Claude, &owned.id).expect("context");
        let root = owned_deletion_root(&owned_ctx)
            .expect("root")
            .expect("managed root");
        assert!(root.to_string_lossy().contains(&owned.id));

        // Adopted legacy dir (Second-style): the adopted tree itself.
        let dir = tempfile::tempdir().expect("temp dir");
        let legacy = dir.path().join("Second");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("config.json"), "{}").unwrap();
        let adopted = import_account(
            ProviderId::Claude,
            &legacy,
            ImportOpts {
                name: Some("Second".into()),
                ..Default::default()
            },
        )
        .expect("adopt legacy dir");
        assert_eq!(adopted.ownership, AccountOwnership::Managed);
        let adopted_ctx = load_context(ProviderId::Claude, &adopted.id).expect("context");
        assert_eq!(
            owned_deletion_root(&adopted_ctx).expect("root"),
            Some(legacy.clone())
        );
        // Desktop adoption records the nested isolated CLI config; the
        // adopted tree itself is untouched apart from the additive nesting.
        assert_eq!(
            adopted.config_dir,
            legacy.join("claude-config").to_string_lossy().to_string()
        );
        assert!(legacy.join("claude-config").is_dir());
        assert!(legacy.join("config.json").is_file());

        // External default: no deletable root, cleanup must opt in itself.
        let external_ctx =
            AccountContext::from_profile(ProviderId::Claude, &AccountProfile::default())
                .expect("default context");
        assert_eq!(external_ctx.ownership, AccountOwnership::External);
        assert_eq!(owned_deletion_root(&external_ctx).expect("root"), None);
    }

    #[test]
    fn import_refuses_roots_and_links() {
        assert!(import_account(
            ProviderId::Claude,
            Path::new("C:\\"),
            ImportOpts {
                name: Some("X".into()),
                ..Default::default()
            },
        )
        .is_err());
    }

    #[test]
    fn new_desktop_creation_uses_isolated_nested_cli_config() {
        let dir = tempfile::tempdir().expect("temp dir");
        let desktop = dir.path().join("fresh-desktop");
        let created = create_account(
            ProviderId::Claude,
            "Fresh",
            NewAccountOpts {
                desktop_dir: Some(desktop.clone()),
                ..test_opts()
            },
        )
        .expect("new desktop account");
        let nested = desktop.join("claude-config");
        assert_eq!(created.desktop_dir, desktop.to_string_lossy().to_string());
        assert_eq!(created.config_dir, nested.to_string_lossy().to_string());
        assert!(nested.is_dir());
        // CLI env points at the nested dir, never at the desktop root.
        let ctx = load_context(ProviderId::Claude, &created.id).expect("context");
        let env = account_cli_env(&ctx).expect("cli env");
        let config = env
            .iter()
            .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            .expect("config dir override");
        assert_eq!(config.1, nested.to_string_lossy().to_string());
    }

    #[test]
    fn canonical_spelling_normalizes_case_slash_and_nt_prefix() {
        let plain = canonical_spelling(Path::new("C:/UsageHub/Profiles"));
        assert_eq!(
            plain,
            canonical_spelling(Path::new("c:\\usagehub\\profiles\\"))
        );
        assert_eq!(
            plain,
            canonical_spelling(Path::new("\\\\?\\C:\\UsageHub\\Profiles"))
        );
        assert!(overlaps(
            &plain,
            &canonical_spelling(Path::new("c:\\usagehub"))
        ));
        assert!(overlaps(
            &canonical_spelling(Path::new("C:\\UsageHub\\profiles\\claude\\account_1")),
            &plain
        ));
        assert!(!overlaps(
            &plain,
            &canonical_spelling(Path::new("C:\\Other"))
        ));
        // Sibling id prefix is not an overlap.
        assert!(!overlaps(
            &canonical_spelling(Path::new("C:\\p\\account_1")),
            &canonical_spelling(Path::new("C:\\p\\account_10"))
        ));
    }

    #[test]
    fn canonical_default_roots_cover_home_cli_tree() {
        let roots = canonical_default_roots(ProviderId::Claude);
        let home = crate::known_folders::home_dir().expect("home");
        assert!(roots.contains(&canonical_spelling(&home.join(".claude"))));
        // A nested spelling still overlaps the canonical root.
        let nested = canonical_spelling(&home.join(".CLAUDE").join("sub"));
        assert!(roots.iter().any(|root| overlaps(&nested, root)));
    }

    #[test]
    fn create_rejects_broad_and_canonical_roots() {
        // Home and the canonical ~/.claude tree can never be adopted.
        let home = crate::known_folders::home_dir().expect("home");
        assert!(create_account(
            ProviderId::Claude,
            "Home",
            NewAccountOpts {
                config_dir: Some(home.clone()),
                ..test_opts()
            },
        )
        .is_err());
        assert!(create_account(
            ProviderId::Claude,
            "Canon",
            NewAccountOpts {
                config_dir: Some(home.join(".claude")),
                ..test_opts()
            },
        )
        .is_err());
        // Marker-less non-empty dir is not a recognized profile root.
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("junk.txt"), "x").unwrap();
        assert!(create_account(
            ProviderId::Claude,
            "Junk",
            NewAccountOpts {
                config_dir: Some(dir.path().to_path_buf()),
                ..test_opts()
            },
        )
        .is_err());
    }

    #[test]
    fn create_rejects_sibling_auth_config_overlap() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = dir.path().join("first");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join(".credentials.json"), "{}").unwrap();
        // Adoption with auth material is import's job; create would refuse
        // the inherited auth. The intruders below are refused by the
        // sibling-overlap gate regardless of their own auth state.
        let owner = import_account(
            ProviderId::Claude,
            &first,
            ImportOpts {
                name: Some("Owner".into()),
                ..Default::default()
            },
        )
        .expect("first adoption");
        // Same, child, and parent roots are all single-ownership violations.
        for candidate in [first.clone(), first.join("child"), dir.path().to_path_buf()] {
            if candidate != first {
                std::fs::create_dir_all(&candidate).ok();
                std::fs::write(candidate.join(".credentials.json"), "{}").ok();
            }
            assert!(
                create_account(
                    ProviderId::Claude,
                    "Intruder",
                    NewAccountOpts {
                        config_dir: Some(candidate.clone()),
                        ..test_opts()
                    },
                )
                .is_err(),
                "overlap must be refused: {}",
                candidate.display()
            );
        }
        // The owner's own deletion root is exactly the adopted dir.
        let ctx = load_context(ProviderId::Claude, &owner.id).expect("context");
        assert_eq!(owned_deletion_root(&ctx).expect("root"), Some(first));
    }

    #[test]
    fn default_create_is_desktop_with_nested_cli_config() {
        let profile =
            create_account(ProviderId::Claude, "Default", test_opts()).expect("default create");
        assert!(profile.pinned, "managed creates are pinned by default");
        let ctx = load_context(ProviderId::Claude, &profile.id).expect("context");
        assert_eq!(ctx.source, crate::accounts::SourceKind::DesktopDir);
        let base = crate::manager::paths::managed_account_dir(ProviderId::Claude, &profile.id)
            .expect("managed base");
        assert_eq!(profile.desktop_dir, base.to_string_lossy().to_string());
        let nested = base.join("claude-config");
        assert_eq!(profile.config_dir, nested.to_string_lossy().to_string());
        assert!(nested.is_dir());
        // Skills are linked from BOTH the desktop base and the nested CLI
        // config, so the actual CLI discovers the shared library.
        assert_eq!(
            crate::manager::paths::first_reparse_point(&base.join("skills")).as_deref(),
            Some(base.join("skills")).as_deref()
        );
        assert_eq!(
            crate::manager::paths::first_reparse_point(&nested.join("skills")).as_deref(),
            Some(nested.join("skills")).as_deref()
        );
        // CLI env points at the nested dir, and deletion owns the desktop root.
        let env = account_cli_env(&ctx).expect("cli env");
        let config = env
            .iter()
            .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            .expect("config dir override");
        assert_eq!(config.1, nested.to_string_lossy().to_string());
        assert_eq!(owned_deletion_root(&ctx).expect("root"), Some(base));
    }

    #[test]
    fn fresh_base_never_reuses_orphan_user_data() {
        // Simulate a lost registry: orphan user-data sits where account_1
        // would land. The create must skip it (id retired, never reused)
        // and land on a fresh dir instead of inheriting old auth.
        let orphan = crate::manager::paths::managed_account_dir(ProviderId::Codex, "account_1")
            .expect("orphan base");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(".credentials.json"), "{}").unwrap();
        let profile = create_account(ProviderId::Codex, "Fresh", test_opts()).expect("create");
        assert_ne!(profile.id, "account_1");
        assert!(!profile.config_dir.contains("account_1"));
        assert!(Path::new(&profile.config_dir).is_dir());
    }

    #[test]
    fn create_refuses_dirs_already_holding_auth() {
        let dir = tempfile::tempdir().expect("temp dir");
        let logged_in = dir.path().join("logged-in");
        std::fs::create_dir_all(&logged_in).unwrap();
        std::fs::write(logged_in.join("config.json"), "{}").unwrap();
        std::fs::write(logged_in.join("Local State"), "{}").unwrap();
        assert!(create_account(
            ProviderId::Claude,
            "Takeover",
            NewAccountOpts {
                desktop_dir: Some(logged_in),
                ..test_opts()
            },
        )
        .is_err());
        let cli_home = dir.path().join("cli-home");
        std::fs::create_dir_all(&cli_home).unwrap();
        std::fs::write(cli_home.join(".credentials.json"), "{}").unwrap();
        assert!(create_account(
            ProviderId::Claude,
            "Takeover",
            NewAccountOpts {
                config_dir: Some(cli_home),
                ..test_opts()
            },
        )
        .is_err());
    }

    #[test]
    fn migration_style_desktop_env_falls_back_to_nested() {
        // A Desktop profile whose registry config_dir is empty (e.g. an
        // older migration adopt) still gets an isolated CLI env: the
        // canonical nested dir, never provider-default discovery.
        let dir = tempfile::tempdir().expect("temp dir");
        let desktop = dir.path().join("second");
        std::fs::create_dir_all(&desktop).unwrap();
        // Correct Desktop layout: the narrow adoption marker the ownership
        // gate requires. The gate itself is untouched (markers still
        // required for non-empty adopted roots).
        std::fs::write(desktop.join("config.json"), "{}").unwrap();
        let mut profile = AccountProfile::default();
        profile.id = "second".into();
        profile.name = "Second".into();
        profile.desktop_dir = desktop.to_string_lossy().to_string();
        profile.ownership = AccountOwnership::Managed;
        let ctx = AccountContext::from_profile(ProviderId::Claude, &profile).expect("context");
        let env = account_cli_env(&ctx).expect("cli env");
        let config = env
            .iter()
            .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            .expect("config dir override");
        assert_eq!(
            config.1,
            desktop.join("claude-config").to_string_lossy().to_string()
        );
        assert_eq!(owned_deletion_root(&ctx).expect("root"), Some(desktop));
    }
}
