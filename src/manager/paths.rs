//! Fork data roots and safe path primitives.
//!
//! Roots (all under the fork namespace, never the original app's dirs):
//! - config: `%APPDATA%/UsageHub` (settings, themes, shared skills library)
//! - local: `%LOCALAPPDATA%/UsageHub` (secrets, backups)
//! - profiles: `%LOCALAPPDATA%/UsageHub/profiles` (initial default; per-account
//!   `desktop_dir`/`config_dir` may point elsewhere, adopted legacy paths such
//!   as Second stay in place and are never moved on rename)

use std::path::{Component, Path, PathBuf};

use crate::manager::ManagerResult;
use crate::providers::ProviderId;

pub const FORK_CONFIG_NAMESPACE: &str = "UsageHub";
pub const PROFILES_DIR_NAME: &str = "profiles";
pub const BACKUPS_DIR_NAME: &str = "backups";
pub const SKILLS_LIBRARY_DIR_NAME: &str = "skills-library";
/// Environment override for the fork config root (smoke/portable use).
/// Takes precedence over the known-folder default when set; must be absolute.
pub const CONFIG_ROOT_ENV: &str = "USAGEHUB_CONFIG_ROOT";
/// Environment override for the fork local root (smoke/portable use).
/// Backups and profiles derive from it when set; must be absolute.
pub const LOCAL_ROOT_ENV: &str = "USAGEHUB_LOCAL_ROOT";
/// Environment override for the managed profiles root (tests, portable use).
pub const PROFILES_ROOT_ENV: &str = "USAGEHUB_PROFILES_ROOT";

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `FILE_ATTRIBUTE_DIRECTORY` for classifying a link inode via
/// `symlink_metadata` without following a dangling target.
pub const FILE_ATTRIBUTE_DIRECTORY_FOR_LINK: u32 = 0x10;

/// Raw non-empty override value for `var`, if present.
fn env_override(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Fail fast on relative/empty root overrides.
///
/// Intended for `main()` and the future `manager::cli` entry point before any
/// filesystem work: absolute-only overrides keep `join_within` errors honest
/// instead of producing misleading "Path escapes its root" failures.
/// Returns `Ok(())` when every set override is an absolute path.
pub fn validate_root_overrides() -> ManagerResult<()> {
    for var in [CONFIG_ROOT_ENV, LOCAL_ROOT_ENV, PROFILES_ROOT_ENV] {
        if let Some(path) = env_override(var) {
            if !path.is_absolute() {
                return Err(format!(
                    "{var} must be an absolute path, got '{}'",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Fork config root: `%APPDATA%/UsageHub`, or `USAGEHUB_CONFIG_ROOT` when set.
///
/// In `cfg(test)` this is always the per-test-thread
/// `app_settings::app_data_directory()` fixture (env ignored) so manager
/// tests can never touch the real roaming profile.
#[cfg(not(test))]
pub fn fork_config_dir() -> Option<PathBuf> {
    if let Some(path) = env_override(CONFIG_ROOT_ENV) {
        return Some(path);
    }
    crate::known_folders::roaming_app_data_dir().map(|dir| dir.join(FORK_CONFIG_NAMESPACE))
}

/// Test fixture root: per-thread settings dir (see `app_settings`).
/// `app_settings::app_data_directory()` in `cfg(test)` is thread-local and
/// never calls back into `manager::paths`, so there is no recursion.
#[cfg(test)]
pub fn fork_config_dir() -> Option<PathBuf> {
    Some(crate::app_settings::app_data_directory())
}

/// Fork local root: `%LOCALAPPDATA%/UsageHub` (secrets, backups), or
/// `USAGEHUB_LOCAL_ROOT` when set.
///
/// In `cfg(test)` this is `<app_data_directory()>/local` (per-test-thread
/// fixture with a local suffix, env ignored).
#[cfg(not(test))]
pub fn fork_local_dir() -> Option<PathBuf> {
    if let Some(path) = env_override(LOCAL_ROOT_ENV) {
        return Some(path);
    }
    crate::known_folders::local_app_data_dir().map(|dir| dir.join(FORK_CONFIG_NAMESPACE))
}

/// Test fixture local root: per-thread settings dir plus a `local` suffix.
#[cfg(test)]
pub fn fork_local_dir() -> Option<PathBuf> {
    Some(crate::app_settings::app_data_directory().join("local"))
}

/// Encrypted backup root: `%LOCALAPPDATA%/UsageHub/backups` (non-roaming),
/// or `<USAGEHUB_LOCAL_ROOT>/backups` when overridden.
pub fn fork_backups_dir() -> Option<PathBuf> {
    fork_local_dir().map(|dir| dir.join(BACKUPS_DIR_NAME))
}

/// Initial managed profiles root: `%LOCALAPPDATA%/UsageHub/profiles`,
/// or `<USAGEHUB_LOCAL_ROOT>/profiles` when overridden.
pub fn default_profiles_root() -> Option<PathBuf> {
    fork_local_dir().map(|dir| dir.join(PROFILES_DIR_NAME))
}

/// Effective managed profiles root ([`PROFILES_ROOT_ENV`] wins when set).
///
/// In `cfg(test)` the per-test-thread fixture wins and env is ignored so
/// parallel tests never race on process-global `set_var` or touch real dirs.
#[cfg(not(test))]
pub fn profiles_root() -> Option<PathBuf> {
    if let Some(path) = env_override(PROFILES_ROOT_ENV) {
        return Some(path);
    }
    default_profiles_root()
}

/// Test fixture profiles root: `<app_data_directory()>/local/profiles`.
#[cfg(test)]
pub fn profiles_root() -> Option<PathBuf> {
    default_profiles_root()
}

/// Managed data dir for a new profile: `<profiles>/<provider>/<account_id>`.
/// The account id charset matches `ProviderAccounts::normalize`
/// (ASCII alphanumeric + `_`); anything else is rejected.
pub fn managed_account_dir(provider: ProviderId, account_id: &str) -> ManagerResult<PathBuf> {
    if account_id.is_empty()
        || !account_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!("Unsafe account directory id '{account_id}'"));
    }
    let root = profiles_root().ok_or("Unable to resolve the profiles directory")?;
    let provider_key = match provider {
        ProviderId::Claude => "claude",
        ProviderId::Codex => "codex",
        _ => return Err("This provider does not support account profiles".into()),
    };
    join_within(&root, &format!("{provider_key}/{account_id}"))
}

/// Backup dir for one account: `<backups>/<provider>/<account_id>`.
pub fn backup_dir_for(provider: ProviderId, account_id: &str) -> ManagerResult<PathBuf> {
    if account_id.is_empty()
        || !account_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(format!("Unsafe account backup id '{account_id}'"));
    }
    let root = fork_backups_dir().ok_or("Unable to resolve the backups directory")?;
    let provider_key = match provider {
        ProviderId::Claude => "claude",
        ProviderId::Codex => "codex",
        _ => return Err("This provider does not support account profiles".into()),
    };
    join_within(&root, &format!("{provider_key}/{account_id}"))
}

/// Adopted legacy paths (e.g. Second) may live anywhere; they are validated
/// for absoluteness only and never moved. New dirs should use
/// [`managed_account_dir`].
pub fn require_absolute(path: &Path) -> ManagerResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "Use an absolute path or ~/path, got '{}'",
            path.display()
        ))
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Strip supported Win32/NT prefixes to the ordinary drive/UNC spelling so
/// `Path::is_absolute` sees them as absolute. The NT object-manager spelling
/// (`\??\C:\x`, `\??\UNC\server\share`) parses as rooted-but-prefixless, so a
/// raw `is_absolute` check rejects it even though `lowered` compares it equal.
/// Genuinely relative (`foo\bar`) and root-relative (`\foo\bar`) spellings
/// match no prefix below and stay non-absolute, so they are still rejected.
fn without_nt_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    let lower = text.to_lowercase();
    // Both UNC prefixes (`\\?\UNC\`, `\??\UNC\`) are 8 ASCII bytes; both
    // device prefixes (`\\?\`, `\??\`) are 4 ASCII bytes, so slicing `text`
    // at the same offset is safe.
    const UNC_PREFIX_LEN: usize = 8;
    const DEVICE_PREFIX_LEN: usize = 4;
    if lower.starts_with("\\\\?\\unc\\") || lower.starts_with("\\??\\unc\\") {
        PathBuf::from(format!("\\\\{}", &text[UNC_PREFIX_LEN..]))
    } else if lower.starts_with("\\\\?\\") || lower.starts_with("\\??\\") {
        PathBuf::from(text[DEVICE_PREFIX_LEN..].to_string())
    } else {
        PathBuf::from(text)
    }
}

fn lowered(path: &Path) -> String {
    // Normalize both slash styles (imported `C:/` paths) and extended-length
    // plus NT-object-manager prefixes (`\\?\C:\x`, `\\?\UNC\server\share`,
    // `\??\C:\x`, `\??\UNC\server\share` as returned by `read_link` for
    // junctions) before comparing, so all spellings of one path compare equal.
    let mut text = normalize_lexical(path)
        .to_string_lossy()
        .replace('/', "\\")
        .to_lowercase();
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

/// True when `candidate` is inside `root` (lexical, case-insensitive for the
/// Windows filesystem). Relative candidates are never inside.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    let root_norm = without_nt_prefix(root);
    let candidate_norm = without_nt_prefix(candidate);
    if !root_norm.is_absolute() || !candidate_norm.is_absolute() {
        return false;
    }
    let root = lowered(&root_norm);
    let candidate = lowered(&candidate_norm);
    if candidate == root {
        return true;
    }
    candidate.len() > root.len()
        && candidate.starts_with(root.as_str())
        && (root.ends_with('\\') || candidate.as_bytes()[root.len()] == b'\\')
}

/// Join a relative path onto `root`, refusing absolute paths, `..` escapes,
/// and joins that would leave `root`.
pub fn join_within(root: &Path, relative: &str) -> ManagerResult<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(format!("Unsafe relative path '{relative}'"));
    }
    let joined = root.join(relative_path);
    if !is_within(root, &joined) {
        return Err(format!("Path escapes its root: '{relative}'"));
    }
    Ok(joined)
}

/// Create a directory and parents. Refuses through reparse points: when any
/// existing ancestor is a junction/symlink, creating content there could land
/// outside the intended root.
pub fn ensure_dir(path: &Path) -> ManagerResult<()> {
    if let Some(reparse) = first_reparse_point(path) {
        return Err(format!(
            "Refusing to create '{}': '{}' is a link",
            path.display(),
            reparse.display()
        ));
    }
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Unable to create '{}': {error}", path.display()))
}

/// True when `path` itself is a symlink/junction (never follows the link).
pub fn is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}

/// First reparse point on `path` or any existing ancestor, if any. Backup and
/// delete must use this to avoid traversing shared-skills links or junctions.
pub fn first_reparse_point(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if is_reparse_point(candidate) {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

/// Read a link target without following it (`read_link` never traverses).
pub fn read_link_target(link: &Path) -> ManagerResult<PathBuf> {
    if !is_reparse_point(link) {
        return Err(format!("Not a link: '{}'", link.display()));
    }
    std::fs::read_link(link).map_err(|error| format!("Unable to read link: {error}"))
}

/// True when a link's target stays inside one of the allowlisted roots.
/// Relative targets resolve against the link's parent directory.
pub fn link_target_within(link: &Path, allowed_roots: &[PathBuf]) -> ManagerResult<bool> {
    let target = read_link_target(link)?;
    let resolved = if target.is_absolute() {
        target
    } else {
        link.parent()
            .map(|parent| parent.join(&target))
            .ok_or("Link has no parent directory")?
    };
    Ok(allowed_roots
        .iter()
        .any(|root| is_within(root, &resolved) || lowered(&resolved) == lowered(root)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_rejects_escapes_and_absolute_paths() {
        let root = PathBuf::from("C:\\UsageHub\\profiles");
        assert!(join_within(&root, "claude\\acc_1").is_ok());
        assert!(join_within(&root, "..\\other").is_err());
        assert!(join_within(&root, "C:\\Windows").is_err());
        assert!(join_within(&root, "\\\\server\\share").is_err());
        assert!(!is_within(&root, Path::new("C:\\UsageHub\\other")));
        assert!(is_within(
            &root,
            Path::new("C:\\UsageHub\\profiles\\claude")
        ));
    }
    #[test]
    fn slash_styles_and_extended_prefixes_compare_equal() {
        let backslash = PathBuf::from("C:\\UsageHub\\profiles");
        let slash = PathBuf::from("C:/UsageHub/profiles");
        let extended = PathBuf::from("\\\\?\\C:\\UsageHub\\profiles");
        let nt = PathBuf::from("\\??\\C:\\UsageHub\\profiles");
        let unc = PathBuf::from("\\\\server\\share\\lib");
        let nt_unc = PathBuf::from("\\??\\UNC\\server\\share\\lib");
        let extended_unc = PathBuf::from("\\\\?\\UNC\\server\\share\\lib");
        let child = PathBuf::from("C:/UsageHub/profiles/claude/acc_1");
        // All spellings of the root contain the child and each other.
        for root in [&backslash, &slash, &extended, &nt] {
            assert!(is_within(root, &child), "{}", root.display());
            assert!(is_within(&child, root) == false, "{}", root.display());
        }
        assert!(is_within(&backslash, &slash));
        assert!(is_within(&slash, &extended));
        assert!(is_within(&extended, &backslash));
        assert!(is_within(&backslash, &nt));
        assert!(is_within(&nt, &backslash));
        assert!(is_within(&unc, &nt_unc));
        assert!(is_within(&nt_unc, &extended_unc));
        assert!(is_within(&extended_unc, &unc));
        // Sibling with a shared text prefix is still outside.
        assert!(!is_within(
            &backslash,
            Path::new("C:/UsageHub/profiles-other")
        ));
        assert!(!is_within(&backslash, Path::new("C:\\UsageHub\\profiles2")));
        // Relative and root-relative spellings are never inside, even when
        // the text after the root matches.
        assert!(!is_within(&backslash, Path::new("UsageHub\\profiles")));
        assert!(!is_within(&backslash, Path::new("\\UsageHub\\profiles")));
        assert!(!is_within(
            &backslash,
            Path::new("\\UsageHub\\profiles\\claude\\acc_1")
        ));
        assert!(!is_within(Path::new("\\UsageHub\\profiles"), &child));
    }

    #[test]
    fn test_roots_stay_inside_the_thread_fixture() {
        let config = fork_config_dir().expect("config fixture");
        let local = fork_local_dir().expect("local fixture");
        let profiles = profiles_root().expect("profiles fixture");
        let temp = std::env::temp_dir();
        for dir in [&config, &local, &profiles] {
            assert!(is_within(&temp, dir), "{}", dir.display());
        }
        assert!(is_within(&config, &local) || local.starts_with(&config));
        assert!(is_within(&local, &profiles));
        assert_eq!(
            config,
            crate::app_settings::app_data_directory(),
            "config fixture must reuse the per-thread settings dir"
        );
        // Fixture roots must never resolve to the real known-folder data dirs.
        let real_roaming =
            crate::known_folders::roaming_app_data_dir().map(|dir| dir.join(FORK_CONFIG_NAMESPACE));
        let real_local =
            crate::known_folders::local_app_data_dir().map(|dir| dir.join(FORK_CONFIG_NAMESPACE));
        assert_ne!(Some(config.clone()), real_roaming);
        assert_ne!(Some(local.clone()), real_local);
        assert!(shared_roots_avoid_production(&config));
    }

    #[cfg(test)]
    fn shared_roots_avoid_production(config: &Path) -> bool {
        // Guard against the classic `C:\Users\<name>\AppData` production
        // prefix: the temp fixture must not live under either real root.
        let text = config.to_string_lossy().to_lowercase();
        !text.contains("\\usagehub\\skills-library") || text.contains("usagehub-test-")
    }

    #[test]
    fn managed_dirs_reject_unsafe_ids() {
        assert!(managed_account_dir(ProviderId::Claude, "acc_1").is_ok());
        assert!(managed_account_dir(ProviderId::Claude, "..").is_err());
        assert!(managed_account_dir(ProviderId::Claude, "a/b").is_err());
        assert!(backup_dir_for(ProviderId::Codex, "work").is_ok());
        assert!(backup_dir_for(ProviderId::Codex, "").is_err());
        // Managed roots stay inside the thread fixture even for providers.
        let dir = managed_account_dir(ProviderId::Claude, "acc_1").unwrap();
        assert!(is_within(&std::env::temp_dir(), &dir));
    }
}
