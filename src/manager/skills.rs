//! Shared skills-library helpers.
//!
//! Only skills are shared between accounts; history, config, and auth stay
//! isolated. The library never stores auth material (no tokens, credential
//! files, or credential targets), which is why its `%APPDATA%` default root
//! is approved. Per-account skill links point into the shared library.
//! Cleanup owns the link inode but never the target: a validated account
//! link is `owned = true` for the inode and is removed with
//! [`remove_link_only`], which never traverses the target.
//! Backup and delete must never follow a link: preserved links are excluded
//! from cleanup plans instead of being listed unowned.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manager::paths;
use crate::manager::ManagerResult;
use crate::providers::ProviderId;

// Win32 handle bindings for native junction creation. The shared `windows`
// crate declarations are used instead of a second manual `extern "system"`
// block, so no `clashing_extern_declarations` warning against the backup
// probe's raw declaration.
#[cfg(windows)]
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_MODE,
            OPEN_EXISTING,
        },
        System::IO::DeviceIoControl,
    },
};

/// Shared library root: `%APPDATA%/UsageHub/skills-library`.
pub fn shared_skills_dir() -> Option<PathBuf> {
    paths::fork_config_dir().map(|dir| dir.join(paths::SKILLS_LIBRARY_DIR_NAME))
}

/// Ensure the shared library exists and return it. Refuses through links.
pub fn ensure_shared_skills_dir() -> ManagerResult<PathBuf> {
    let dir = shared_skills_dir().ok_or("Unable to resolve the skills library")?;
    paths::ensure_dir(&dir)?;
    Ok(dir)
}

/// Per-account skills dir: `<profiles>/<provider>/<account_id>/skills`.
/// Adopted legacy paths use `<desktop_or_config_dir>/skills` instead; the
/// caller passes the explicit base.
pub fn account_skills_dir(provider: ProviderId, account_id: &str) -> ManagerResult<PathBuf> {
    paths::join_within(&paths::managed_account_dir(provider, account_id)?, "skills")
}

/// Sanitize a skill name for use as a link file name.
pub fn sanitize_skill_name(name: &str) -> ManagerResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!("Unsafe skill name '{name}'"));
    }
    Ok(trimmed.to_string())
}

/// Expected link path for a skill inside an account skills dir.
pub fn skill_link_path(account_skills: &Path, skill_name: &str) -> ManagerResult<PathBuf> {
    let name = sanitize_skill_name(skill_name)?;
    paths::join_within(account_skills, &name)
}

/// Validate that `link` is a link whose target stays inside `allowed_roots`
/// (the shared library root). Returns the raw target for metadata matching.
pub fn validate_skill_link(link: &Path, allowed_roots: &[PathBuf]) -> ManagerResult<PathBuf> {
    if paths::first_reparse_point(link).as_deref() != Some(link) {
        return Err(format!("Not a direct link: '{}'", link.display()));
    }
    if !paths::link_target_within(link, allowed_roots)? {
        return Err(format!(
            "Skill link '{}' points outside the shared library",
            link.display()
        ));
    }
    paths::read_link_target(link)
}

/// Remove a skill link itself without touching its target. Refuses unless the
/// path is a direct link (junction/symlink); ordinary files and directories
/// are never removed by this helper.
pub fn remove_link_only(link: &Path) -> ManagerResult<()> {
    if paths::first_reparse_point(link).as_deref() != Some(link) {
        return Err(format!("Refusing to remove non-link '{}'", link.display()));
    }
    // Classify the link inode itself via `symlink_metadata` (never follows a
    // dangling target): directory junctions/symlinks carry
    // `FILE_ATTRIBUTE_DIRECTORY`, file symlinks do not. Try the matching
    // removal first, then the other spelling, so dangling links in either
    // form are still unlinked without ever traversing the target.
    let is_dir_link = std::fs::symlink_metadata(link)
        .map(|metadata| {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes() & paths::FILE_ATTRIBUTE_DIRECTORY_FOR_LINK != 0
        })
        .unwrap_or(false);
    let first = if is_dir_link {
        std::fs::remove_dir(link)
    } else {
        std::fs::remove_file(link)
    };
    if first.is_ok() {
        return Ok(());
    }
    let fallback = if is_dir_link {
        std::fs::remove_file(link)
    } else {
        std::fs::remove_dir(link)
    };
    match (first, fallback) {
        (Ok(()), _) | (_, Ok(())) => Ok(()),
        (Err(first_error), Err(second_error)) => Err(format!(
            "Unable to remove link '{}': {first_error} / {second_error}",
            link.display()
        )),
    }
}

/// What one importable skill entry looks like against the shared library.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillImportAction {
    /// Not in the library yet; import copies it in.
    New,
    /// Same bytes already present; import skips it.
    Unchanged,
    /// Name taken by different bytes; import skips unless `replace` is set.
    Conflict,
}

/// One skill entry previewed (or applied) by an import run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillImportEntry {
    pub skill_name: String,
    pub action: SkillImportAction,
    pub source_path: PathBuf,
    pub target_path: PathBuf,
}

/// Outcome of [`import_skills_from`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillImportReport {
    pub skill_names: Vec<String>,
    pub imported: usize,
    pub overwritten: usize,
    pub skipped_unchanged: usize,
    pub skipped_conflicts: Vec<String>,
}

/// Ensure `<base>/skills` is the deliberate shared-library junction.
///
/// - Missing entry: created as a native mount-point junction (no shell, no
///   privilege needed, unlike symlinks) pointing at the shared library.
/// - Existing direct link: validated against the library root; a link that
///   points elsewhere is an error, never retargeted or followed.
/// - Existing real directory: preserved as-is (user skills stay put); the
///   caller can run [`preview_skills_import`] / [`import_skills_from`] to
///   share them explicitly with conflict preview.
pub fn ensure_account_skills_junction(base: &Path) -> ManagerResult<PathBuf> {
    let library = ensure_shared_skills_dir()?;
    let link = paths::join_within(&paths::require_absolute(base)?, "skills")?;
    match std::fs::symlink_metadata(&link) {
        Err(_) => create_junction(&library, &link)?,
        Ok(_) => {
            if paths::first_reparse_point(&link).as_deref() != Some(link.as_path()) {
                return Ok(link);
            }
            validate_skill_link(&link, std::slice::from_ref(&library))?;
        }
    }
    Ok(link)
}

/// Remove an account `skills` junction itself without touching the library.
/// Refuses when the entry is not a direct link (a real user-skills dir is
/// never deleted here; that belongs to the cleanup slice with a verified
/// backup). Dangling junctions unlink fine: the target is never traversed.
pub fn unlink_account_skills_junction(base: &Path) -> ManagerResult<()> {
    let link = paths::join_within(&paths::require_absolute(base)?, "skills")?;
    remove_link_only(&link)
}

/// Create a directory junction `link` -> `target` with no shell.
///
/// Native `DeviceIoControl(FSCTL_SET_REPARSE_POINT)` mount-point creation:
/// `cmd.exe` is never spawned, so `& % ^ !` (cmd metachars / env expansion),
/// spaces, Unicode, and either slash style stay literal in both paths. The
/// exact paths are passed as wide strings; nothing is re-spelled, quoted,
/// or expanded, and no unsafe path is silently altered. Backup's directory
/// fallback should reuse this helper (`skills::create_junction(target,
/// link)` — note target-first order) instead of building a second `mklink`
/// escaping convention.
pub fn create_junction(target: &Path, link: &Path) -> ManagerResult<()> {
    if let Some(parent) = link.parent() {
        paths::ensure_dir(parent)?;
    }
    #[cfg(windows)]
    create_junction_native(target, link)?;
    #[cfg(not(windows))]
    {
        let _ = (target, link);
        return Err("Skills junctions require Windows".into());
    }
    // Confirm the inode really is a link.
    if paths::first_reparse_point(link).as_deref() != Some(link) {
        return Err(format!(
            "Skills junction '{}' was not created as a link",
            link.display()
        ));
    }
    Ok(())
}

/// NT mount-point names for `target`: `(substitute, print)` as UTF-16 units
/// without NULs. The substitute name carries the NT prefix (`\??\` for local
/// volumes, `\??\UNC\` for UNC shares); the print name is the Win32 spelling.
/// Wide-native throughout, so Unicode (and `& % ^ !`, spaces, either slash
/// style) stays literal. Refuses non-absolute targets instead of silently
/// anchoring a relative path.
#[cfg(windows)]
fn nt_mount_names(target: &Path) -> ManagerResult<(Vec<u16>, Vec<u16>)> {
    use std::os::windows::ffi::OsStrExt;
    // Canonical spelling first (resolves `.`/`..`, slash style, and any
    // `\\?\` normalization the OS applies); the given absolute spelling still
    // forms a valid buffer when the target does not exist yet.
    let absolute = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    if !absolute.is_absolute() {
        return Err(format!(
            "Skills junction target must be absolute, got '{}'",
            target.display()
        ));
    }
    const EXTENDED_PREFIX: &[u16] = &[0x5C, 0x5C, 0x3F, 0x5C]; // `\\?\`
    const NT_PREFIX: &[u16] = &[0x5C, 0x3F, 0x3F, 0x5C]; // `\??\`
    const UNC_INFIX: &[u16] = &[0x55, 0x4E, 0x43, 0x5C]; // `UNC\`
    const UNC_PREFIX: &[u16] = &[0x5C, 0x5C]; // `\\`
    let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
    let inner: &[u16] = wide.strip_prefix(EXTENDED_PREFIX).unwrap_or(&wide);
    let (substitute, print) = if let Some(body) = inner.strip_prefix(NT_PREFIX) {
        // Already an NT spelling: keep the substitute verbatim; the print
        // name drops the prefix (`\??\UNC\` maps back to `\\`).
        let print = match body.strip_prefix(UNC_INFIX) {
            Some(rest) => [UNC_PREFIX, rest].concat(),
            None => body.to_vec(),
        };
        (inner.to_vec(), print)
    } else if let Some(rest) = inner.strip_prefix(UNC_INFIX) {
        // Extended `\\?\UNC\server\share\...` (canonical UNC spelling):
        // substitute `\??\UNC\...`, print `\\server\share\...`.
        (
            [NT_PREFIX, UNC_INFIX, rest].concat(),
            [UNC_PREFIX, rest].concat(),
        )
    } else if let Some(rest) = inner.strip_prefix(UNC_PREFIX) {
        // Bare UNC `\\server\share\...`: substitute `\??\UNC\...`.
        ([NT_PREFIX, UNC_INFIX, rest].concat(), inner.to_vec())
    } else {
        // Local volume (or an already-plain absolute spelling).
        ([NT_PREFIX, inner].concat(), inner.to_vec())
    };
    if substitute.is_empty() || print.is_empty() {
        return Err(format!(
            "Skills junction target must be absolute, got '{}'",
            target.display()
        ));
    }
    Ok((substitute, print))
}

/// Native mount-point junction creation (Windows only, no shell).
#[cfg(windows)]
fn create_junction_native(target: &Path, link: &Path) -> ManagerResult<()> {
    use std::os::windows::ffi::OsStrExt;
    // Wide (UTF-16, NUL-terminated) spelling of one path, passed verbatim.
    fn wide_null(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain([0]).collect()
    }
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    /// SDK `MAXIMUM_REPARSE_DATA_BUFFER_SIZE`: the whole `REPARSE_DATA_BUFFER`
    /// (8-byte header plus mount-point section) must fit in 16 KiB — far
    /// tighter than a bare `u16` range check.
    const MAXIMUM_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;
    /// `GENERIC_WRITE` for the link handle: the mount point is written, never
    /// read back, through `CreateFileW`.
    const LINK_ACCESS_WRITE: u32 = 0x4000_0000;
    let (substitute, print) = nt_mount_names(target)?;
    let sub_bytes = substitute
        .len()
        .checked_mul(2)
        .ok_or("Skills junction target too long")?;
    let print_bytes = print
        .len()
        .checked_mul(2)
        .ok_or("Skills junction target too long")?;
    // Mount-point `REPARSE_DATA_BUFFER` section (MS-FSCC 2.1.2.1): 8 bytes of
    // offsets/lengths, then both names NUL-terminated. Lengths exclude their
    // NUL; `PrintNameOffset` starts past the substitute NUL.
    // `ReparseDataLength` covers this section only; the 8-byte header
    // (tag/length/reserved) rides in the total buffer but outside it. The old
    // layout omitted both NULs and mis-based the print offset, which the
    // filesystem rejects with `ERROR_INVALID_REPARSE_DATA` (4392).
    let section_len = 8usize
        .checked_add(sub_bytes + 2)
        .and_then(|sum| sum.checked_add(print_bytes + 2))
        .ok_or("Skills junction target too long")?;
    let total_len = section_len
        .checked_add(8)
        .ok_or("Skills junction target too long")?;
    if total_len > MAXIMUM_REPARSE_DATA_BUFFER_SIZE {
        return Err("Skills junction target too long".into());
    }
    let print_offset = sub_bytes + 2;
    let mut buffer = Vec::with_capacity(total_len);
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&(section_len as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buffer.extend_from_slice(&(sub_bytes as u16).to_le_bytes()); // SubstituteNameLength, no NUL
    buffer.extend_from_slice(&(print_offset as u16).to_le_bytes()); // PrintNameOffset
    buffer.extend_from_slice(&(print_bytes as u16).to_le_bytes()); // PrintNameLength, no NUL
    for unit in &substitute {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Substitute NUL
    for unit in &print {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Print NUL
    debug_assert_eq!(buffer.len(), total_len);
    // Fresh mount point: the link dir must not exist yet (the caller ensures a
    // missing entry); refuse anything else rather than repurposing it.
    if let Err(error) = std::fs::create_dir(link) {
        return Err(format!(
            "Unable to create the skills junction '{}': {error}",
            link.display()
        ));
    }
    let link_wide = wide_null(link);
    // SAFETY: Win32 handle API with a valid NUL-terminated wide path; the
    // handle is always closed and the buffer outlives the call.
    let failed: Option<u32> = unsafe {
        match CreateFileW(
            PCWSTR::from_raw(link_wide.as_ptr()),
            LINK_ACCESS_WRITE,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        ) {
            Err(_) => Some(GetLastError().0),
            Ok(handle) => {
                let mut returned: u32 = 0;
                let failed = DeviceIoControl(
                    handle,
                    FSCTL_SET_REPARSE_POINT,
                    Some(buffer.as_ptr() as *const std::ffi::c_void),
                    buffer.len() as u32,
                    None,
                    0,
                    Some(&mut returned as *mut u32),
                    None,
                )
                .err()
                .map(|_| GetLastError().0);
                let _ = CloseHandle(handle);
                failed
            }
        }
    };
    if let Some(code) = failed {
        // Only the directory this helper just created is removed; a
        // pre-existing entry was never repurposed (`create_dir` refused it).
        std::fs::remove_dir(link).ok();
        return Err(format!(
            "Unable to create the skills junction '{}' (os error {code})",
            link.display()
        ));
    }
    Ok(())
}

/// Credential/session filenames that must never enter the shared library,
/// at any depth. Compared case-insensitively (Windows filesystems are).
/// This is a fixed denylist, not a content secret scanner: no file content
/// is ever inspected for secrets.
const BLOCKED_SKILL_FILES: &[&str] = &[
    ".credentials.json",
    "auth.json",
    "local state",
    "cookies",
    ".env",
];

/// Canonical spelling for root-overlap checks: backslashes, lowercase,
/// Win32/NT prefixes stripped. Mirrors `lifecycle::canonical_spelling`
/// (kept local so this module never depends on the lifecycle slice).
fn canonical_spelling(path: &Path) -> String {
    let mut spelling = path.to_string_lossy().replace('/', "\\").to_lowercase();
    for prefix in ["\\\\?\\unc\\", "\\??\\unc\\", "\\\\?\\", "\\??\\"] {
        if let Some(rest) = spelling.strip_prefix(prefix) {
            spelling = if prefix.ends_with("unc\\") {
                format!("\\\\{rest}")
            } else {
                rest.to_string()
            };
            break;
        }
    }
    spelling.trim_end_matches('\\').to_string()
}

fn overlaps(canon: &str, other: &str) -> bool {
    canon == other
        || canon.starts_with(&format!("{other}\\"))
        || other.starts_with(&format!("{canon}\\"))
}

/// Every registered account desktop/config dir (both providers): the only
/// parents a migratable `skills` leaf may sit under, besides the canonical
/// Default CLI config roots. Read-only registry load.
fn registered_skill_home_bases() -> Vec<PathBuf> {
    let settings = crate::manager::state::load_fresh();
    let mut homes = Vec::new();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let Some(accounts) = settings.accounts.get(provider) else {
            continue;
        };
        for profile in &accounts.profiles {
            for field in [&profile.desktop_dir, &profile.config_dir] {
                if !field.trim().is_empty() {
                    homes.push(PathBuf::from(field.trim()));
                }
            }
        }
    }
    homes
}

/// Fail-closed gate for any user-supplied skills-import source. Refuses:
/// filesystem roots and links; broad parents (home/app-data and ancestors);
/// fork-managed trees; canonical CLI/Desktop roots; anything that looks
/// like an account or config root itself (desktop markers or top-level
/// credential files); and any candidate tree containing a blocked
/// credential/session filename at any depth. No auth/config/history can
/// ever leak into the shared library and from there to all accounts.
fn validate_skill_source(source: &Path) -> ManagerResult<PathBuf> {
    let absolute = paths::require_absolute(source)?;
    if absolute
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
    {
        return Err("Refusing to import a filesystem root".into());
    }
    if std::fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.is_symlink()) {
        return Err(format!(
            "Refusing to import a link '{}': point at its target instead",
            absolute.display()
        ));
    }
    if !absolute.is_dir() {
        return Err(format!(
            "Refusing '{}': import a skill directory (with SKILL.md) or a skills root, not a file",
            absolute.display()
        ));
    }
    let canon = canonical_spelling(&absolute);
    for dir in [
        crate::known_folders::home_dir(),
        crate::known_folders::roaming_app_data_dir(),
        crate::known_folders::local_app_data_dir(),
    ] {
        if let Some(dir) = dir {
            let root = canonical_spelling(&dir);
            if canon == root || root.starts_with(&format!("{canon}\\")) {
                return Err(format!(
                    "Refusing broad parent root '{}': point at a narrow skill directory instead",
                    absolute.display()
                ));
            }
        }
    }
    // Narrow exemption for the exact migration shape: a directory NAMED
    // `skills` whose parent is a known registered desktop/config root or a
    // canonical Default CLI config root is the account's real skills home
    // (`<Second>/skills`, `<Second>/claude-config/skills`,
    // `~/.claude/skills`), not an account root. Computed before the overlap
    // checks so the leaf's purposeful descent into those roots is allowed.
    let leaf_name = absolute
        .file_name()
        .map(|stem| stem.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let known_leaf = leaf_name == "skills" && {
        let parent = absolute.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut homes = registered_skill_home_bases();
        if let Some(home) = crate::known_folders::home_dir() {
            homes.push(home.join(".claude"));
            homes.push(home.join(".codex"));
        }
        homes
            .iter()
            .any(|home| canonical_spelling(&parent) == canonical_spelling(home))
    };
    let mut managed = vec![
        paths::fork_config_dir(),
        paths::fork_local_dir(),
        paths::profiles_root(),
        paths::fork_backups_dir(),
        shared_skills_dir(),
    ];
    if let Some(home) = crate::known_folders::home_dir() {
        managed.push(Some(home.join(".claude")));
        managed.push(Some(home.join(".codex")));
    }
    if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
        managed.push(Some(roaming.join("Claude")));
    }
    for var in ["CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        if let Some(value) = std::env::var_os(var) {
            if !value.is_empty() {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    managed.push(Some(path));
                }
            }
        }
    }
    for root in managed.into_iter().flatten() {
        let spell = canonical_spelling(&root);
        // Protected roots refuse equality and ancestors unconditionally (a
        // source must never BE or CONTAIN a protected root). A known skills
        // leaf is a purposeful descendant, which is the one allowed shape.
        let refused = if known_leaf {
            canon == spell || spell.starts_with(&format!("{canon}\\"))
        } else {
            overlaps(&canon, &spell)
        };
        if refused {
            return Err(format!(
                "Refusing '{}': it is a UsageHub, provider, or account root, not a skill source",
                absolute.display()
            ));
        }
    }
    // A directory shaped like an account/config root is never a skill
    // source, wherever it lives: desktop trees (config.json plus session
    // state) and CLI config dirs (top-level credential files). The known
    // skills leaf is exempt: its parent IS such a root by construction,
    // and the purposeful descent was already allowed above.
    if !known_leaf {
        let looks_like_desktop = absolute.join("config.json").is_file()
            && (absolute.join("Local State").is_file()
                || absolute.join("Preferences").is_file()
                || absolute.join(".claude.json").is_file()
                || absolute.join("projects").is_dir());
        let looks_like_cli_config =
            absolute.join(".credentials.json").is_file() || absolute.join("auth.json").is_file();
        if looks_like_desktop || looks_like_cli_config {
            return Err(format!(
                "Refusing '{}': it looks like an account/config root, not a skill source",
                absolute.display()
            ));
        }
    }
    // A bare directory NAMED `skills` is only ever the account skills home:
    // outside a known registered or canonical parent it is refused rather
    // than treated as a generic collection.
    if leaf_name == "skills" && !known_leaf {
        return Err(format!(
            "Refusing '{}': not a recognized account skills home",
            absolute.display()
        ));
    }
    reject_blocked_content(&absolute)?;
    Ok(absolute)
}

/// Walk the whole candidate tree and refuse any blocked credential/session
/// filename at any depth. Links are refused, never followed. Iterator
/// errors propagate: a half-walked tree must never report clean.
fn reject_blocked_content(root: &Path) -> ManagerResult<()> {
    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        if std::fs::symlink_metadata(&path)
            .map(|meta| meta.is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "Refusing to follow source link '{}'",
                path.display()
            ));
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if BLOCKED_SKILL_FILES.contains(&name.as_str()) {
            return Err(format!(
                "Refusing '{}': credential/session files ({}) must never enter the shared library",
                path.display(),
                name
            ));
        }
        if path.is_dir() {
            reject_blocked_content(&path)?;
        }
    }
    Ok(())
}

/// Sorted directory listing with propagated errors: a half-read directory
/// must never preview or import as if it were fully seen.
fn read_dir_sorted(dir: &Path) -> ManagerResult<Vec<std::fs::DirEntry>> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(dir)
        .map_err(|error| format!("Unable to read '{}': {error}", dir.display()))?;
    for entry in read {
        entries
            .push(entry.map_err(|error| format!("Unable to read '{}': {error}", dir.display()))?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

/// Select the importable skills inside a validated source.
///
/// - The source itself carries `SKILL.md`: it is one single skill, named
///   by its directory name.
/// - Otherwise the source is a skills root: each direct child *directory*
///   carrying `SKILL.md` is one skill. Anything else (root READMEs, stray
///   files, skill-less dirs) is ignored, never imported.
fn collect_skill_candidates(source: &Path) -> ManagerResult<Vec<(String, PathBuf)>> {
    if source.join("SKILL.md").is_file() {
        let raw = source
            .file_name()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = sanitize_skill_name(&raw)?;
        return Ok(vec![(name, source.to_path_buf())]);
    }
    let mut candidates = Vec::new();
    for entry in read_dir_sorted(source)? {
        let path = entry.path();
        if std::fs::symlink_metadata(&path)
            .map(|meta| meta.is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "Refusing to follow source link '{}'",
                path.display()
            ));
        }
        if path.is_dir() && path.join("SKILL.md").is_file() {
            let raw = entry.file_name().to_string_lossy().to_string();
            candidates.push((sanitize_skill_name(&raw)?, path));
        }
    }
    Ok(candidates)
}

/// Preview importing skills from `source` into the shared library.
/// Read-only: nothing is copied and the library dir is never created —
/// the lookup uses [`shared_skills_dir`], not the ensuring variant, so a
/// preview on a fresh machine leaves no trace. Only single skill dirs
/// (`SKILL.md`) or skills roots (child dirs with `SKILL.md`) are accepted;
/// account/config roots and credential/session files are refused by
/// [`validate_skill_source`].
pub fn preview_skills_import(source: &Path) -> ManagerResult<Vec<SkillImportEntry>> {
    let source = validate_skill_source(source)?;
    let library = shared_skills_dir().ok_or("Unable to resolve the skills library")?;
    let mut preview = Vec::new();
    for (name, source_path) in collect_skill_candidates(&source)? {
        let target_path = skill_link_path(&library, &name)?;
        let action = if std::fs::symlink_metadata(&target_path).is_err() {
            SkillImportAction::New
        } else {
            match skill_trees_equal(&source_path, &target_path) {
                Ok(true) => SkillImportAction::Unchanged,
                // Unknown bytes (unreadable target): Conflict is the safe
                // side — skipped without `replace`, never silently kept.
                Ok(false) | Err(_) => SkillImportAction::Conflict,
            }
        };
        preview.push(SkillImportEntry {
            skill_name: name,
            action,
            source_path,
            target_path,
        });
    }
    preview.sort_by(|a, b| a.skill_name.cmp(&b.skill_name));
    Ok(preview)
}

/// Copy `source` skills into the shared library.
///
/// - `New` entries are copied in; `Unchanged` entries are skipped.
/// - `Conflict` entries are skipped unless `replace` is set, in which case
///   the library entry is removed first (link-inode only when it is a link)
///   and the source bytes take its place.
/// Existing library content is never modified except by an explicit
/// `replace` conflict overwrite. The library dir is created only when
/// there is something to copy, so empty/no-op imports leave no trace.
pub fn import_skills_from(source: &Path, replace: bool) -> ManagerResult<SkillImportReport> {
    let preview = preview_skills_import(source)?;
    let mut report = SkillImportReport {
        skill_names: Vec::new(),
        imported: 0,
        overwritten: 0,
        skipped_unchanged: 0,
        skipped_conflicts: Vec::new(),
    };
    if preview.is_empty() {
        return Ok(report);
    }
    ensure_shared_skills_dir()?;
    for entry in preview {
        report.skill_names.push(entry.skill_name.clone());
        match entry.action {
            SkillImportAction::Unchanged => report.skipped_unchanged += 1,
            SkillImportAction::Conflict if !replace => {
                report.skipped_conflicts.push(entry.skill_name.clone());
            }
            SkillImportAction::New | SkillImportAction::Conflict => {
                if entry.action == SkillImportAction::Conflict {
                    remove_tree(&entry.target_path)?;
                }
                copy_tree(&entry.source_path, &entry.target_path)?;
                if entry.action == SkillImportAction::Conflict {
                    report.overwritten += 1;
                } else {
                    report.imported += 1;
                }
            }
        }
    }
    Ok(report)
}

/// True when two skill trees (file or dir) carry the same bytes. Read
/// errors propagate: an unreadable tree compares as unknown (the caller
/// treats that as `Conflict`), never as silently equal.
fn skill_trees_equal(first: &Path, second: &Path) -> ManagerResult<bool> {
    let first_meta = std::fs::symlink_metadata(first)
        .map_err(|error| format!("Unable to read '{}': {error}", first.display()))?;
    let second_meta = std::fs::symlink_metadata(second)
        .map_err(|error| format!("Unable to read '{}': {error}", second.display()))?;
    if first_meta.is_symlink() || second_meta.is_symlink() {
        return Err(format!(
            "Refusing to compare through a link ('{}' vs '{}')",
            first.display(),
            second.display()
        ));
    }
    if first_meta.is_file() && second_meta.is_file() {
        return files_equal_streaming(first, second);
    }
    if first_meta.is_dir() && second_meta.is_dir() {
        return dir_trees_equal(first, second);
    }
    Ok(false)
}

/// Bounded streaming byte compare (8 KiB chunks): file size is never loaded
/// into RAM, however large a skill resource grows.
fn files_equal_streaming(first: &Path, second: &Path) -> ManagerResult<bool> {
    use std::io::Read;
    let file_a = std::fs::File::open(first)
        .map_err(|error| format!("Unable to read '{}': {error}", first.display()))?;
    let file_b = std::fs::File::open(second)
        .map_err(|error| format!("Unable to read '{}': {error}", second.display()))?;
    if file_a.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX)
        != file_b.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX)
    {
        return Ok(false);
    }
    let mut reader_a = std::io::BufReader::with_capacity(8192, file_a);
    let mut reader_b = std::io::BufReader::with_capacity(8192, file_b);
    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let len_a = reader_a
            .read(&mut buf_a)
            .map_err(|error| format!("Unable to read '{}': {error}", first.display()))?;
        let len_b = reader_b
            .read(&mut buf_b)
            .map_err(|error| format!("Unable to read '{}': {error}", second.display()))?;
        if len_a != len_b || buf_a[..len_a] != buf_b[..len_b] {
            return Ok(false);
        }
        if len_a == 0 {
            return Ok(true);
        }
    }
}

fn dir_trees_equal(first: &Path, second: &Path) -> ManagerResult<bool> {
    let first_entries = read_dir_sorted(first)?;
    let second_entries = read_dir_sorted(second)?;
    if first_entries.len() != second_entries.len() {
        return Ok(false);
    }
    for (a, b) in first_entries.iter().zip(second_entries.iter()) {
        if a.file_name() != b.file_name() {
            return Ok(false);
        }
        if !skill_trees_equal(&a.path(), &b.path())? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Remove a library tree replaced by `replace` imports. Links unlink
/// inode-only (never traversing the target); plain files/dirs remove
/// directly. Refuses anything that is not inside the shared library root.
fn remove_tree(path: &Path) -> ManagerResult<()> {
    let Some(library) = shared_skills_dir() else {
        return Err("Unable to resolve the skills library".into());
    };
    if !paths::is_within(&library, path) && path != library {
        return Err(format!(
            "Refusing to remove '{}': outside the library",
            path.display()
        ));
    }
    if paths::first_reparse_point(path).as_deref() == Some(path) {
        return remove_link_only(path);
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| format!("Nothing to remove at '{}'", path.display()))?;
    if metadata.is_file() {
        std::fs::remove_file(path)
            .map_err(|error| format!("Unable to remove '{}': {error}", path.display()))?;
    } else if metadata.is_dir() {
        std::fs::remove_dir_all(path)
            .map_err(|error| format!("Unable to remove '{}': {error}", path.display()))?;
    } else {
        return Err(format!(
            "Refusing to remove special file '{}'",
            path.display()
        ));
    }
    Ok(())
}

/// Recursive copy for skill trees (files and dirs). No symlinks are
/// recreated from the source: a source link is an error, never followed.
fn copy_tree(source: &Path, target: &Path) -> ManagerResult<()> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("Unable to read '{}': {error}", source.display()))?;
    if metadata.is_symlink() {
        return Err(format!(
            "Refusing to follow source link '{}'",
            source.display()
        ));
    }
    if metadata.is_file() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Unable to create '{}': {error}", parent.display()))?;
        }
        std::fs::copy(source, target)
            .map_err(|error| format!("Unable to copy '{}': {error}", source.display()))?;
        return Ok(());
    }
    if metadata.is_dir() {
        std::fs::create_dir_all(target)
            .map_err(|error| format!("Unable to create '{}': {error}", target.display()))?;
        // Every entry error propagates: a half-copied skill must never
        // report success (false green).
        for entry in read_dir_sorted(source)? {
            let name = entry.file_name();
            copy_tree(&entry.path(), &target.join(name))?;
        }
        return Ok(());
    }
    Err(format!(
        "Refusing to copy special file '{}'",
        source.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_names_are_sanitized() {
        assert_eq!(sanitize_skill_name("my-skill_1").unwrap(), "my-skill_1");
        assert!(sanitize_skill_name("../escape").is_err());
        assert!(sanitize_skill_name("a/b").is_err());
        assert!(sanitize_skill_name("").is_err());
    }

    #[test]
    fn non_links_are_never_removed() {
        // A missing path is not a link; removal must be refused, not attempted.
        assert!(remove_link_only(Path::new("C:\\UsageHub\\no-such-link")).is_err());
        assert!(validate_skill_link(
            Path::new("C:\\UsageHub\\plain"),
            &[PathBuf::from("C:\\lib")]
        )
        .is_err());
    }
    /// Allowed shared link unlinks the inode without mutating the target.
    /// Skips when the environment cannot create links (no privilege).
    #[test]
    fn allowed_link_unlinks_without_target_mutation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let library = dir.path().join("library");
        let account = dir.path().join("account");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&account).unwrap();
        let target = library.join("my-skill");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("SKILL.md"), "# skill").unwrap();
        let link = account.join("my-skill");
        if std::os::windows::fs::symlink_dir(&target, &link).is_err() {
            // Link creation needs privilege/developer mode; nothing to assert.
            return;
        }
        // Proven link validates against the allowlisted library root.
        validate_skill_link(&link, &[library.clone()]).expect("allowed link");
        assert!(validate_skill_link(&link, &[account.clone()]).is_err());
        remove_link_only(&link).expect("inode unlink");
        assert!(!link.exists() && std::fs::symlink_metadata(&link).is_err());
        // Target content is untouched.
        assert_eq!(std::fs::read(target.join("SKILL.md")).unwrap(), b"# skill");
    }

    /// Dangling directory/file links unlink without following the target.
    /// Skips when the environment cannot create links (no privilege).
    #[test]
    fn dangling_links_unlink_without_traversal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let library = dir.path().join("library");
        let account = dir.path().join("account");
        std::fs::create_dir_all(&library).unwrap();
        std::fs::create_dir_all(&account).unwrap();
        // Dangling directory link: target never existed.
        let missing_dir = library.join("gone-dir");
        let dir_link = account.join("gone-dir");
        if std::os::windows::fs::symlink_dir(&missing_dir, &dir_link).is_err() {
            return;
        }
        // Still owned: `read_link` target stays inside the allowlisted root
        // even though the target is missing.
        validate_skill_link(&dir_link, &[library.clone()]).expect("dangling dir owned");
        remove_link_only(&dir_link).expect("dangling dir unlink");
        assert!(std::fs::symlink_metadata(&dir_link).is_err());
        // Dangling file link: points at a library file that was deleted.
        let file_target = library.join("note.md");
        std::fs::write(&file_target, "# note").unwrap();
        let file_link = account.join("note.md");
        let file_created = std::os::windows::fs::symlink_file(&file_target, &file_link).is_ok();
        std::fs::remove_file(&file_target).ok();
        if !file_created {
            return;
        }
        validate_skill_link(&file_link, &[library.clone()]).expect("dangling file owned");
        remove_link_only(&file_link).expect("dangling file unlink");
        assert!(std::fs::symlink_metadata(&file_link).is_err());
    }

    /// Import copies new skills, skips identical ones, and only overwrites
    /// conflicts with explicit `replace` — library bytes stay untouched
    /// otherwise.
    #[test]
    fn import_preview_conflict_and_replace_semantics() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("incoming");
        std::fs::create_dir_all(source.join("usagehub-test-import")).unwrap();
        std::fs::write(source.join("usagehub-test-import").join("SKILL.md"), "# v1").unwrap();

        let library = ensure_shared_skills_dir().expect("library root");
        let target = library.join("usagehub-test-import");
        let _ = remove_tree(&target);

        let preview = preview_skills_import(&source).expect("preview");
        let entry = preview
            .iter()
            .find(|entry| entry.skill_name == "usagehub-test-import")
            .expect("new skill previewed");
        assert_eq!(entry.action, SkillImportAction::New);

        let report = import_skills_from(&source, false).expect("import");
        assert_eq!(report.imported, 1);
        assert_eq!(std::fs::read(target.join("SKILL.md")).unwrap(), b"# v1");

        // Identical re-import is a no-op skip.
        let again = preview_skills_import(&source).expect("re-preview");
        assert!(again
            .iter()
            .all(|entry| entry.action != SkillImportAction::New
                || entry.skill_name != "usagehub-test-import"));
        let unchanged = import_skills_from(&source, false).expect("re-import");
        assert_eq!(unchanged.imported, 0);

        // Diverged source is a conflict: kept without `replace`.
        std::fs::write(source.join("usagehub-test-import").join("SKILL.md"), "# v2").unwrap();
        let conflicted = preview_skills_import(&source).expect("conflict preview");
        let entry = conflicted
            .iter()
            .find(|entry| entry.skill_name == "usagehub-test-import")
            .expect("conflict previewed");
        assert_eq!(entry.action, SkillImportAction::Conflict);
        let kept = import_skills_from(&source, false).expect("kept import");
        assert_eq!(kept.imported, 0);
        assert_eq!(
            kept.skipped_conflicts,
            vec!["usagehub-test-import".to_string()]
        );
        assert_eq!(std::fs::read(target.join("SKILL.md")).unwrap(), b"# v1");

        // Explicit `replace` overwrites with the source bytes.
        let replaced = import_skills_from(&source, true).expect("replace import");
        assert_eq!(replaced.overwritten, 1);
        assert_eq!(std::fs::read(target.join("SKILL.md")).unwrap(), b"# v2");
        let _ = remove_tree(&target);
    }

    /// An existing real user-skills dir is preserved, never converted or
    /// deleted by junction helpers.
    #[test]
    fn junction_preserves_existing_real_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = dir.path().join("adopted");
        let real = base.join("skills");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("mine.md"), "# mine").unwrap();

        let ensured = ensure_account_skills_junction(&base).expect("preserve");
        assert_eq!(ensured, real);
        assert!(std::fs::symlink_metadata(&real).unwrap().is_dir());
        assert!(paths::first_reparse_point(&real).is_none());
        assert_eq!(std::fs::read(real.join("mine.md")).unwrap(), b"# mine");
        assert!(unlink_account_skills_junction(&base).is_err());
    }

    /// Native junction round-trip with shell-hostile names: `& % ^ !`,
    /// spaces, and Unicode stay literal (no shell is ever spawned), the
    /// target bytes traverse through the link, and cleanup unlinks only the
    /// inode. Guards the `REPARSE_DATA_BUFFER` layout (NUL after both names,
    /// `PrintNameOffset` past the substitute NUL, section length covering the
    /// NULs): the old layout failed `FSCTL_SET_REPARSE_POINT` with
    /// `ERROR_INVALID_REPARSE_DATA` (4392).
    #[test]
    fn junction_round_trip_with_shell_metachars_and_unicode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("lib & lib %100 ^! юнікод");
        let skill = target.join("my-skill & more ^!");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# metachars & % ^ ! ✓").unwrap();
        let link = dir
            .path()
            .join("link & dir %50 ^! лінк")
            .join("my-skill & more ^!");
        create_junction(&target, &link).expect("junction with metachar names");
        // The inode is a direct link, not a plain directory.
        assert_eq!(
            paths::first_reparse_point(&link).as_deref(),
            Some(link.as_path())
        );
        // The recorded target stays inside the intended root.
        assert!(
            paths::link_target_within(&link, std::slice::from_ref(&target))
                .expect("link target readable"),
            "unexpected target: {}",
            paths::read_link_target(&link)
                .expect("read link target")
                .display()
        );
        // Target bytes traverse through the junction.
        assert_eq!(
            std::fs::read(link.join("my-skill & more ^!").join("SKILL.md")).unwrap(),
            "# metachars & % ^ ! ✓".as_bytes()
        );
        // Cleanup unlinks the inode only; the target tree is untouched.
        remove_link_only(&link).expect("unlink inode");
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert_eq!(
            std::fs::read(skill.join("SKILL.md")).unwrap(),
            "# metachars & % ^ ! ✓".as_bytes()
        );
    }

    /// A pasted account/config root is never a skill source, even when it
    /// lives outside every known canonical path.
    #[test]
    fn import_refuses_account_shaped_sources() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Fake desktop data dir: Electron markers, not skills.
        let desktop = dir.path().join("fake-desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(desktop.join("config.json"), "{}").unwrap();
        std::fs::write(desktop.join("Local State"), "{}").unwrap();
        assert!(preview_skills_import(&desktop).is_err());
        assert!(import_skills_from(&desktop, false).is_err());
        // Fake CLI config dir: top-level credential file.
        let cli = dir.path().join("fake-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(cli.join(".credentials.json"), "{}").unwrap();
        assert!(preview_skills_import(&cli).is_err());
        // The library importing itself is refused too.
        if let Some(library) = shared_skills_dir() {
            assert!(preview_skills_import(&library).is_err());
        }
    }

    /// Credential/session filenames are refused at any depth, and a clean
    /// normal skill with resources still imports, previews Unchanged, and
    /// conflicts on divergence.
    #[test]
    fn import_refuses_nested_credentials_but_keeps_normal_skills() {
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("root");
        let good = source.join("usagehub-test-good");
        std::fs::create_dir_all(good.join("resources")).unwrap();
        std::fs::write(good.join("SKILL.md"), "# good").unwrap();
        std::fs::write(good.join("resources").join("data.txt"), "data").unwrap();
        // Nested credential file poisons the whole source (fail-closed).
        let bad = source.join("usagehub-test-bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("SKILL.md"), "# bad").unwrap();
        std::fs::write(bad.join(".credentials.json"), "{}").unwrap();
        assert!(preview_skills_import(&source).is_err());
        assert!(import_skills_from(&source, false).is_err());
        std::fs::remove_dir_all(&bad).unwrap();

        // Single clean skill now imports with its resources.
        let only = dir.path().join("usagehub-test-solo");
        std::fs::create_dir_all(only.join("resources")).unwrap();
        std::fs::write(only.join("SKILL.md"), "# solo").unwrap();
        std::fs::write(only.join("resources").join("data.txt"), "data").unwrap();
        let preview = preview_skills_import(&only).expect("solo preview");
        assert_eq!(preview.len(), 1);
        assert_eq!(preview[0].skill_name, "usagehub-test-solo");
        let library = ensure_shared_skills_dir().expect("library");
        let target = library.join("usagehub-test-solo");
        let _ = remove_tree(&target);
        let report = import_skills_from(&only, false).expect("solo import");
        assert_eq!(report.imported, 1);
        assert_eq!(
            std::fs::read(target.join("resources").join("data.txt")).unwrap(),
            b"data"
        );
        // Re-preview is Unchanged; divergence is Conflict; replace wins.
        let again = preview_skills_import(&only).expect("re-preview");
        assert!(again
            .iter()
            .all(|entry| entry.action == SkillImportAction::Unchanged));
        std::fs::write(only.join("SKILL.md"), "# solo v2").unwrap();
        let conflicted = preview_skills_import(&only).expect("conflict preview");
        assert!(conflicted
            .iter()
            .all(|entry| entry.action == SkillImportAction::Conflict));
        let replaced = import_skills_from(&only, true).expect("replace");
        assert_eq!(replaced.overwritten, 1);
        let _ = remove_tree(&target);
    }

    /// A `skills` leaf under a registered account root migrates fine, while
    /// a same-named dir under an unknown root is refused.
    #[test]
    fn known_skills_leaf_migrates_but_unknown_skills_dir_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Genuine registered legacy source home: pre-create real `skills`
        // leaves BEFORE adoption so the adopter preserves real dirs instead
        // of autolinking junctions. Importing through a junction stays
        // fail-closed (links are refused, never followed).
        let second = dir.path().join("second");
        for home in [
            second.join("skills"),
            second.join("claude-config").join("skills"),
        ] {
            let skill = home.join("usagehub-test-leaf");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "# leaf").unwrap();
        }
        std::fs::write(second.join("config.json"), "{}").unwrap();
        crate::manager::lifecycle::import_account(
            crate::providers::ProviderId::Claude,
            &second,
            crate::manager::lifecycle::ImportOpts {
                name: Some("leaf-second".into()),
                ..Default::default()
            },
        )
        .expect("adopt second");
        // The nested CLI config home is registered too (import ensures it);
        // both leaves stayed real dirs, never junctions.
        let nested = second.join("claude-config");
        assert!(nested.is_dir());
        for home in [second.join("skills"), nested.join("skills")] {
            assert!(
                crate::manager::paths::first_reparse_point(&home).is_none(),
                "fixture must stay a real dir: {}",
                home.display()
            );
            let preview = preview_skills_import(&home)
                .unwrap_or_else(|error| panic!("known leaf refused: {home:?}: {error}"));
            assert_eq!(preview.len(), 1);
            assert_eq!(preview[0].skill_name, "usagehub-test-leaf");
            let _ = remove_tree(&library_join("usagehub-test-leaf"));
        }
        // Same basename under an unknown root: refused, not a collection.
        let other = dir.path().join("other").join("skills");
        std::fs::create_dir_all(&other).unwrap();
        assert!(preview_skills_import(&other).is_err());
        let _ = remove_tree(&library_join("usagehub-test-leaf"));
    }

    fn library_join(name: &str) -> std::path::PathBuf {
        ensure_shared_skills_dir().expect("library").join(name)
    }
}
