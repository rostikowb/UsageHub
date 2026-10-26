//! Encrypted 7-day deletion-undo snapshots for account data.
//!
//! A snapshot covers the account registry entry (profile metadata), the
//! account's explicit data files (credential file, config-dir tree, Desktop
//! `config.json` + `Local State`, known shortcut `.lnk` bytes), confirmed
//! owned credential-vault entries (via `super::credentials`), and allowlisted
//! shared-skill links as *metadata only* (link path + target, never followed,
//! never recursed). Unknown reparse points are skipped, never traversed.
//!
//! Format (per backup directory
//! `<backups>/<provider>/<account_id>/<backup_id>/`):
//! - `data.uhb`: framed AES-256-GCM stream. Plaintext layout is
//!   `u64 LE manifest length || manifest JSON || entry payload bytes in
//!   manifest order || EOF marker`. Each 64 KiB plaintext chunk is one frame
//!   (`u32 LE length || ciphertext+tag`) with a per-frame nonce derived from
//!   a random 12-byte base nonce and the frame sequence, AAD-bound to
//!   `UHBA1:<backup_id>:<seq>`. Truncation, reordering, bit-flips, and
//!   trailing garbage all fail verification.
//! - `key.dpapi`: the random per-archive data key wrapped with DPAPI
//!   `CurrentUser` scope (no entropy, `CRYPTPROTECT_UI_FORBIDDEN`). The key
//!   is zeroized after use.
//! - `meta.json`: non-secret [`BackupInfo`] plus a format marker (the personal
//!   tombstone). Purge removes the whole directory: ciphertext, wrapped key,
//!   and tombstone together. No token or secret ever appears in `meta.json`,
//!   logs, or CLI output.
//!
//! Confcurrency: this module never acquires the per-account operation guard
//! (the cleanup owner holds it across stop + snapshot + verify + delete) and
//! uses `manager::state` only for short registry commits during restore.
//! There is no plaintext temp/staging: files are hashed while being read and
//! ciphertext is streamed straight to `data.uhb`. Account directories and
//! files are created with [`paths::ensure_dir`] (which refuses to build
//! through reparse points) and then locked to the current user.
//!
//! [`BackupInfo`]: super::BackupInfo

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::manager::{AccountContext, BackupInfo, ManagerResult};
use crate::providers::ProviderId;

// ---------------------------------------------------------------------------
// Format constants and guards.
// ---------------------------------------------------------------------------

/// Plaintext magic at the start of every `data.uhb`.
const ARCHIVE_MAGIC: &[u8; 8] = b"UHBAF001";
/// Archive format version recorded in the header and the manifest.
const ARCHIVE_VERSION: u16 = 1;
/// Plaintext bytes per AES-GCM frame.
const FRAME_CHUNK: usize = 64 * 1024;
/// Largest single file accepted into a snapshot. Full Electron user-data
/// trees run to gigabytes; files stream in bounded 64 KiB frames so this
/// cap only rejects absurd single files, never materialized buffers.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Largest total payload accepted into one snapshot (u64 accounting over
/// the streaming pass; nothing is ever materialized in memory).
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Largest registry-metadata blob accepted (profile JSON is hundreds of
/// bytes; this cap keeps the one place that buffers it bounded).
const MAX_PROFILE_BYTES: u64 = 1024 * 1024;
/// Largest manifest accepted on read.
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
/// Largest entry count accepted into one snapshot. Sized against
/// MAX_MANIFEST_BYTES: entries serialize to a few hundred bytes of JSON each,
/// so the manifest cap stays the effective bound while this count stops
/// absurd floods before any per-entry work.
const MAX_ENTRIES: usize = 100_000;
/// Prefix for account ids derived from legacy plaintext backup dirs.
pub const LEGACY_ACCOUNT_PREFIX: &str = "legacy_";

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn random_bytes<const N: usize>() -> ManagerResult<[u8; N]> {
    let mut buffer = [0u8; N];
    getrandom::fill(&mut buffer)
        .map_err(|error| format!("Unable to collect backup randomness: {error}"))?;
    Ok(buffer)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(input: &str) -> ManagerResult<Vec<u8>> {
    if input.len() % 2 != 0 {
        return Err("Invalid hex string".into());
    }
    let mut out = Vec::with_capacity(input.len() / 2);
    for pair in input.as_bytes().chunks(2) {
        let high = hex_value(pair[0])?;
        let low = hex_value(pair[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> ManagerResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("Invalid hex string".into()),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

fn hash_file(path: &Path) -> ManagerResult<(u64, String)> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("Unable to read '{}': {error}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("Unable to read '{}': {error}", path.display()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hex_encode(&hasher.finalize())))
}

/// Backup ids double as directory names: strict charset so they can never
/// escape the account backup root.
fn validate_backup_id(backup_id: &str) -> ManagerResult<()> {
    if backup_id.is_empty()
        || backup_id.len() > 128
        || !backup_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!("Unsafe backup id '{backup_id}'"));
    }
    Ok(())
}

/// Archive-internal names are never materialized to disk (restore writes to
/// the recorded absolute paths), but they are still validated for hygiene.
fn validate_rel(rel: &str) -> ManagerResult<()> {
    if rel.is_empty() || rel.len() > 512 {
        return Err(format!("Unsafe archive entry name '{rel}'"));
    }
    let path = Path::new(rel);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(format!("Unsafe archive entry name '{rel}'"));
    }
    Ok(())
}

/// Sanitize an external name (legacy dir name, credential target, file name)
/// into an archive-internal or id-safe token.
fn sanitize_token(name: &str, max_len: usize) -> String {
    let mut out = String::new();
    for character in name.chars().flat_map(|character| character.to_lowercase()) {
        if character.is_ascii_alphanumeric() {
            out.push(character);
        } else if out.bytes().last().is_some_and(|byte| byte != b'_') {
            out.push('_');
        }
        if out.len() >= max_len {
            break;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

fn provider_key(provider: ProviderId) -> ManagerResult<&'static str> {
    match provider {
        ProviderId::Claude => Ok("claude"),
        ProviderId::Codex => Ok("codex"),
        _ => Err("This provider does not support account snapshots".into()),
    }
}

/// Resolve an explicit snapshot root exactly once and return the spelling
/// the archive records. A legitimate adopted root may itself be a directory
/// junction: that single alias is resolved here, up front. Anything else
/// that is itself a link (notably validated skill-link inodes) is returned
/// as the link itself — the collector records the inode, never the target.
/// Ancestors above the result must be link-free, otherwise the snapshot
/// aborts before any hashing or sealing. Cleanup normalizes every requested
/// root with this same function before snapshotting, so coverage
/// attestation compares identical spellings.
pub fn canonical_requested_root(path: &Path) -> ManagerResult<PathBuf> {
    if !path.is_absolute() {
        return Err(format!(
            "Refusing to snapshot relative path '{}'",
            path.display()
        ));
    }
    // A validated shared-skill link is always the inode itself, never the
    // target — including directory junctions, whose symlink metadata reads
    // as a directory. Resolving one here would launder it into ordinary
    // library content and break attested skill-link roots, so skill links
    // return unchanged and only non-skill directory aliases resolve once.
    let resolved = if super::paths::is_reparse_point(path)
        && super::skills::validate_skill_link(path, &shared_library_roots()).is_ok()
    {
        path.to_path_buf()
    } else if super::paths::is_reparse_point(path) {
        // Classify by raw attributes, not FileType: on Windows a junction
        // reports is_symlink() (name-surrogate bit) so file_type().is_dir()
        // is false and the junction branch would be unreachable. Only
        // non-skill directory aliases resolve once; anything else stays.
        if link_is_dir(path) {
            std::fs::canonicalize(path).map_err(|error| {
                format!(
                    "Unable to resolve snapshot root '{}': {error}",
                    path.display()
                )
            })?
        } else {
            path.to_path_buf()
        }
    } else {
        std::fs::canonicalize(path).map_err(|error| {
            format!(
                "Unable to resolve snapshot root '{}': {error}",
                path.display()
            )
        })?
    };
    let mut current = resolved.parent();
    while let Some(ancestor) = current {
        if super::paths::is_reparse_point(ancestor) {
            return Err(format!(
                "Refusing to snapshot '{}': '{}' sits under a link",
                path.display(),
                ancestor.display()
            ));
        }
        current = ancestor.parent();
    }
    Ok(resolved)
}

/// Case-insensitive lexical identity key for one filesystem destination:
/// `.`/`..` segments, both slash styles, letter case, and the Win32/NT
/// prefixes (`\\?\C:\x`, `\??\C:\x`, `\\?\UNC\...`) collapse to one string.
/// A walked canonical child and a raw explicit skill-link root (which
/// [`canonical_requested_root`] deliberately leaves unresolved so the inode
/// is never laundered into library content) compare equal, without resolving
/// anything on disk. Mirrors the comparison used by the path guards.
fn destination_key(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut text = normalized
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

// ---------------------------------------------------------------------------
// Manifest model (serialized into the encrypted stream).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManifestKind {
    Account,
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    /// Regular file bytes appended to the payload stream; restored to `abs`.
    File,
    /// Owned directory recorded with no payload bytes; recreated on restore.
    /// Covers empty directories and explicit owned roots so coverage
    /// attestation can name every requested root.
    Dir,
    /// Serialized [`AccountProfile`](crate::accounts::AccountProfile) bytes;
    /// re-inserted into the settings registry, never written to disk as-is.
    Profile,
    /// Vault entry; the secret rides inline as base64 (`secret_b64`).
    Cred,
    /// Shared-skill link metadata (`abs` + `link_target`); target bytes are
    /// never snapshotted and the target is never traversed.
    Link,
}

#[derive(Clone, Serialize, Deserialize)]
struct ManifestEntry {
    rel: String,
    kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    abs: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link_target: Option<PathBuf>,
    #[serde(default)]
    link_is_dir: bool,
    size: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret_b64: Option<String>,
}

impl std::fmt::Debug for ManifestEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManifestEntry")
            .field("rel", &self.rel)
            .field("kind", &self.kind)
            .field("abs", &self.abs)
            .field("target", &self.target)
            .field("link_target", &self.link_target)
            .field("link_is_dir", &self.link_is_dir)
            .field("size", &self.size)
            .field("sha256", &self.sha256)
            .field(
                "secret_b64",
                &self
                    .secret_b64
                    .as_ref()
                    .map(|secret| format!("[redacted {} chars]", secret.len())),
            )
            .finish()
    }
}

impl Drop for ManifestEntry {
    fn drop(&mut self) {
        // Scrub the base64 secret text in place via the `zeroize` crate
        // (volatile write, compiler-fence guarded); the allocation is freed
        // only after scrubbing. Clones are short-lived by construction
        // (seal/verify streaming passes).
        if let Some(secret) = self.secret_b64.as_mut() {
            secret.zeroize();
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileManifest {
    format: u16,
    backup_id: String,
    provider: String,
    account_id: String,
    created_unix: u64,
    kind: ManifestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_original_path: Option<PathBuf>,
    #[serde(default)]
    warnings: Vec<String>,
    entries: Vec<ManifestEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupMetaFile {
    format: u16,
    info: BackupInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_original_path: Option<PathBuf>,
}

/// One payload aligned 1:1 with a manifest entry that carries bytes
/// ([`EntryKind::File`] and [`EntryKind::Profile`]).
enum EntryPayload {
    Inline(Vec<u8>),
    Disk(PathBuf),
    None,
}

// ---------------------------------------------------------------------------
// DPAPI key wrap (CurrentUser scope, UI forbidden).
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[repr(C)]
struct CryptBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[cfg(windows)]
#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        data_in: *const CryptBlob,
        data_descr: *const u16,
        optional_entropy: *const CryptBlob,
        reserved: *mut std::ffi::c_void,
        prompt_struct: *mut std::ffi::c_void,
        flags: u32,
        data_out: *mut CryptBlob,
    ) -> i32;
    fn CryptUnprotectData(
        data_in: *const CryptBlob,
        data_descr: *mut *mut u16,
        optional_entropy: *const CryptBlob,
        reserved: *mut std::ffi::c_void,
        prompt_struct: *mut std::ffi::c_void,
        flags: u32,
        data_out: *mut CryptBlob,
    ) -> i32;
}

#[cfg(windows)]
extern "system" {
    fn LocalFree(mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

#[cfg(windows)]
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

#[cfg(windows)]
fn dpapi_protect(plaintext: &[u8]) -> ManagerResult<Vec<u8>> {
    let input = CryptBlob {
        cb_data: u32::try_from(plaintext.len())
            .map_err(|_| "Backup key too large for DPAPI".to_string())?,
        pb_data: plaintext.as_ptr() as *mut u8,
    };
    let mut output = CryptBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return Err("Unable to wrap the backup key with DPAPI".into());
    }
    Ok(unsafe {
        let wrapped = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut std::ffi::c_void);
        wrapped
    })
}

#[cfg(windows)]
fn dpapi_unprotect(blob: &[u8]) -> ManagerResult<Vec<u8>> {
    let input = CryptBlob {
        cb_data: u32::try_from(blob.len())
            .map_err(|_| "Backup key blob too large for DPAPI".to_string())?,
        pb_data: blob.as_ptr() as *mut u8,
    };
    let mut output = CryptBlob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pb_data.is_null() {
        return Err("Unable to unwrap the backup key with DPAPI".into());
    }
    Ok(unsafe {
        let key = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut std::ffi::c_void);
        key
    })
}

#[cfg(not(windows))]
fn dpapi_protect(_plaintext: &[u8]) -> ManagerResult<Vec<u8>> {
    Err("Encrypted backups require Windows DPAPI".into())
}

#[cfg(not(windows))]
fn dpapi_unprotect(_blob: &[u8]) -> ManagerResult<Vec<u8>> {
    Err("Encrypted backups require Windows DPAPI".into())
}

// ---------------------------------------------------------------------------
// Current-user file ACL.
// ---------------------------------------------------------------------------

/// Lock `path` to the current user (inheritance removed, full control for
/// the current user only). Fails closed: a backup whose files cannot be
/// locked is aborted rather than left world-readable.
fn lock_to_current_user(path: &Path, is_dir: bool) -> ManagerResult<()> {
    #[cfg(not(windows))]
    {
        let _ = (path, is_dir);
        return Ok(());
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME")
            .map_err(|_| "Unable to determine USERNAME for backup ACL".to_string())?;
        let grant = if is_dir {
            format!("{user}:(OI)(CI)F")
        } else {
            format!("{user}:F")
        };
        let mut command = std::process::Command::new("icacls");
        command
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(&grant);
        const ICACLS_CAPTURE_CAP: usize = 64 * 1024;
        let output = crate::manager::process::run_bounded(
            &mut command,
            std::time::Duration::from_secs(10),
            ICACLS_CAPTURE_CAP,
            "backup.icacls",
        )
        .map_err(|error| format!("Unable to lock '{}': {error}", path.display()))?;
        // Fail closed: truncation or a non-zero exit aborts the backup
        // rather than leaving the file world-readable.
        if output.stdout.len() > ICACLS_CAPTURE_CAP
            || output.stderr.len() > ICACLS_CAPTURE_CAP
            || !output.status.success()
        {
            return Err(format!(
                "Unable to lock '{}' to the current user",
                path.display()
            ));
        }
        Ok(())
    }
}

/// Create `dir` and any missing ancestors. Returns the levels this call
/// created (top-down) so restore can roll exactly those back; pre-existing
/// parents are never touched or re-ACL'd. Any existing reparse point or
/// non-directory on the way aborts instead of writing through an alias.
fn ensure_created_tree(dir: &Path) -> ManagerResult<Vec<PathBuf>> {
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = dir;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) => {
                if super::paths::is_reparse_point(current) {
                    return Err(format!(
                        "Refusing to restore through link '{}'",
                        current.display()
                    ));
                }
                if !metadata.file_type().is_dir() {
                    return Err(format!(
                        "Refusing to restore through non-directory '{}'",
                        current.display()
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
            }
            Err(error) => {
                return Err(format!("Unable to stat '{}': {error}", current.display()));
            }
        }
        let Some(parent) = current.parent() else {
            return Err(format!(
                "Refusing to restore path with no root '{}'",
                dir.display()
            ));
        };
        current = parent;
    }
    let mut created: Vec<PathBuf> = Vec::new();
    for level in missing.iter().rev() {
        if let Err(error) = (|| -> ManagerResult<()> {
            std::fs::create_dir(level)
                .map_err(|error| format!("Unable to recreate '{}': {error}", level.display()))?;
            lock_to_current_user(level, true)?;
            Ok(())
        })() {
            // An ACL failure after creation must not leave an unlocked
            // directory behind: remove what this call created (deepest
            // first; remove_dir only removes empty dirs) and fail.
            for done in created.iter().rev() {
                let _ = std::fs::remove_dir(done);
            }
            return Err(error);
        }
        created.push(level.clone());
    }
    Ok(created)
}

/// Write small files durably (create + write_all + sync_all) so a power
/// loss cannot leave a half-written key or tombstone behind.
fn write_persisted(path: &Path, bytes: &[u8], what: &str) -> ManagerResult<()> {
    let file =
        std::fs::File::create(path).map_err(|error| format!("Unable to write {what}: {error}"))?;
    (&file)
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("Unable to persist {what}: {error}"))?;
    Ok(())
}

struct Collector {
    entries: Vec<(ManifestEntry, EntryPayload)>,
    /// Collapsed physical destinations ([`destination_key`]): the
    /// same inode reached through an overlapping walked root and as an
    /// explicit target records one entry, whichever spelling came first.
    seen: BTreeMap<String, EntryKind>,
    total_bytes: u64,
    warnings: Vec<String>,
}

impl Collector {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            seen: BTreeMap::new(),
            total_bytes: 0,
            warnings: Vec::new(),
        }
    }

    /// Reserve `abs` as a snapshot destination. Returns `false` when the same
    /// physical path was already recorded with the same kind — overlapping
    /// roots collapse to one entry — and fails when it was recorded as a
    /// different kind, which means the tree changed while it was read.
    fn reserve(&mut self, abs: &Path, kind: EntryKind) -> ManagerResult<bool> {
        let key = destination_key(abs);
        match self.seen.get(&key) {
            Some(existing) if *existing == kind => Ok(false),
            Some(existing) => Err(format!(
                "Snapshot destination '{}' changed from {existing:?} to {kind:?} mid-snapshot",
                abs.display()
            )),
            None => {
                self.seen.insert(key, kind);
                Ok(true)
            }
        }
    }

    fn check_budgets(&self, extra_bytes: u64) -> ManagerResult<()> {
        if self.entries.len() + 1 > MAX_ENTRIES {
            return Err(format!("Backup snapshot exceeds {MAX_ENTRIES} entries"));
        }
        if self.total_bytes.saturating_add(extra_bytes) > MAX_TOTAL_BYTES {
            return Err("Backup snapshot exceeds the 64 GiB payload cap".into());
        }
        Ok(())
    }

    fn push_file(&mut self, abs: &Path, rel: String) -> ManagerResult<()> {
        validate_rel(&rel)?;
        // Single files resolve once here so the recorded `abs` matches the
        // canonical spellings used for directory trees and restore. Fails
        // closed when the layout shifts mid-snapshot.
        let abs = canonical_requested_root(abs)?;
        refuse_library_content(&abs, &shared_library_roots())?;
        if !self.reserve(&abs, EntryKind::File)? {
            return Ok(());
        }
        let (size, sha256) = hash_file(&abs)?;
        if size > MAX_FILE_BYTES {
            return Err(format!(
                "Refusing to snapshot oversized file '{}' ({size} bytes)",
                abs.display()
            ));
        }
        self.check_budgets(size)?;
        self.total_bytes += size;
        self.entries.push((
            ManifestEntry {
                rel,
                kind: EntryKind::File,
                abs: Some(abs.clone()),
                target: None,
                link_target: None,
                link_is_dir: false,
                size,
                sha256,
                secret_b64: None,
            },
            EntryPayload::Disk(abs),
        ));
        Ok(())
    }

    fn push_inline(&mut self, rel: &str, kind: EntryKind, bytes: Vec<u8>) -> ManagerResult<()> {
        validate_rel(rel)?;
        let size = bytes.len() as u64;
        self.check_budgets(size)?;
        self.total_bytes += size;
        let sha256 = sha256_hex(&bytes);
        self.entries.push((
            ManifestEntry {
                rel: rel.to_string(),
                kind,
                abs: None,
                target: None,
                link_target: None,
                link_is_dir: false,
                size,
                sha256,
                secret_b64: None,
            },
            EntryPayload::Inline(bytes),
        ));
        Ok(())
    }

    fn push_cred(
        &mut self,
        index: usize,
        record: &super::credentials::CredentialRecord,
    ) -> ManagerResult<()> {
        let rel = format!("cred/{index:04}.cred");
        validate_rel(&rel)?;
        let secret_b64 = super::credentials::base64_encode(record.secret_bytes());
        // Hash the raw secret bytes (not the base64) so verification detects
        // any vault-unrelated mutation of the stored material.
        let sha256 = sha256_hex(record.secret_bytes());
        let size = record.secret_bytes().len() as u64;
        self.check_budgets(size)?;
        self.total_bytes += size;
        self.entries.push((
            ManifestEntry {
                rel,
                kind: EntryKind::Cred,
                abs: None,
                target: Some(record.target.clone()),
                link_target: None,
                link_is_dir: false,
                size,
                sha256,
                secret_b64: Some(secret_b64),
            },
            EntryPayload::None,
        ));
        Ok(())
    }

    fn push_link(
        &mut self,
        abs: &Path,
        rel: String,
        target: PathBuf,
        link_is_dir: bool,
    ) -> ManagerResult<()> {
        validate_rel(&rel)?;
        if !abs.is_absolute() || !target.is_absolute() {
            return Err("Refusing to snapshot a non-absolute skill link".into());
        }
        // An explicit skill-link root and the same link reached through a
        // walked parent root are one physical destination: record it once.
        if !self.reserve(abs, EntryKind::Link)? {
            return Ok(());
        }
        self.check_budgets(0)?;
        let fingerprint = format!("link:{}->{}", abs.display(), target.display());
        self.entries.push((
            ManifestEntry {
                rel,
                kind: EntryKind::Link,
                abs: Some(abs.to_path_buf()),
                target: None,
                link_target: Some(target),
                link_is_dir,
                size: 0,
                sha256: sha256_hex(fingerprint.as_bytes()),
                secret_b64: None,
            },
            EntryPayload::None,
        ));
        Ok(())
    }

    /// Record an owned directory with no payload bytes. Covers empty
    /// directories and explicit owned roots so restore can recreate the tree
    /// and attestation can name every requested root.
    fn push_dir(&mut self, abs: &Path, rel: String) -> ManagerResult<()> {
        validate_rel(&rel)?;
        if !abs.is_absolute() {
            return Err(format!(
                "Refusing to snapshot relative path '{}'",
                abs.display()
            ));
        }
        if !self.reserve(abs, EntryKind::Dir)? {
            return Ok(());
        }
        self.check_budgets(0)?;
        let fingerprint = format!("dir:{}", abs.display());
        self.entries.push((
            ManifestEntry {
                rel,
                kind: EntryKind::Dir,
                abs: Some(abs.to_path_buf()),
                target: None,
                link_target: None,
                link_is_dir: false,
                size: 0,
                sha256: sha256_hex(fingerprint.as_bytes()),
                secret_b64: None,
            },
            EntryPayload::None,
        ));
        Ok(())
    }
}

/// Bounded wait for the per-account operation mutex on mutating entry
/// points. Cleanup holds the same mutex across stop+snapshot+verify+delete
/// (Windows mutexes are recursive on the owning thread, so nested
/// acquisition from cleanup's own calls is balanced, not a deadlock); a
/// direct call that cannot acquire in time fails instead of racing live
/// deletion. The settings mutex is never held across these waits.
pub const BACKUP_GUARD_TIMEOUT_MS: u32 = 30_000;
/// Shorter bound for read-only verification passes (UI worker threads).
pub const BACKUP_VERIFY_TIMEOUT_MS: u32 = 5_000;

/// Re-validate a caller-supplied context against fresh registry state after
/// the account guard is held and before any mutation: an account removed or
/// reconfigured since preview must abort instead of snapshotting the wrong
/// source or covering the wrong vault targets. Read-only; no nesting.
fn recheck_ctx(ctx: &AccountContext) -> ManagerResult<()> {
    let settings = super::state::load_fresh();
    let accounts = settings
        .accounts
        .get(ctx.provider)
        .ok_or_else(|| format!("Account '{}' is no longer registered", ctx.profile.id))?;
    let existing = accounts
        .profiles
        .iter()
        .find(|profile| profile.id == ctx.profile.id)
        .ok_or_else(|| format!("Account '{}' is no longer registered", ctx.profile.id))?;
    if !existing.same_source(&ctx.profile)
        || existing.credential_targets != ctx.profile.credential_targets
    {
        return Err(format!(
            "Account '{}' changed since preview; re-run preview before snapshotting",
            ctx.profile.id
        ));
    }
    Ok(())
}

/// Refuse to snapshot a live Desktop profile: a running Chromium-family app
/// holds its user-data-dir lock files open with no sharing, and copying
/// `Local State` under a running app yields an inconsistent snapshot. Only
/// an actually-held lock refuses — stale zero-byte `SingletonLock` /
/// `lockfile` leftovers from a previous run open fine and do not block.
/// The cleanup owner stops the account (while holding the per-account
/// guard) before snapshotting, so after a real stop this probe passes;
/// direct callers get this error as the closed-profile prerequisite.
fn ensure_quiesced(ctx: &AccountContext) -> ManagerResult<()> {
    if let Some(desktop) = &ctx.desktop_dir {
        for lock in ["SingletonLock", "lockfile"] {
            if lock_held_by_live_process(&desktop.join(lock))? {
                return Err(format!(
                     "Refusing to snapshot '{}': the profile is running ({} is locked by a live process). Stop it first so the backup is consistent.",
                     ctx.display(),
                     desktop.join(lock).display()
                 ));
            }
        }
    }
    Ok(())
}

/// True when `path` exists and is currently opened by a live process with
/// no sharing. Implemented with an exclusive `CreateFileW` probe
/// (`OPEN_EXISTING`, zero share mode): a stale leftover opens fine and
/// reports false, while a running holder fails the probe with a sharing or
/// access violation. Nothing is created, deleted, or modified.
#[cfg(windows)]
fn lock_held_by_live_process(path: &Path) -> ManagerResult<bool> {
    use std::os::windows::ffi::OsStrExt;
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(false);
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x8000_0000,
            0,
            std::ptr::null(),
            3,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(handle) };
        return Ok(false);
    }
    match unsafe { GetLastError() } {
        // Another process holds the file open without sharing (or denies
        // readers outright): treat as a live lock.
        32 | 33 | 5 => Ok(true),
        code => Err(format!(
            "Unable to probe lock state of '{}' (Win32 {code})",
            path.display()
        )),
    }
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
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
}

#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = -1isize as *mut std::ffi::c_void;

#[cfg(not(windows))]
fn lock_held_by_live_process(path: &Path) -> ManagerResult<bool> {
    // No deny-share probing off Windows; fall back to presence.
    Ok(path.exists())
}

/// Bind a metadata/credential-only snapshot to a registered account. Called
/// only when a targeted snapshot covers no file/link/dir entries: without
/// this check the archive would be an unbound token authenticating nothing.
/// Managed accounts always qualify; External accounts qualify only when
/// they declare confirmed-owned vault targets (credential-only reset).
/// Read-only registry lookup — no guard, no mutation, no nesting.
fn authenticate_metadata_only_snapshot(ctx: &AccountContext) -> ManagerResult<()> {
    let settings = super::state::load_fresh();
    let accounts = settings
        .accounts
        .get(ctx.provider)
        .ok_or_else(|| format!("Refusing unbound snapshot for account '{}'", ctx.profile.id))?;
    let existing = accounts
        .profiles
        .iter()
        .find(|profile| profile.id == ctx.profile.id)
        .ok_or_else(|| {
            format!(
                "Refusing unbound snapshot for unregistered account '{}'",
                ctx.profile.id
            )
        })?;
    if existing.ownership == crate::accounts::AccountOwnership::Managed {
        return Ok(());
    }
    let declares_vault = ctx
        .profile
        .credential_targets
        .iter()
        .any(|target| !target.trim().is_empty());
    if declares_vault {
        return Ok(());
    }
    Err(format!(
        "Refusing unbound snapshot for external account '{}' with no owned vault targets",
        ctx.profile.id
    ))
}
/// Classify a link as directory-like without following it. Following a
/// dangling directory junction reports "file" and restores the wrong kind;
/// the directory attribute classifies the inode itself.
fn link_is_dir(link: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        std::fs::symlink_metadata(link)
            .map(|metadata| {
                metadata.file_attributes()
                    & crate::manager::paths::FILE_ATTRIBUTE_DIRECTORY_FOR_LINK
                    != 0
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        false
    }
}

/// Resolve a validated link target to an absolute spelling for recording.
/// Relative targets resolve against the link's parent; the result is
/// lexically cleaned (`.`/`..`) without touching the filesystem, so
/// dangling links keep working and nothing is followed.
fn resolve_link_target(link: &Path, raw_target: PathBuf) -> PathBuf {
    if raw_target.is_absolute() {
        return raw_target;
    }
    let joined = link
        .parent()
        .map(|parent| parent.join(&raw_target))
        .unwrap_or(raw_target);
    let mut out = PathBuf::new();
    let mut depth = 0u32;
    let mut rooted = false;
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => {
                out.push(Component::RootDir.as_os_str());
                rooted = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    out.pop();
                    depth -= 1;
                } else if !rooted {
                    out.push("..");
                }
            }
            Component::Normal(part) => {
                out.push(part);
                depth += 1;
            }
        }
    }
    out
}

/// Refuse to snapshot file/dir bytes that live inside the shared skills
/// library: the library is link-target-only by design and never stores auth
/// material, so a snapshot reaching into it is a misconfiguration, not data.
fn refuse_library_content(path: &Path, shared_roots: &[PathBuf]) -> ManagerResult<()> {
    if shared_roots
        .iter()
        .any(|root| super::paths::is_within(root, path))
    {
        return Err(format!(
            "Refusing to snapshot shared-library content '{}'",
            path.display()
        ));
    }
    Ok(())
}

/// Walk `base` recursively. `base` is resolved exactly once with
/// [`canonical_requested_root`] (a legitimate adopted root may itself be a
/// junction); every recorded `abs` derives from that canonical target.
/// Regular files become `File` entries under `rel_prefix`; validated
/// shared-skill links become `Link` metadata; every other reparse point is
/// skipped (never followed) with a recorded warning. File/dir bytes inside
/// the shared skills library are refused outright. Empty directories
/// (including an empty `base`) are recorded as `Dir` entries so restore
/// recreates the tree and attestation names roots. The same inode reached by
/// an overlapping sibling root is collapsed by `Collector::reserve`, so one
/// physical destination is recorded once.
fn walk_dir(
    collector: &mut Collector,
    base: &Path,
    rel_prefix: &str,
    shared_roots: &[PathBuf],
) -> ManagerResult<()> {
    let canonical_base = canonical_requested_root(base)?;
    refuse_library_content(&canonical_base, shared_roots)?;
    let mut stack = vec![(canonical_base.clone(), rel_prefix.to_string())];
    // The requested root itself is always recorded: empty trees survive
    // only through this marker, and attestation names every root.
    collector.push_dir(&canonical_base, rel_prefix.to_string())?;
    while let Some((dir, prefix)) = stack.pop() {
        let listing = std::fs::read_dir(&dir)
            .map_err(|error| format!("Unable to list '{}': {error}", dir.display()))?;
        // Deterministic order so two snapshots of the same tree seal
        // identical entry sequences.
        let mut children: Vec<PathBuf> = Vec::new();
        for child in listing {
            let child =
                child.map_err(|error| format!("Unable to list '{}': {error}", dir.display()))?;
            children.push(child.path());
        }
        children.sort();
        for child in children {
            let name = child
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let rel = format!("{prefix}/{name}");
            if super::paths::is_reparse_point(&child) {
                refuse_library_content(&child, shared_roots)?;
                match super::skills::validate_skill_link(&child, shared_roots) {
                    Ok(target) => {
                        let target = resolve_link_target(&child, target);
                        collector.push_link(&child, rel, target, link_is_dir(&child))?;
                    }
                    Err(_) => {
                        collector.warnings.push(format!(
                            "Skipped non-skill link '{}' (never followed)",
                            child.display()
                        ));
                    }
                }
                continue;
            }
            // The shared library is link-target-only: real bytes under it
            // are never swept into an account snapshot.
            refuse_library_content(&child, shared_roots)?;
            let file_type = std::fs::symlink_metadata(&child)
                .map_err(|error| format!("Unable to stat '{}': {error}", child.display()))?
                .file_type();
            if file_type.is_dir() {
                // Recorded even when non-empty: empty subtrees survive only
                // through these markers, and attestation names every root.
                collector.push_dir(&child, rel.clone())?;
                stack.push((child, rel));
            } else if file_type.is_file() {
                collector.push_file(&child, rel)?;
            } else {
                collector
                    .warnings
                    .push(format!("Skipped special file '{}'", child.display()));
            }
        }
    }
    Ok(())
}

fn shared_library_roots() -> Vec<PathBuf> {
    super::skills::shared_skills_dir().into_iter().collect()
}

/// Conventional shortcut locations for one account. Lifecycle owns shortcut
/// creation; backup snapshots whatever exists here by exact absolute path so
/// restore can recreate it byte-for-byte.
fn known_shortcut_paths(ctx: &AccountContext) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let file_stem = format!(
        "UsageHub {} {}",
        ctx.provider.descriptor().key,
        ctx.profile.name.trim()
    );
    let push_lnk = |dir: PathBuf, out: &mut Vec<PathBuf>| {
        out.push(dir.join(format!("{file_stem}.lnk")));
    };
    if let Some(roaming) = crate::known_folders::roaming_app_data_dir() {
        push_lnk(
            roaming
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join("UsageHub"),
            &mut paths,
        );
    }
    if let Some(home) = crate::known_folders::home_dir() {
        push_lnk(home.join("Desktop"), &mut paths);
    }
    paths
}

/// Snapshot `.lnk` files that provably belong to the account: `.lnk` files
/// directly inside the managed account dir, plus (for managed accounts only)
/// Start Menu/Desktop shortcuts whose file name references the account id or
/// display name. External accounts never trigger Start Menu/Desktop name
/// matching — no global discovery for read-only accounts.
fn collect_shortcuts(ctx: &AccountContext, collector: &mut Collector) -> ManagerResult<()> {
    let managed = super::paths::managed_account_dir(ctx.provider, &ctx.profile.id).ok();
    if let Some(dir) = managed {
        if dir.is_dir() && super::paths::first_reparse_point(&dir).is_none() {
            if let Ok(listing) = std::fs::read_dir(&dir) {
                let mut direct: Vec<PathBuf> = listing
                    .filter_map(|child| child.ok().map(|child| child.path()))
                    .collect();
                direct.sort();
                for child in direct {
                    let is_lnk = child
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"));
                    if !is_lnk || super::paths::is_reparse_point(&child) {
                        continue;
                    }
                    if std::fs::symlink_metadata(&child)
                        .map(|metadata| metadata.file_type().is_file())
                        .unwrap_or(false)
                    {
                        let name = child
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "shortcut.lnk".to_string());
                        collector.push_file(&child, format!("shortcuts/{name}"))?;
                    }
                }
            }
        }
    }
    if ctx.ownership == crate::accounts::AccountOwnership::Managed {
        let id_lower = ctx.profile.id.to_lowercase();
        let name_lower = ctx.profile.name.to_lowercase();
        for candidate in known_shortcut_paths(ctx) {
            let matches = candidate
                .file_stem()
                .map(|stem| {
                    let stem = stem.to_string_lossy().to_lowercase();
                    stem.contains(&id_lower)
                        || (!name_lower.is_empty() && stem.contains(&name_lower))
                })
                .unwrap_or(false);
            if !matches {
                continue;
            }
            if super::paths::is_reparse_point(&candidate) {
                continue;
            }
            if std::fs::symlink_metadata(&candidate)
                .map(|metadata| metadata.file_type().is_file())
                .unwrap_or(false)
            {
                let name = candidate
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "shortcut.lnk".to_string());
                collector.push_file(&candidate, format!("shortcuts/{name}"))?;
            }
        }
    }
    Ok(())
}

fn collect_account_snapshot(ctx: &AccountContext) -> ManagerResult<Collector> {
    ensure_quiesced(ctx)?;
    let mut collector = Collector::new();
    // Registry metadata first: deterministic entry order.
    let profile_json = serde_json::to_vec(&ctx.profile)
        .map_err(|error| format!("Unable to serialize account metadata: {error}"))?;
    collector.push_inline("meta/profile.json", EntryKind::Profile, profile_json)?;

    // Confirmed-owned vault entries (exact targets only, no enumeration).
    let credentials = super::credentials::capture_owned(ctx)?;
    for (index, record) in credentials.iter().enumerate() {
        collector.push_cred(index, record)?;
    }

    let shared_roots = shared_library_roots();
    match ctx.source {
        crate::accounts::SourceKind::CredentialFile => {
            if let Some(config_dir) = &ctx.config_dir {
                if let Some(credential_file) = &ctx.credential_file {
                    // The explicit credential file usually lives inside the
                    // config dir tree; avoid snapshotting it twice.
                    let inside = super::paths::is_within(config_dir, credential_file);
                    if !inside && credential_file.is_file() {
                        let name = credential_file
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| ".credentials.json".to_string());
                        collector.push_file(credential_file, format!("config/{name}"))?;
                    }
                }
                if config_dir.is_dir() {
                    walk_dir(&mut collector, config_dir, "config", &shared_roots)?;
                }
            } else if let Some(credential_file) = &ctx.credential_file {
                if credential_file.is_file() {
                    let name = credential_file
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| ".credentials.json".to_string());
                    collector.push_file(credential_file, format!("config/{name}"))?;
                }
            }
        }
        crate::accounts::SourceKind::DesktopDir => {
            if let Some(desktop) = &ctx.desktop_dir {
                for name in ["config.json", "Local State"] {
                    let candidate = desktop.join(name);
                    if super::paths::is_reparse_point(&candidate) {
                        continue;
                    }
                    if std::fs::symlink_metadata(&candidate)
                        .map(|metadata| metadata.file_type().is_file())
                        .unwrap_or(false)
                    {
                        collector.push_file(&candidate, format!("desktop/{name}"))?;
                    }
                }
            }
            if let Some(config_dir) = &ctx.config_dir {
                if config_dir.is_dir() {
                    walk_dir(&mut collector, config_dir, "config", &shared_roots)?;
                }
            }
        }
        crate::accounts::SourceKind::ProviderDefault => {
            // External Default data is shared/global by definition: snapshot
            // metadata plus confirmed-owned vault entries only, never walk
            // provider-default discovery paths.
            collector.warnings.push(
                "Provider-default source: snapshotted metadata and owned credentials only"
                    .to_string(),
            );
        }
    }
    collect_shortcuts(ctx, &mut collector)?;
    Ok(collector)
}

fn collect_paths_snapshot(targets: &[PathBuf]) -> ManagerResult<Collector> {
    // An empty target list is legal here: the caller (`create_backup_for_paths`)
    // authenticates metadata/credential-only snapshots against the registry.
    // Every requested root that exists lands in the archive (files, validated
    // skill-link inodes, or `Dir` markers for empty directories) so coverage
    // attestation can name each root exactly. Overlapping roots (a Desktop
    // tree plus its nested CLI config plus the skill links inside both)
    // collapse by physical destination: one entry per inode, whichever
    // spelling reaches it first, so restore never creates a path twice.
    let mut collector = Collector::new();
    let shared_roots = shared_library_roots();
    for (index, target) in targets.iter().enumerate() {
        if !target.is_absolute() {
            return Err(format!(
                "Refusing to snapshot relative path '{}'",
                target.display()
            ));
        }
        let canonical = canonical_requested_root(target).map_err(|error| {
            format!(
                "Snapshot target '{}' is unavailable: {error}",
                target.display()
            )
        })?;
        let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| {
            format!(
                "Snapshot target '{}' is unavailable: {error}",
                target.display()
            )
        })?;
        let base = canonical
            .file_name()
            .map(|name| sanitize_token(&name.to_string_lossy(), 64))
            .unwrap_or_else(|| "target".to_string());
        let prefix = format!("explicit/{index:04}_{base}");
        if super::paths::is_reparse_point(&canonical) {
            refuse_library_content(&canonical, &shared_roots)?;
            match super::skills::validate_skill_link(&canonical, &shared_roots) {
                Ok(link_target) => {
                    let link_target = resolve_link_target(&canonical, link_target);
                    collector.push_link(
                        &canonical,
                        prefix,
                        link_target,
                        link_is_dir(&canonical),
                    )?;
                }
                Err(_) => {
                    collector.warnings.push(format!(
                        "Skipped non-skill link '{}' (never followed)",
                        target.display()
                    ));
                }
            }
            continue;
        }
        if metadata.file_type().is_dir() {
            walk_dir(&mut collector, &canonical, &prefix, &shared_roots)?;
        } else if metadata.file_type().is_file() {
            collector.push_file(&canonical, format!("{prefix}/{}", base))?;
        } else {
            return Err(format!(
                "Refusing to snapshot special file '{}'",
                target.display()
            ));
        }
    }
    Ok(collector)
}

fn collect_legacy_snapshot(dir: &Path) -> ManagerResult<Collector> {
    if !dir.is_absolute() {
        return Err(format!(
            "Refusing to import relative legacy path '{}'",
            dir.display()
        ));
    }
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| format!("Legacy backup '{}' is unavailable: {error}", dir.display()))?;
    if !metadata.file_type().is_dir() || super::paths::is_reparse_point(dir) {
        return Err(format!(
            "Legacy backup '{}' is not a plain directory",
            dir.display()
        ));
    }
    let mut collector = Collector::new();
    walk_dir(&mut collector, dir, "legacy", &shared_library_roots())?;
    if !collector
        .entries
        .iter()
        .any(|(entry, _)| entry.kind == EntryKind::File)
    {
        return Err(format!("Legacy backup '{}' holds no files", dir.display()));
    }
    Ok(collector)
}

// ---------------------------------------------------------------------------
// Framed AES-GCM archive writer/reader.
// ---------------------------------------------------------------------------

fn frame_aad(backup_id: &str, seq: u64) -> Vec<u8> {
    format!("UHBA1:{backup_id}:{seq:020}").into_bytes()
}

fn frame_nonce(base: &[u8; 12], seq: u64) -> [u8; 12] {
    let mut nonce = *base;
    for (index, byte) in seq.to_be_bytes().iter().enumerate() {
        nonce[4 + index] ^= byte;
    }
    nonce
}

struct Framer<'a> {
    cipher: &'a Aes256Gcm,
    backup_id: &'a str,
    base_nonce: [u8; 12],
    seq: u64,
    chunk: Vec<u8>,
    out: BufWriter<std::fs::File>,
}

impl<'a> Framer<'a> {
    fn feed(&mut self, mut bytes: &[u8]) -> ManagerResult<()> {
        while !bytes.is_empty() {
            let room = FRAME_CHUNK - self.chunk.len();
            let take = room.min(bytes.len());
            self.chunk.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.chunk.len() == FRAME_CHUNK {
                self.flush_frame()?;
            }
        }
        Ok(())
    }

    fn feed_file(&mut self, path: &Path, expected: u64) -> ManagerResult<()> {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("Unable to read '{}': {error}", path.display()))?;
        let mut reader = BufReader::new(file);
        let mut remaining = expected;
        let mut buffer = [0u8; 65536];
        while remaining > 0 {
            let want = (remaining.min(buffer.len() as u64)) as usize;
            let read = reader
                .read(&mut buffer[..want])
                .map_err(|error| format!("Unable to read '{}': {error}", path.display()))?;
            if read == 0 {
                return Err(format!("File '{}' shrank during snapshot", path.display()));
            }
            remaining -= read as u64;
            self.feed(&buffer[..read])?;
        }
        // The file must not have grown between hashing and sealing either.
        let mut tail = [0u8; 1];
        let extra = reader
            .read(&mut tail)
            .map_err(|error| format!("Unable to read '{}': {error}", path.display()))?;
        if extra != 0 {
            return Err(format!("File '{}' changed during snapshot", path.display()));
        }
        Ok(())
    }

    fn flush_frame(&mut self) -> ManagerResult<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let nonce = frame_nonce(&self.base_nonce, self.seq);
        let aad = frame_aad(self.backup_id, self.seq);
        let ciphertext = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &self.chunk,
                    aad: &aad,
                },
            )
            .map_err(|error| format!("Unable to seal backup frame {}: {error:?}", self.seq))?;
        let length = u32::try_from(ciphertext.len())
            .map_err(|_| format!("Backup frame {} too large", self.seq))?;
        self.out
            .write_all(&length.to_le_bytes())
            .and_then(|()| self.out.write_all(&ciphertext))
            .map_err(|error| format!("Unable to write backup frame {}: {error}", self.seq))?;
        // Zeroize BEFORE clearing: `clear` + `zeroize` would leave the
        // plaintext in the retained capacity.
        self.chunk.zeroize();
        self.chunk.clear();
        self.seq += 1;
        Ok(())
    }

    fn finish(mut self) -> ManagerResult<()> {
        self.flush_frame()?;
        self.out
            .flush()
            .map_err(|error| format!("Unable to flush backup archive: {error}"))?;
        // Durable before the self-verify/re-delete gate: a power loss must
        // not leave a "verified" backup that is empty on disk.
        self.out
            .get_ref()
            .sync_all()
            .map_err(|error| format!("Unable to persist backup archive: {error}"))?;
        Ok(())
    }
}

struct FrameReader {
    file: BufReader<std::fs::File>,
    cipher: Aes256Gcm,
    backup_id: String,
    base_nonce: [u8; 12],
    seq: u64,
    pending: Vec<u8>,
}

impl FrameReader {
    fn open(data_path: &Path, key: &[u8], backup_id: &str) -> ManagerResult<Self> {
        let file = std::fs::File::open(data_path)
            .map_err(|error| format!("Unable to read '{}': {error}", data_path.display()))?;
        let mut file = BufReader::new(file);
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)
            .map_err(|_| format!("Backup '{backup_id}' is truncated (missing header)"))?;
        if &magic != ARCHIVE_MAGIC {
            return Err(format!("Backup '{backup_id}' is not a UsageHub archive"));
        }
        let mut version = [0u8; 2];
        file.read_exact(&mut version)
            .map_err(|_| format!("Backup '{backup_id}' is truncated (missing version)"))?;
        if u16::from_le_bytes(version) != ARCHIVE_VERSION {
            return Err(format!("Backup '{backup_id}' uses an unsupported format"));
        }
        let mut base_nonce = [0u8; 12];
        file.read_exact(&mut base_nonce)
            .map_err(|_| format!("Backup '{backup_id}' is truncated (missing nonce)"))?;
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|_| format!("Backup '{backup_id}' has an invalid data key"))?;
        Ok(Self {
            file,
            cipher,
            backup_id: backup_id.to_string(),
            base_nonce,
            seq: 0,
            pending: Vec::new(),
        })
    }

    fn fill_pending(&mut self) -> ManagerResult<bool> {
        // A 0-byte read at a frame boundary is clean EOF; 1-3 bytes is a
        // torn frame length and fails instead of passing as EOF.
        let mut first = [0u8; 1];
        match self.file.read_exact(&mut first) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => {
                return Err(format!(
                    "Unable to read backup '{}': {error}",
                    self.backup_id
                ))
            }
        }
        let mut length = [0u8; 4];
        length[0] = first[0];
        if let Err(error) = self.file.read_exact(&mut length[1..]) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(format!(
                    "Backup '{}' has a torn frame length at frame {}",
                    self.backup_id, self.seq
                ));
            }
            return Err(format!(
                "Unable to read backup '{}': {error}",
                self.backup_id
            ));
        }
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > FRAME_CHUNK + 16 {
            return Err(format!(
                "Backup '{}' frame {} has an impossible length ({length})",
                self.backup_id, self.seq
            ));
        }
        let mut ciphertext = vec![0u8; length];
        self.file.read_exact(&mut ciphertext).map_err(|_| {
            format!(
                "Backup '{}' is truncated inside frame {}",
                self.backup_id, self.seq
            )
        })?;
        let nonce = frame_nonce(&self.base_nonce, self.seq);
        let aad = frame_aad(&self.backup_id, self.seq);
        let plaintext = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| {
                format!(
                    "Backup '{}' failed authentication at frame {} (tampered or wrong key)",
                    self.backup_id, self.seq
                )
            })?;
        ciphertext.zeroize();
        self.seq += 1;
        self.pending.extend_from_slice(&plaintext);
        Ok(true)
    }

    /// Drain exactly `n` plaintext bytes or fail on truncation.
    fn take(&mut self, n: usize) -> ManagerResult<Vec<u8>> {
        let mut out = Vec::with_capacity(n.min(65536));
        let mut remaining = n;
        while remaining > 0 {
            if self.pending.is_empty() && !self.fill_pending()? {
                return Err(format!(
                    "Backup '{}' is truncated (expected {n} bytes, {remaining} missing)",
                    self.backup_id
                ));
            }
            let take = remaining.min(self.pending.len());
            out.extend_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
            remaining -= take;
        }
        Ok(out)
    }

    /// Stream exactly `n` plaintext bytes through `sink` in bounded pieces
    /// without ever materializing the whole span: at most one decrypted
    /// frame plus the caller's chunk is alive at a time. File entries of any
    /// size verify through this path; only the manifest, the small profile
    /// blob, and vault secrets (already bounded) use [`take`]. The sink is
    /// fallible so writers propagate their errors instead of panicking or
    /// truncating silently.
    fn consume_exact(
        &mut self,
        mut remaining: u64,
        mut sink: impl FnMut(&[u8]) -> ManagerResult<()>,
    ) -> ManagerResult<()> {
        let total = remaining;
        while remaining > 0 {
            if self.pending.is_empty() && !self.fill_pending()? {
                return Err(format!(
                    "Backup '{}' is truncated (expected {total} bytes, {remaining} missing)",
                    self.backup_id
                ));
            }
            let take = (remaining.min(self.pending.len() as u64)) as usize;
            sink(&self.pending[..take])?;
            self.pending.drain(..take);
            remaining -= take as u64;
        }
        Ok(())
    }

    fn expect_end(&mut self) -> ManagerResult<()> {
        // Any leftover decrypted bytes or any further frame means the
        // archive carries data the manifest did not describe. Even a 1-3
        // byte remnant (a torn frame length) invalidates the terminal
        // state: only a byte-clean EOF passes.
        if !self.pending.is_empty() {
            return Err(format!(
                "Backup '{}' has trailing data the manifest does not describe",
                self.backup_id
            ));
        }
        let buffered = self
            .file
            .fill_buf()
            .map_err(|error| format!("Unable to read backup '{}': {error}", self.backup_id))?;
        if !buffered.is_empty() {
            return Err(format!(
                "Backup '{}' has {} trailing bytes the manifest does not describe",
                self.backup_id,
                buffered.len()
            ));
        }
        if self.fill_pending()? {
            return Err(format!(
                "Backup '{}' has trailing frames the manifest does not describe",
                self.backup_id
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sealing: validate, encrypt, write key/meta, self-verify.
// ---------------------------------------------------------------------------

/// Maximum base64 text accepted for one vault secret before decoding, so a
/// corrupt manifest cannot force a huge allocation.
const MAX_CRED_B64_CHARS: usize = 8 * 1024 * 1024;

fn validate_entries_for(
    entries: &[ManifestEntry],
    kind: Option<ManifestKind>,
) -> ManagerResult<()> {
    if entries.is_empty() {
        return Err("Refusing to seal an empty snapshot".into());
    }
    if entries.len() > MAX_ENTRIES {
        return Err(format!("Snapshot exceeds {MAX_ENTRIES} entries"));
    }
    // Seal and verify accept exactly the same byte classes: per-file and
    // aggregate caps are enforced here so the read side never allocates
    // what the writer could not have produced.
    let mut total: u64 = 0;
    for entry in entries {
        validate_rel(&entry.rel)?;
        if entry.size > MAX_FILE_BYTES {
            return Err(format!("Snapshot entry '{}' exceeds size caps", entry.rel));
        }
        total = total.saturating_add(entry.size);
        if total > MAX_TOTAL_BYTES {
            return Err("Snapshot exceeds the 64 GiB payload cap".into());
        }
        match entry.kind {
            EntryKind::File | EntryKind::Link | EntryKind::Dir => {
                let abs = entry.abs.as_ref().ok_or_else(|| {
                    format!(
                        "Snapshot entry '{}' is missing its original path",
                        entry.rel
                    )
                })?;
                if !abs.is_absolute() {
                    return Err(format!(
                        "Snapshot entry '{}' has a non-absolute path",
                        entry.rel
                    ));
                }
            }
            EntryKind::Profile => {
                if entry.abs.is_some() {
                    return Err(format!(
                        "Profile entry '{}' must not carry a path",
                        entry.rel
                    ));
                }
                if entry.size > MAX_PROFILE_BYTES {
                    return Err(format!("Profile entry '{}' exceeds size caps", entry.rel));
                }
            }
            EntryKind::Cred => {
                let target = entry.target.as_ref().ok_or_else(|| {
                    format!("Credential entry '{}' is missing its target", entry.rel)
                })?;
                if target.trim().is_empty() {
                    return Err(format!(
                        "Credential entry '{}' has an empty target",
                        entry.rel
                    ));
                }
                let secret_b64 = entry.secret_b64.as_ref().ok_or_else(|| {
                    format!("Credential entry '{}' is missing its secret", entry.rel)
                })?;
                // Bound the decode before allocating: base64 expands 4/3.
                if secret_b64.len() > MAX_CRED_B64_CHARS {
                    return Err(format!(
                        "Credential entry '{}' exceeds size caps",
                        entry.rel
                    ));
                }
                let secret = super::credentials::base64_decode(secret_b64)?;
                if secret.len() as u64 != entry.size {
                    return Err(format!("Credential entry '{}' size mismatch", entry.rel));
                }
                if hex_decode(&entry.sha256)
                    .map_err(|_| format!("Credential entry '{}' has a bad hash", entry.rel))?
                    != Sha256::digest(&secret).to_vec()
                {
                    return Err(format!("Credential entry '{}' hash mismatch", entry.rel));
                }
            }
        }
        if entry.kind == EntryKind::Dir && entry.size != 0 {
            return Err(format!(
                "Directory entry '{}' must not carry a size",
                entry.rel
            ));
        }
        if entry.kind == EntryKind::Link {
            let target = entry
                .link_target
                .as_ref()
                .ok_or_else(|| format!("Link entry '{}' is missing its target", entry.rel))?;
            if !target.is_absolute() {
                return Err(format!(
                    "Link entry '{}' has a non-absolute target",
                    entry.rel
                ));
            }
        }
        if entry.sha256.len() != 64 || hex_decode(&entry.sha256).is_err() {
            return Err(format!("Snapshot entry '{}' has a bad hash", entry.rel));
        }
    }
    match kind {
        // A legacy snapshot is a file tree plus directory/link markers at
        // its recorded original path: it must never carry vault secrets or
        // registry metadata, and restore enforces the same.
        Some(ManifestKind::Legacy) => {
            if entries
                .iter()
                .any(|entry| matches!(entry.kind, EntryKind::Cred | EntryKind::Profile))
            {
                return Err(
                    "Legacy snapshots must not carry credentials or registry metadata".into(),
                );
            }
        }
        // Account snapshots always lead with the registry metadata entry so
        // the streaming restore pass parses the restore identity (and the
        // vault-membership list) before any file or credential is written.
        Some(ManifestKind::Account) => {
            if !matches!(
                entries.first().map(|entry| entry.kind),
                Some(EntryKind::Profile)
            ) {
                return Err("Account snapshots must lead with the registry metadata entry".into());
            }
        }
        None => {}
    }
    Ok(())
}
#[allow(clippy::too_many_arguments)]
fn seal_archive(
    provider: ProviderId,
    account_id: &str,
    backup_id: &str,
    created_unix: u64,
    kind: ManifestKind,
    legacy_original_path: Option<PathBuf>,
    warnings: Vec<String>,
    entries: Vec<ManifestEntry>,
    payloads: Vec<EntryPayload>,
) -> ManagerResult<BackupInfo> {
    validate_backup_id(backup_id)?;
    validate_entries_for(&entries, Some(kind))?;
    if entries.len() != payloads.len() {
        return Err("Internal snapshot payload mismatch".into());
    }
    let provider_name = provider_key(provider)?.to_string();

    let manifest = FileManifest {
        format: ARCHIVE_VERSION,
        backup_id: backup_id.to_string(),
        provider: provider_name.clone(),
        account_id: account_id.to_string(),
        created_unix,
        kind,
        legacy_original_path: legacy_original_path.clone(),
        warnings,
        entries,
    };
    let mut manifest_json = serde_json::to_vec(&manifest)
        .map_err(|error| format!("Unable to encode snapshot: {error}"))?;
    if manifest_json.len() > MAX_MANIFEST_BYTES {
        manifest_json.zeroize();
        return Err("Snapshot manifest exceeds size caps".into());
    }
    let manifest_hash = sha256_hex(&manifest_json);

    let key = Zeroizing::new(random_bytes::<32>()?);
    let base_nonce: [u8; 12] = random_bytes()?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|error| format!("Unable to initialize backup encryption: {error:?}"))?;

    let account_dir = super::paths::backup_dir_for(provider, account_id)?;
    super::paths::ensure_dir(&account_dir)?;
    lock_to_current_user(&account_dir, true)?;
    let backup_dir = super::paths::join_within(&account_dir, backup_id)?;
    if backup_dir.exists() {
        manifest_json.zeroize();
        return Err(format!("Backup '{backup_id}' already exists"));
    }
    // Stage the directory itself first so a failed seal never leaves a
    // half-written backup behind.
    std::fs::create_dir(&backup_dir)
        .map_err(|error| format!("Unable to create backup '{backup_id}': {error}"))?;
    lock_to_current_user(&backup_dir, true)?;
    let sealed: ManagerResult<BackupInfo> = (|| {
        let data_path = backup_dir.join("data.uhb");
        {
            let data_file = std::fs::File::create(&data_path)
                .map_err(|error| format!("Unable to write backup archive: {error}"))?;
            let mut framer = Framer {
                cipher: &cipher,
                backup_id,
                base_nonce,
                seq: 0,
                chunk: Vec::with_capacity(FRAME_CHUNK),
                out: BufWriter::new(data_file),
            };
            framer
                .out
                .write_all(ARCHIVE_MAGIC)
                .map_err(|error| format!("Unable to write backup archive: {error}"))?;
            framer
                .out
                .write_all(&ARCHIVE_VERSION.to_le_bytes())
                .map_err(|error| format!("Unable to write backup archive: {error}"))?;
            framer
                .out
                .write_all(&base_nonce)
                .map_err(|error| format!("Unable to write backup archive: {error}"))?;
            let manifest_len = manifest_json.len() as u64;
            framer.feed(&manifest_len.to_le_bytes())?;
            // Clone-free path: feed borrows, zeroize happens below.
            framer.feed(&manifest_json)?;
            // Re-encode the manifest for payload ordering reference is
            // unnecessary: payloads align with the already-fed manifest.
            // Payloads align 1:1 with entries; Disk payloads stream exactly
            // the hashed `entry.size` so the manifest stays authoritative. A
            // file that changed between hashing and sealing fails the
            // post-write self-verify instead of silently carrying drift.
            for (entry, payload) in manifest.entries.iter().zip(payloads.iter()) {
                match payload {
                    EntryPayload::Inline(bytes) => framer.feed(bytes)?,
                    EntryPayload::Disk(path) => framer.feed_file(path, entry.size)?,
                    EntryPayload::None => {}
                }
            }
            let eof_marker = format!("UHBA-EOF:{manifest_hash}");
            framer.feed(eof_marker.as_bytes())?;
            framer.finish()?;
        }
        lock_to_current_user(&data_path, false)?;
        let wrapped = dpapi_protect(key.as_ref())?;
        let key_path = backup_dir.join("key.dpapi");
        write_persisted(&key_path, &wrapped, "backup key")?;
        lock_to_current_user(&key_path, false)?;
        let size_bytes = std::fs::metadata(&data_path)
            .map_err(|error| format!("Unable to stat backup archive: {error}"))?
            .len();
        let credential_targets: Vec<String> =
            serde_json::from_slice::<FileManifest>(&manifest_json)
                .map(|parsed| {
                    parsed
                        .entries
                        .iter()
                        .filter(|entry| entry.kind == EntryKind::Cred)
                        .filter_map(|entry| entry.target.clone())
                        .collect()
                })
                .unwrap_or_default();
        let info = BackupInfo {
            backup_id: backup_id.to_string(),
            provider,
            account_id: account_id.to_string(),
            created_unix,
            expires_unix: BackupInfo::expiry_for(created_unix),
            path: backup_dir.clone(),
            size_bytes,
            manifest_hash: manifest_hash.clone(),
            credential_targets,
        };
        let meta = BackupMetaFile {
            format: ARCHIVE_VERSION,
            info: info.clone(),
            legacy_original_path,
        };
        let meta_json = serde_json::to_vec_pretty(&meta)
            .map_err(|error| format!("Unable to encode backup metadata: {error}"))?;
        let meta_path = backup_dir.join("meta.json");
        write_persisted(&meta_path, &meta_json, "backup metadata")?;
        lock_to_current_user(&meta_path, false)?;
        Ok(info)
    })();
    // The streaming pass above borrowed the sealed manifest; free its entries
    // now that the archive is written (self-verify below re-reads from disk).
    drop(manifest);
    manifest_json.zeroize();
    drop(key);
    match sealed {
        Ok(info) => {
            // Self-verify by reread/decrypt before reporting success: cleanup
            // deletes live data only after a verified snapshot exists. A
            // failed self-verify removes the directory so no valid-looking
            // orphan survives a fresh-metadata race.
            if let Err(error) = verify_backup(backup_id) {
                let _ = std::fs::remove_dir_all(&backup_dir);
                return Err(error);
            }
            Ok(info)
        }
        Err(error) => {
            let _ = std::fs::remove_dir_all(&backup_dir);
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Reading: locate, verify, restore.
// ---------------------------------------------------------------------------

fn read_meta_file(dir: &Path) -> Option<BackupMetaFile> {
    let bytes = std::fs::read(dir.join("meta.json")).ok()?;
    serde_json::from_slice::<BackupMetaFile>(&bytes)
        .ok()
        .filter(|meta| {
            meta.format == ARCHIVE_VERSION
                && dir.join("data.uhb").is_file()
                && dir.join("key.dpapi").is_file()
        })
}

/// Locate a backup by id across every provider/account directory. The id is
/// validated before any scan so a hostile id can never escape the backups
/// root, and the returned directory is the enumerated one (identity-bound
/// by `bound_backup_info`) — never the plaintext-recorded metadata path.
fn locate_backup(backup_id: &str) -> ManagerResult<(PathBuf, BackupMetaFile)> {
    validate_backup_id(backup_id)?;
    for info in list_all_backups()? {
        if info.backup_id == backup_id {
            let meta_path = info.path.join("meta.json");
            let bytes = std::fs::read(&meta_path)
                .map_err(|error| format!("Unable to read backup '{backup_id}': {error}"))?;
            let meta: BackupMetaFile = serde_json::from_slice(&bytes)
                .map_err(|_| format!("Backup '{backup_id}' metadata is corrupt"))?;
            return Ok((info.path, meta));
        }
    }
    Err(format!("Unknown backup '{backup_id}'"))
}

fn collector_to_seal_parts(
    collector: Collector,
) -> (Vec<String>, Vec<ManifestEntry>, Vec<EntryPayload>) {
    let warnings = collector.warnings;
    let mut entries = Vec::with_capacity(collector.entries.len());
    let mut payloads = Vec::with_capacity(collector.entries.len());
    for (entry, payload) in collector.entries {
        entries.push(entry);
        payloads.push(payload);
    }
    (warnings, entries, payloads)
}

fn provider_from_key(key: &str) -> ManagerResult<ProviderId> {
    ProviderId::from_key(key).ok_or_else(|| format!("Backup names unknown provider '{key}'"))
}

/// Read and fully authenticate a backup: header, frame tags and order,
/// manifest hash, every entry hash, and the EOF marker. Returns the
/// authenticated manifest; key material never leaves this function.
fn verify_stream(dir: &Path, meta: &BackupMetaFile) -> ManagerResult<FileManifest> {
    verify_stream_capturing(dir, meta, None)
}

/// [`verify_stream`] that also decodes the authenticated registry metadata
/// entry (Account snapshots lead with it) into `profile`.
///
/// Restore must know the exact stored identity and source — not merely a
/// hash — before it may claim the registry or write one byte, so the record
/// is decoded from the same authenticated pass instead of a second read.
fn verify_stream_capturing(
    dir: &Path,
    meta: &BackupMetaFile,
    profile: Option<&mut Option<crate::accounts::AccountProfile>>,
) -> ManagerResult<FileManifest> {
    let key_blob = std::fs::read(dir.join("key.dpapi")).map_err(|error| {
        format!(
            "Unable to read backup key for '{}': {error}",
            meta.info.backup_id
        )
    })?;
    let mut key = dpapi_unprotect(&key_blob)?;
    if key.len() != 32 {
        key.zeroize();
        return Err(format!(
            "Backup '{}' has an invalid data key",
            meta.info.backup_id
        ));
    }
    let result = verify_stream_with_key(dir, meta, &key, profile);
    key.zeroize();
    result
}

fn verify_stream_with_key(
    dir: &Path,
    meta: &BackupMetaFile,
    key: &[u8],
    mut profile: Option<&mut Option<crate::accounts::AccountProfile>>,
) -> ManagerResult<FileManifest> {
    let backup_id = &meta.info.backup_id;
    let mut reader = FrameReader::open(&dir.join("data.uhb"), key, backup_id)?;
    let manifest_len = reader.take(8).and_then(|bytes| {
        TryInto::<[u8; 8]>::try_into(bytes)
            .map(u64::from_le_bytes)
            .map_err(|_| format!("Backup '{backup_id}' has a corrupt manifest length"))
    })?;
    if manifest_len == 0 || manifest_len > MAX_MANIFEST_BYTES as u64 {
        return Err(format!(
            "Backup '{backup_id}' has an impossible manifest length"
        ));
    }
    let mut manifest_json = reader.take(manifest_len as usize)?;
    let manifest: FileManifest = serde_json::from_slice(&manifest_json)
        .map_err(|_| format!("Backup '{backup_id}' manifest is corrupt"))?;
    if manifest.format != ARCHIVE_VERSION
        || manifest.backup_id != *backup_id
        || manifest.provider != provider_key(meta.info.provider).unwrap_or("?")
        || manifest.account_id != meta.info.account_id
    {
        return Err(format!(
            "Backup '{backup_id}' manifest does not match its metadata"
        ));
    }
    let manifest_hash = sha256_hex(&manifest_json);
    if manifest_hash != meta.info.manifest_hash {
        return Err(format!(
            "Backup '{backup_id}' manifest hash mismatch (tampered)"
        ));
    }
    validate_entries_for(&manifest.entries, Some(manifest.kind))?;
    for entry in &manifest.entries {
        match entry.kind {
            EntryKind::File | EntryKind::Profile => {
                if entry.size > MAX_FILE_BYTES {
                    return Err(format!(
                        "Backup '{backup_id}' entry '{}' exceeds caps",
                        entry.rel
                    ));
                }
                // Incremental hash over bounded frame chunks: even a
                // multi-gigabyte file never materializes in memory. The one
                // exception is the registry metadata entry, whose decoded
                // value restore needs before any write: it is already capped
                // at MAX_PROFILE_BYTES and holds paths only.
                let mut hasher = Sha256::new();
                let mut metadata: Option<Vec<u8>> = (profile.is_some()
                    && entry.kind == EntryKind::Profile
                    && entry.size <= MAX_PROFILE_BYTES)
                    .then(|| Vec::with_capacity(entry.size as usize));
                reader.consume_exact(entry.size, |chunk| {
                    hasher.update(chunk);
                    if let Some(bytes) = metadata.as_mut() {
                        bytes.extend_from_slice(chunk);
                    }
                    Ok(())
                })?;
                if hex_encode(&hasher.finalize()) != entry.sha256 {
                    return Err(format!(
                        "Backup '{backup_id}' entry '{}' hash mismatch (tampered)",
                        entry.rel
                    ));
                }
                if let (Some(bytes), Some(slot)) = (metadata, profile.as_deref_mut()) {
                    *slot = Some(serde_json::from_slice(&bytes).map_err(|_| {
                        format!("Backup '{backup_id}' account metadata is corrupt")
                    })?);
                }
            }
            EntryKind::Cred => {
                let secret_b64 = entry.secret_b64.as_ref().ok_or_else(|| {
                    format!(
                        "Backup '{backup_id}' credential '{}' lost its secret",
                        entry.rel
                    )
                })?;
                // Decoded only to re-hash; the plaintext secret never leaves
                // this scope and is zeroized immediately.
                let mut secret = super::credentials::base64_decode(secret_b64)?;
                let matches = sha256_hex(&secret) == entry.sha256;
                secret.zeroize();
                if !matches {
                    return Err(format!(
                        "Backup '{backup_id}' credential '{}' hash mismatch (tampered)",
                        entry.rel
                    ));
                }
            }
            EntryKind::Link | EntryKind::Dir => {}
        }
    }
    let expected_marker = format!("UHBA-EOF:{manifest_hash}");
    let marker = reader.take(expected_marker.len())?;
    if marker != expected_marker.as_bytes() {
        return Err(format!(
            "Backup '{backup_id}' is missing its end marker (truncated)"
        ));
    }
    reader.expect_end()?;
    // The manifest embeds credential secrets; scrub it before returning.
    manifest_json.zeroize();
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// Public contract API.
// ---------------------------------------------------------------------------

/// Snapshot an account with default discovery (explicit profile paths only;
/// provider-default sources contribute metadata plus owned vault entries and
/// are never walked globally). Refuses live Desktop profiles — stop/close
/// the account first (cleanup does this under the per-account guard).
pub fn create_backup(ctx: &AccountContext) -> ManagerResult<BackupInfo> {
    let created = now_unix();
    let backup_id = generate_backup_id(created)?;
    create_backup_with_id(ctx, &backup_id, created)
}

/// Deterministic variant of [`create_backup`] (explicit id/timestamp) for
/// tests and fixtures.
pub fn create_backup_with_id(
    ctx: &AccountContext,
    backup_id: &str,
    created_unix: u64,
) -> ManagerResult<BackupInfo> {
    // Serialize against concurrent open/poller/cleanup work on this
    // account; cleanup's own calls nest on the same thread (recursive
    // Windows mutex, balanced by the guards' drops). Then re-validate the
    // context against fresh registry state before touching anything.
    let _guard = super::state::AccountOperationGuard::acquire(
        ctx.provider,
        &ctx.profile.id,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    recheck_ctx(ctx)?;
    validate_backup_id(backup_id)?;
    provider_key(ctx.provider)?;
    let collector = collect_account_snapshot(ctx)?;
    let (warnings, entries, payloads) = collector_to_seal_parts(collector);
    seal_archive(
        ctx.provider,
        &ctx.profile.id,
        backup_id,
        created_unix,
        ManifestKind::Account,
        None,
        warnings,
        entries,
        payloads,
    )
}

fn generate_backup_id(created_unix: u64) -> ManagerResult<String> {
    // Randomness failures propagate: a constant fallback would collide two
    // creates within the same second and mask an entropy outage.
    let random = hex_encode(&random_bytes::<6>()?);
    Ok(format!("bk_{created_unix}_{random}"))
}

/// Snapshot exactly `targets` (files and/or directories) for per-scope
/// cleanup: the caller (cleanup preview, validated and owned) selects the
/// precise files being deleted so unrelated data is never pulled in.
/// External Default data is only snapshotted through this explicit path —
/// default discovery never walks global locations.
pub fn create_backup_for_paths(
    ctx: &AccountContext,
    targets: &[PathBuf],
) -> ManagerResult<BackupInfo> {
    let created = now_unix();
    let backup_id = generate_backup_id(created)?;
    create_backup_for_paths_with_id(ctx, targets, &backup_id, created)
}
/// Deterministic variant of [`create_backup_for_paths`] for tests/fixtures.
pub fn create_backup_for_paths_with_id(
    ctx: &AccountContext,
    targets: &[PathBuf],
    backup_id: &str,
    created_unix: u64,
) -> ManagerResult<BackupInfo> {
    let _guard = super::state::AccountOperationGuard::acquire(
        ctx.provider,
        &ctx.profile.id,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    recheck_ctx(ctx)?;
    validate_backup_id(backup_id)?;
    provider_key(ctx.provider)?;
    ensure_quiesced(ctx)?;
    let mut collector = collect_paths_snapshot(targets)?;
    // Confirmed-owned vault entries ride along: cleanup deletes them via
    // `credentials::delete_owned` after the snapshot, so the targeted backup
    // must cover them exactly like the default snapshot does.
    for (index, record) in super::credentials::capture_owned(ctx)?.iter().enumerate() {
        collector.push_cred(index, record)?;
    }
    let (warnings, mut entries, mut payloads) = collector_to_seal_parts(collector);
    // A snapshot that covers no file/link/dir entries is only meaningful
    // when bound to a registered account (vault-only logout, empty
    // directories already deleted, registry-only retry). Managed accounts
    // always qualify; External accounts qualify only when they declare
    // confirmed-owned vault targets (credential-only explicit reset).
    // Anything else would be an unbound archive — refuse it.
    let covers_data = entries.iter().any(|entry| {
        matches!(
            entry.kind,
            EntryKind::File | EntryKind::Link | EntryKind::Dir
        )
    });
    if !covers_data {
        authenticate_metadata_only_snapshot(ctx)?;
    }
    let profile_json = serde_json::to_vec(&ctx.profile)
        .map_err(|error| format!("Unable to serialize account metadata: {error}"))?;
    let profile_sha = sha256_hex(&profile_json);
    entries.insert(
        0,
        ManifestEntry {
            rel: "meta/profile.json".to_string(),
            kind: EntryKind::Profile,
            abs: None,
            target: None,
            link_target: None,
            link_is_dir: false,
            size: profile_json.len() as u64,
            sha256: profile_sha,
            secret_b64: None,
        },
    );
    payloads.insert(0, EntryPayload::Inline(profile_json));
    seal_archive(
        ctx.provider,
        &ctx.profile.id,
        backup_id,
        created_unix,
        ManifestKind::Account,
        None,
        warnings,
        entries,
        payloads,
    )
}

/// Human-readable account directory scan for one provider/account pair.
/// Corrupt or incomplete backup dirs are skipped, never returned. Listed
/// paths always come from directory enumeration — the plaintext `meta.json`
/// `path` is never trusted, so a tampered tombstone cannot redirect reads,
/// restores, or purges at an unrelated directory.
pub fn list_backups(provider: ProviderId, account_id: &str) -> ManagerResult<Vec<BackupInfo>> {
    let account_dir = super::paths::backup_dir_for(provider, account_id)?;
    let mut infos = Vec::new();
    let listing = match std::fs::read_dir(&account_dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(infos),
        Err(error) => {
            return Err(format!(
                "Unable to list backups for '{account_id}': {error}"
            ));
        }
    };
    for child in listing {
        let child = child.map_err(|error| format!("Unable to list backups: {error}"))?;
        let dir = child.path();
        if !dir.is_dir() || super::paths::is_reparse_point(&dir) {
            continue;
        }
        if let Some(info) = bound_backup_info(&dir, provider, account_id) {
            infos.push(info);
        }
    }
    infos.sort_by(|left, right| {
        left.created_unix
            .cmp(&right.created_unix)
            .then_with(|| left.backup_id.cmp(&right.backup_id))
    });
    Ok(infos)
}

/// Bind an enumerated backup directory to its metadata: the directory
/// names must agree with the manifest identity (provider/account/backup),
/// and the returned [`BackupInfo::path`] is the enumerated directory —
/// never the plaintext-recorded path.
fn bound_backup_info(dir: &Path, provider: ProviderId, account_id: &str) -> Option<BackupInfo> {
    let meta = read_meta_file(dir)?;
    let backup_id = dir.file_name()?.to_string_lossy().into_owned();
    if meta.info.provider != provider
        || meta.info.account_id != account_id
        || meta.info.backup_id != backup_id
    {
        return None;
    }
    validate_backup_id(&backup_id).ok()?;
    let mut info = meta.info;
    info.path = dir.to_path_buf();
    Some(info)
}

/// Every complete backup across all providers/accounts, oldest first.
/// Migration uses this to offer legacy snapshots alongside account ones.
/// Paths are enumeration-derived and identity-bound (see `list_backups`).
pub fn list_all_backups() -> ManagerResult<Vec<BackupInfo>> {
    let Some(root) = super::paths::fork_backups_dir() else {
        return Ok(Vec::new());
    };
    let mut infos = Vec::new();
    let Ok(providers) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    for provider_dir in providers.filter_map(|child| child.ok().map(|child| child.path())) {
        let provider_key = provider_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(provider) = ProviderId::from_key(&provider_key) else {
            continue;
        };
        if super::paths::is_reparse_point(&provider_dir) {
            continue;
        }
        let Ok(accounts) = std::fs::read_dir(&provider_dir) else {
            continue;
        };
        for account_dir in accounts.filter_map(|child| child.ok().map(|child| child.path())) {
            if !account_dir.is_dir() || super::paths::is_reparse_point(&account_dir) {
                continue;
            }
            let account_id = account_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let Ok(backups) = std::fs::read_dir(&account_dir) else {
                continue;
            };
            for backup_dir in backups.filter_map(|child| child.ok().map(|child| child.path())) {
                if !backup_dir.is_dir() || super::paths::is_reparse_point(&backup_dir) {
                    continue;
                }
                if let Some(info) = bound_backup_info(&backup_dir, provider, &account_id) {
                    infos.push(info);
                }
            }
        }
    }
    infos.sort_by(|left, right| {
        left.created_unix
            .cmp(&right.created_unix)
            .then_with(|| left.backup_id.cmp(&right.backup_id))
    });
    Ok(infos)
}

/// Authenticated coverage attestation for one backup. Re-authenticates the
/// archive exactly like [`verify_backup`] and returns the distinct recorded
/// roots (every File/Link/Dir `abs`, canonical spelling) plus the covered
/// vault targets. Cleanup compares each requested root against `roots`
/// (after normalizing requests with [`canonical_requested_root`]) instead
/// of trusting an unconditional covered flag; unrequested extra roots are
/// visible here rather than silently implied. `credential_targets` is the
/// proof that a planned vault deletion is restorable: cleanup refuses when a
/// currently present owned target is missing from it.
#[derive(Clone, Debug, Default)]
pub struct CoverageAttestation {
    pub roots: Vec<PathBuf>,
    pub credential_targets: Vec<String>,
}

pub fn coverage_attestation(backup_id: &str) -> ManagerResult<CoverageAttestation> {
    let (dir, meta) = locate_backup(backup_id)?;
    let _guard = super::state::AccountOperationGuard::acquire(
        meta.info.provider,
        &meta.info.account_id,
        BACKUP_VERIFY_TIMEOUT_MS,
    )?;
    let manifest = verify_stream(&dir, &meta)?;
    let mut roots = BTreeSet::new();
    let mut credential_targets = BTreeSet::new();
    for entry in &manifest.entries {
        match entry.kind {
            EntryKind::File | EntryKind::Link | EntryKind::Dir => {
                if let Some(abs) = &entry.abs {
                    roots.insert(abs.clone());
                }
            }
            EntryKind::Cred => {
                if let Some(target) = &entry.target {
                    credential_targets.insert(target.clone());
                }
            }
            EntryKind::Profile => {}
        }
    }
    Ok(CoverageAttestation {
        roots: roots.into_iter().collect(),
        credential_targets: credential_targets.into_iter().collect(),
    })
}

/// Fully re-authenticate a backup (header, frames, manifest, entry hashes,
/// EOF marker). Reads nothing into live locations; returns the stored info.
pub fn verify_backup(backup_id: &str) -> ManagerResult<BackupInfo> {
    let (dir, meta) = locate_backup(backup_id)?;
    let _guard = super::state::AccountOperationGuard::acquire(
        meta.info.provider,
        &meta.info.account_id,
        BACKUP_VERIFY_TIMEOUT_MS,
    )?;
    let manifest = verify_stream(&dir, &meta)?;
    let _ = manifest;
    Ok(meta.info)
}
/// True when `abs` is already the link the archive recorded: a live reparse
/// point whose target resolves to the same shared-library target. Used for
/// cleanup-preserved skill junctions, which survive FullReset in place so a
/// restore must not refuse them as conflicts. Anything else — a real file
/// or dir, a non-link, an unknown target, a divergent target — returns
/// false and the caller refuses.
fn identical_preserved_link(abs: &Path, recorded: &Path, shared_roots: &[PathBuf]) -> bool {
    if !super::paths::is_reparse_point(abs) {
        return false;
    }
    let live_target = match std::fs::read_link(abs) {
        Ok(target) => target,
        Err(_) => return false,
    };
    let live_target = resolve_link_target(abs, live_target);
    let recorded = resolve_link_target(abs, recorded.to_path_buf());
    if live_target != recorded {
        return false;
    }
    shared_roots
        .iter()
        .any(|root| super::paths::is_within(root, &recorded) || recorded == *root)
}

/// True when two entries claiming the same physical destination do not
/// describe the same object. Identical duplicates (an overlapping walked root
/// and an explicit skill-link target, in different spellings) collapse to one
/// creation; divergent duplicates are refused before any write rather than
/// silently resolved in favor of one entry.
fn destinations_diverge(first: &ManifestEntry, second: &ManifestEntry) -> bool {
    if first.kind != second.kind {
        return true;
    }
    match first.kind {
        EntryKind::File => first.size != second.size || first.sha256 != second.sha256,
        EntryKind::Link => {
            first.link_target != second.link_target || first.link_is_dir != second.link_is_dir
        }
        // One directory marker per destination needs no payload comparison.
        EntryKind::Dir | EntryKind::Profile | EntryKind::Cred => false,
    }
}

/// Path bound for one restore entry, checked before any write. The shared
/// skills library is link-target-only by design: file/dir bytes are never
/// restored into it. Legacy snapshots additionally stay inside their
/// recorded original root — never into an active account tree.
fn check_restore_abs(
    entry: &ManifestEntry,
    backup_id: &str,
    kind: ManifestKind,
    legacy_root: Option<&PathBuf>,
    shared_roots: &[PathBuf],
) -> ManagerResult<PathBuf> {
    let abs = entry
        .abs
        .as_ref()
        .ok_or_else(|| format!("Backup '{backup_id}' entry '{}' lost its path", entry.rel))?;
    if !abs.is_absolute() {
        return Err(format!(
            "Backup '{backup_id}' entry '{}' has a non-absolute path",
            entry.rel
        ));
    }
    if shared_roots
        .iter()
        .any(|root| super::paths::is_within(root, abs))
    {
        return Err(format!(
            "Refusing to restore '{}' inside the shared skills library",
            abs.display()
        ));
    }
    if kind == ManifestKind::Legacy {
        let root = legacy_root.ok_or_else(|| {
            format!("Backup '{backup_id}' legacy snapshot lost its original path")
        })?;
        if !super::paths::is_within(root, abs) {
            return Err(format!(
                "Refusing to restore '{}' outside the recorded legacy root",
                abs.display()
            ));
        }
    }
    Ok(abs.clone())
}

/// Best-effort rollback of a failed restore. Removes only what the restore
/// journal recorded (files, links, created dirs, written vault targets);
/// the preflight guarantees none of these existed beforehand, so rollback
/// cannot destroy live data. All errors are swallowed — the original
/// restore error is returned instead. Links use a dual-try removal (file
/// then dir): directory junctions and symlinks report different kinds.
fn rollback_restore(files: &[PathBuf], links: &[PathBuf], dirs: &[PathBuf], creds: &[String]) {
    for target in creds {
        let _ = super::credentials::delete_single(target);
    }
    for link in links.iter().rev() {
        if std::fs::remove_file(link).is_err() {
            let _ = std::fs::remove_dir(link);
        }
    }
    for file in files.iter().rev() {
        let _ = std::fs::remove_file(file);
    }
    // `remove_dir` only removes empty dirs, so a non-empty (foreign) dir
    // is never touched. Repeat to a fixpoint: parents become empty only
    // after their journaled children are gone, regardless of journal order.
    for _ in 0..dirs.len().max(1) {
        let mut progress = false;
        for dir in dirs.iter().rev() {
            if std::fs::remove_dir(dir).is_ok() {
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
}

/// Every live path and vault target a restore would write, in canonical
/// spelling. Compared against other registered accounts' sources before any
/// byte is written: single ownership means one account never writes into
/// another account's auth/config root or vault target.
struct RestoreTargets {
    destinations: Vec<String>,
    credentials: Vec<String>,
}

impl RestoreTargets {
    fn from_manifest(manifest: &FileManifest) -> Self {
        let mut destinations = Vec::new();
        let mut credentials = Vec::new();
        for entry in &manifest.entries {
            match entry.kind {
                EntryKind::File | EntryKind::Dir | EntryKind::Link => {
                    if let Some(abs) = entry.abs.as_ref() {
                        destinations.push(super::native::canonical_spelling(abs));
                    }
                }
                EntryKind::Cred => {
                    if let Some(target) = entry.target.as_ref() {
                        credentials.push(target.clone());
                    }
                }
                EntryKind::Profile => {}
            }
        }
        Self {
            destinations,
            credentials,
        }
    }
}

/// Roots owned by one registered profile, resolved from stored settings only
/// (no credential or token reads): its explicit sources when it has any,
/// otherwise the virtual Default roots
/// (`lifecycle::canonical_default_roots`) for the external Default, or the
/// owned managed root `<profiles>/<provider>/<id>` for a Managed profile
/// created by us.
fn profile_source_roots(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
) -> Vec<String> {
    let mut roots = Vec::new();
    for field in [
        &profile.desktop_dir,
        &profile.config_dir,
        &profile.credentials_path,
    ] {
        if !field.trim().is_empty() {
            roots.push(super::native::canonical_spelling(Path::new(field.trim())));
        }
    }
    if !roots.is_empty() {
        return roots;
    }
    if profile.id.eq_ignore_ascii_case("default") || !profile.is_managed() {
        return super::lifecycle::canonical_default_roots(provider);
    }
    super::paths::managed_account_dir(provider, &profile.id)
        .map(|dir| super::native::canonical_spelling(&dir))
        .into_iter()
        .collect()
}

/// Stored record serialization matches the authenticated metadata hash: the
/// exact same-id/source profile this restore would re-insert.
fn registered_record_matches(
    profile: &crate::accounts::AccountProfile,
    snapshot_sha: &str,
) -> bool {
    if snapshot_sha.is_empty() {
        return false;
    }
    serde_json::to_vec(profile)
        .map(|bytes| sha256_hex(&bytes) == snapshot_sha)
        .unwrap_or(false)
}

fn snapshot_profile_sha(manifest: &FileManifest) -> &str {
    manifest
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Profile)
        .map(|entry| entry.sha256.as_str())
        .unwrap_or("")
}

/// Cross-account ownership conflicts for one restore.
///
/// Rejected: any File/Dir/Link destination overlapping ANOTHER registered
/// profile's source or owned root (both providers, disabled included), the
/// target provider's canonical Default root for a non-Default archive, and
/// any vault target another profile already owns. Only the exact
/// authenticated same-id/source record is exempt, so the FullReset Default
/// and logout restores that keep their own record still succeed.
///
/// This is what stops a stale snapshot from writing auth/history into a root
/// that was deleted with its account and then adopted by a new one: the old
/// id is gone from the registry, but the root is somebody else's now.
fn ownership_conflicts(
    settings: &crate::app_settings::SettingsFile,
    provider: ProviderId,
    restored: Option<&crate::accounts::AccountProfile>,
    snapshot_sha: &str,
    targets: &RestoreTargets,
) -> Vec<String> {
    let mut conflicts = Vec::new();
    let restores_default =
        restored.is_some_and(|profile| profile.id.eq_ignore_ascii_case("default"));
    if !restores_default && settings.accounts.get(provider).is_some() {
        for root in super::lifecycle::canonical_default_roots(provider) {
            if targets
                .destinations
                .iter()
                .any(|path| super::native::overlaps(path, &root))
            {
                conflicts.push(format!("canonical Default root '{root}'"));
            }
        }
    }
    for probe in [ProviderId::Claude, ProviderId::Codex] {
        let Some(accounts) = settings.accounts.get(probe) else {
            continue;
        };
        for profile in &accounts.profiles {
            let same_authenticated = probe == provider
                && restored.is_some_and(|restored| restored.id == profile.id)
                && registered_record_matches(profile, snapshot_sha);
            if same_authenticated {
                continue;
            }
            for root in profile_source_roots(probe, profile) {
                if targets
                    .destinations
                    .iter()
                    .any(|path| super::native::overlaps(path, &root))
                {
                    conflicts.push(format!(
                        "path owned by another registered account '{}' ('{root}')",
                        profile.id
                    ));
                    break;
                }
            }
            for target in &targets.credentials {
                if profile
                    .credential_targets
                    .iter()
                    .any(|owned| owned == target)
                {
                    conflicts.push(format!(
                        "credential '{target}' owned by account '{}'",
                        profile.id
                    ));
                }
            }
        }
    }
    conflicts
}

fn restore_conflict_message(backup_id: &str, conflicts: &[String]) -> String {
    format!(
        "Refusing to restore backup '{backup_id}': {}; remove or rename them first",
        conflicts.join(", ")
    )
}

fn provider_accounts_mut(
    settings: &mut crate::app_settings::SettingsFile,
    provider: ProviderId,
) -> ManagerResult<&mut crate::accounts::ProviderAccounts> {
    match provider {
        ProviderId::Claude => Ok(&mut settings.accounts.claude),
        ProviderId::Codex => Ok(&mut settings.accounts.codex),
        _ => Err("This provider does not support account snapshots".into()),
    }
}

/// Claim the restored registry identity BEFORE the write pass.
///
/// The conflict scan reads a settings snapshot that a concurrent
/// create/import can invalidate; those operations validate and commit under
/// the same settings mutex, so inserting the exact restored record here (one
/// short mutex transaction, no I/O) is what makes their collision gates
/// observe the claim. The ownership rule is re-checked against the freshly
/// loaded registry inside that same transaction, so a root adopted between
/// preflight and writes also refuses the restore. The caller keeps the
/// per-account guard across this call.
///
/// Returns `true` only when this call inserted the record — the one
/// reservation a failed restore may undo. A pre-existing record that already
/// byte-matches the authenticated snapshot (FullReset keeping the Default,
/// logout keeping the account) is left exactly as it is.
fn reserve_restored_profile(
    provider: ProviderId,
    profile: &crate::accounts::AccountProfile,
    snapshot_sha: &str,
    targets: &RestoreTargets,
    backup_id: &str,
) -> ManagerResult<bool> {
    if snapshot_sha.is_empty() {
        return Err(format!(
            "Backup '{backup_id}' for account '{}' lost its metadata hash",
            profile.id
        ));
    }
    super::state::mutate_settings(|settings| {
        let conflicts =
            ownership_conflicts(settings, provider, Some(profile), snapshot_sha, targets);
        if !conflicts.is_empty() {
            return Err(restore_conflict_message(backup_id, &conflicts));
        }
        let accounts = provider_accounts_mut(settings, provider)?;
        if let Some(existing) = accounts
            .profiles
            .iter()
            .find(|existing| existing.id == profile.id)
        {
            return if registered_record_matches(existing, snapshot_sha) {
                Ok(false)
            } else {
                Err(format!(
                    "Refusing to restore backup '{backup_id}': registered account id '{}' no longer matches",
                    profile.id
                ))
            };
        }
        accounts.used_ids.insert(profile.id.clone());
        accounts.profiles.push(profile.clone());
        Ok(true)
    })
}

/// Drop the reservation a failed restore inserted. Only a record that still
/// byte-matches the reserved profile is removed: a pre-existing matching
/// record (never inserted here) and a record another writer has since
/// changed are both left alone. `used_ids` stays retired — a claimed id is
/// never handed to a new account.
fn release_restored_profile(provider: ProviderId, profile: &crate::accounts::AccountProfile) {
    let Ok(reserved_sha) = serde_json::to_vec(profile).map(|bytes| sha256_hex(&bytes)) else {
        return;
    };
    let _ = super::state::mutate_settings(|settings| -> ManagerResult<()> {
        let accounts = provider_accounts_mut(settings, provider)?;
        accounts.profiles.retain(|existing| {
            !(existing.id == profile.id && registered_record_matches(existing, &reserved_sha))
        });
        Ok(())
    });
}

/// Restore a verified backup to its original paths and account id.
///
/// Refuses the whole restore — writing nothing — when any live conflict
/// exists: an occupied file/link path, an existing vault target, a distinct
/// already-registered account id, or a destination/vault target that belongs
/// to another registered profile. Single ownership is enforced across both
/// providers (disabled profiles included) using the same source resolution
/// as create/import: explicit profile paths, the owned managed root, or the
/// canonical Default roots for the external Default. Only the exact
/// authenticated same-id/source record is exempt, so a non-Default archive
/// can never write into a canonical Default tree.
///
/// Account backups recreate files, skill links (re-validated against the
/// shared library root), owned vault entries, and the registry entry
/// (`used_ids` retained, never reused). The registry identity is reserved
/// (short settings-mutex transaction, ownership re-checked on the fresh
/// view) after authentication and before the first write, so a concurrent
/// create/import cannot adopt the same source mid-restore; a failed restore
/// removes only that reservation. A stored profile that byte-matches the
/// authenticated snapshot (FullReset keeping the Default record, logout
/// keeping the account) is left in place. Legacy backups restore inside
/// their recorded original legacy root only and touch neither the registry
/// nor the vault — but they still may not write into a root another
/// registered profile has since adopted.
///
/// Overlapping roots can describe one physical destination twice (a walked
/// Desktop tree and an explicit skill-link target, in different spellings).
/// Each destination is created once: byte-identical or same-kind duplicates
/// collapse to the first entry, and divergent duplicates refuse the restore
/// before any write.
pub fn restore_backup(backup_id: &str) -> ManagerResult<()> {
    let (dir, meta) = locate_backup(backup_id)?;
    // Hold the per-account guard from verification through the registry
    // commit: provider/id come from the enumeration-bound identity, so a
    // concurrent open/poller/cleanup on the same account serializes here
    // instead of racing the restore's writes.
    let _guard = super::state::AccountOperationGuard::acquire(
        meta.info.provider,
        &meta.info.account_id,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    // Authenticate everything before touching live state. The authenticated
    // registry metadata is decoded from the same pass: the identity and
    // source this restore claims must be known before the conflict scan.
    let mut snapshot_profile: Option<crate::accounts::AccountProfile> = None;
    let manifest = verify_stream_capturing(&dir, &meta, Some(&mut snapshot_profile))?;
    if manifest.kind == ManifestKind::Account {
        let profile = snapshot_profile
            .as_ref()
            .ok_or_else(|| format!("Backup '{backup_id}' lost its account metadata"))?;
        if profile.id != manifest.account_id {
            return Err(format!("Backup '{backup_id}' account metadata id mismatch"));
        }
    }

    // --- Conflict scan (no writes yet) -------------------------------------
    // Occupied file paths refuse the whole restore: the restore never
    // overwrites live data and never silently claims a skipped entry was
    // restored. The one exception is a preserved identical skill link:
    // cleanup keeps validated shared-library junctions in place, so a
    // restore that meets the same inode (same shared-library target) skips
    // recreation — anything divergent still refuses. Existing directories
    // are structural and allowed; a blocked dir refuses.
    let shared_roots = shared_library_roots();
    let mut conflicts = Vec::new();
    // Entries whose live inode already matches the recorded one (a preserved
    // skill link left in place by cleanup) need no recreation.
    let mut preserved: BTreeSet<usize> = BTreeSet::new();
    // Overlapping roots may describe one physical destination twice (a walked
    // parent tree and an explicit linked-skill target, in two spellings).
    // The first entry owns the single creation; identical later entries are
    // skipped and divergent ones refuse the whole restore before any write.
    let mut first_destination: BTreeMap<String, usize> = BTreeMap::new();
    let mut duplicates: BTreeSet<usize> = BTreeSet::new();
    for (index, entry) in manifest.entries.iter().enumerate() {
        match entry.kind {
            EntryKind::File | EntryKind::Link | EntryKind::Dir => {
                let abs = check_restore_abs(
                    entry,
                    backup_id,
                    manifest.kind,
                    manifest.legacy_original_path.as_ref(),
                    &shared_roots,
                )?;
                let key = destination_key(&abs);
                if let Some(&first) = first_destination.get(&key) {
                    if destinations_diverge(&manifest.entries[first], entry) {
                        // Same physical path, different recorded object:
                        // never pick one silently, and never overwrite.
                        return Err(format!(
                            "Backup '{backup_id}' records conflicting entries for '{}'",
                            abs.display()
                        ));
                    }
                    duplicates.insert(index);
                    continue;
                }
                first_destination.insert(key, index);
                match entry.kind {
                    EntryKind::File => {
                        if std::fs::symlink_metadata(&abs).is_ok() {
                            conflicts.push(format!("occupied '{}'", abs.display()));
                        }
                    }
                    EntryKind::Link => {
                        if std::fs::symlink_metadata(&abs).is_ok() {
                            let recorded = entry.link_target.as_ref().ok_or_else(|| {
                                format!("Backup '{backup_id}' link '{}' lost its target", entry.rel)
                            })?;
                            if identical_preserved_link(&abs, recorded, &shared_roots) {
                                preserved.insert(index);
                            } else {
                                conflicts.push(format!("occupied '{}'", abs.display()));
                            }
                        }
                    }
                    EntryKind::Dir => match std::fs::symlink_metadata(&abs) {
                        Ok(metadata) => {
                            if super::paths::is_reparse_point(&abs)
                                || !metadata.file_type().is_dir()
                            {
                                conflicts.push(format!("occupied '{}'", abs.display()));
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!("Unable to stat '{}': {error}", abs.display()));
                        }
                    },
                    EntryKind::Cred | EntryKind::Profile => {
                        unreachable!("credential/profile entries carry no filesystem destination")
                    }
                }
            }
            EntryKind::Cred => {
                let target = entry.target.as_ref().ok_or_else(|| {
                    format!(
                        "Backup '{backup_id}' credential '{}' lost its target",
                        entry.rel
                    )
                })?;
                if super::credentials::credential_exists(target)? {
                    conflicts.push(format!("live credential '{target}'"));
                }
            }
            EntryKind::Profile => {}
        }
    }
    // Registry + ownership. An already-registered id is only tolerated when
    // the stored profile identity/source exactly matches the authenticated
    // snapshot (FullReset keeps the Default record; logout keeps the account
    // while only auth files are deleted). Anything distinct still refuses.
    //
    // Ownership: a snapshot may only write into roots and vault targets that
    // belong to its own record. A root deleted with its account and later
    // adopted by another registered profile (same empty dir, different id)
    // belongs to that profile now, so the stale snapshot is refused before
    // any write instead of writing auth/history into live foreign data.
    let target: ProviderId = provider_from_key(&manifest.provider)?;
    let settings = super::state::load_fresh();
    let snapshot_sha = snapshot_profile_sha(&manifest);
    let restore_targets = RestoreTargets::from_manifest(&manifest);
    conflicts.extend(ownership_conflicts(
        &settings,
        target,
        snapshot_profile.as_ref(),
        snapshot_sha,
        &restore_targets,
    ));
    if manifest.kind == ManifestKind::Account {
        let accounts = settings
            .accounts
            .get(target)
            .ok_or_else(|| format!("Backup '{backup_id}' names an unsupported provider"))?;
        // A stored profile that exactly matches the authenticated snapshot
        // (FullReset keeping the Default, logout keeping the account) needs
        // no re-insert; the reservation re-validates this before any write.
        let mismatched = accounts
            .profiles
            .iter()
            .find(|profile| profile.id == manifest.account_id)
            .is_some_and(|existing| !registered_record_matches(existing, snapshot_sha));
        if mismatched {
            conflicts.push(format!("registered account id '{}'", manifest.account_id));
        }
    }
    if !conflicts.is_empty() {
        return Err(restore_conflict_message(backup_id, &conflicts));
    }

    // --- Streaming restore (second authenticated pass) ----------------------
    let key_blob = std::fs::read(dir.join("key.dpapi"))
        .map_err(|error| format!("Unable to read backup key for '{backup_id}': {error}"))?;
    let mut key = Zeroizing::new(dpapi_unprotect(&key_blob)?);
    // Canonical re-encoding of the verified manifest: the second streaming
    // pass must describe the identical manifest (same auth binding).
    let mut verified_manifest = serde_json::to_vec(&manifest)
        .map_err(|error| format!("Unable to re-encode backup manifest: {error}"))?;
    let verified_hash = sha256_hex(&verified_manifest);
    verified_manifest.zeroize();
    drop(verified_manifest);
    // Claim the registry identity now — after full authentication and the
    // conflict scan, before the first write. Create/import collision gates
    // read this same registry under this same mutex, so from here on no
    // fresh account can adopt the restored source mid-restore.
    let reserved = if let Some(profile) = snapshot_profile.as_ref() {
        let inserted =
            reserve_restored_profile(target, profile, snapshot_sha, &restore_targets, backup_id)?;
        Some((profile.clone(), inserted))
    } else {
        None
    };
    let restore: ManagerResult<()> = (|| {
        // Restore journal: every live mutation below is recorded so any
        // failure rolls the account back to its pre-restore state. The
        // preflight guarantees none of these paths/targets existed, so
        // rollback only removes what this restore created.
        let mut journal_files: Vec<PathBuf> = Vec::new();
        let mut journal_links: Vec<PathBuf> = Vec::new();
        let mut journal_dirs: Vec<PathBuf> = Vec::new();
        let mut reader = FrameReader::open(&dir.join("data.uhb"), key.as_ref(), backup_id)?;
        let manifest_len = reader.take(8).and_then(|bytes| {
            TryInto::<[u8; 8]>::try_into(bytes)
                .map(u64::from_le_bytes)
                .map_err(|_| format!("Backup '{backup_id}' has a corrupt manifest length"))
        })?;
        if manifest_len == 0 || manifest_len > MAX_MANIFEST_BYTES as u64 {
            return Err(format!(
                "Backup '{backup_id}' has an impossible manifest length"
            ));
        }
        let mut manifest_json = reader.take(manifest_len as usize)?;
        if sha256_hex(&manifest_json) != verified_hash {
            manifest_json.zeroize();
            return Err(format!(
                "Backup '{backup_id}' stream changed between verification and restore"
            ));
        }
        let manifest2: FileManifest = serde_json::from_slice(&manifest_json)
            .map_err(|_| format!("Backup '{backup_id}' manifest is corrupt"))?;
        validate_entries_for(&manifest2.entries, Some(manifest2.kind))?;
        if manifest2.format != ARCHIVE_VERSION
            || manifest2.backup_id != backup_id
            || manifest2.account_id != manifest.account_id
            || manifest2.provider != manifest.provider
        {
            manifest_json.zeroize();
            return Err(format!(
                "Backup '{backup_id}' stream manifest does not match"
            ));
        }
        // The streaming pass parses the restore identity first: the seal
        // order (Profile entry first for Account) is enforced by validation,
        // so vault membership and the legacy root are known before any file
        // or credential is written.
        let outcome: ManagerResult<(
            Option<crate::accounts::AccountProfile>,
            Vec<super::credentials::CredentialRecord>,
        )> = (|| {
            let mut profile: Option<crate::accounts::AccountProfile> = None;
            let mut cred_records: Vec<super::credentials::CredentialRecord> = Vec::new();
            for (index, entry) in manifest2.entries.iter().enumerate() {
                // Preserved identical skill links carry no stream payload and
                // need no recreation: the live inode already is the recorded
                // one. Anything divergent was refused by the preflight.
                if preserved.contains(&index) {
                    debug_assert!(matches!(entry.kind, EntryKind::Link));
                    continue;
                }
                match entry.kind {
                    EntryKind::File => {
                        let abs = check_restore_abs(
                            entry,
                            backup_id,
                            manifest2.kind,
                            manifest2.legacy_original_path.as_ref(),
                            &shared_roots,
                        )?;
                        // A later entry for a destination an earlier entry
                        // already owns (overlapping roots recorded in two
                        // spellings) must not create the file twice. Its
                        // payload still streams through the authenticated
                        // reader so the archive stays in sync and the
                        // duplicate is proven byte-identical to the first.
                        if duplicates.contains(&index) {
                            let mut hasher = Sha256::new();
                            reader.consume_exact(entry.size, |chunk| {
                                hasher.update(chunk);
                                Ok(())
                            })?;
                            if hex_encode(&hasher.finalize()) != entry.sha256 {
                                return Err(format!(
                                    "Backup '{backup_id}' entry '{}' failed its restore hash",
                                    entry.rel
                                ));
                            }
                            continue;
                        }
                        if let Some(parent) = abs.parent() {
                            // Creates missing levels and locks only those; a
                            // pre-existing home/profile parent is never re-ACL'd,
                            // and any link on the way aborts the restore.
                            journal_dirs.extend(ensure_created_tree(parent)?);
                        }
                        // `create_new` re-checks occupancy at creation time so a
                        // race between the conflict scan and this write fails
                        // closed instead of overwriting.
                        let file = std::fs::File::create_new(&abs).map_err(|error| {
                            format!("Unable to recreate '{}': {error}", abs.display())
                        })?;
                        // Journaled before any byte lands: a truncated stream,
                        // failed hash, or failed ACL below still rolls this
                        // partial file back instead of leaving it unjournaled.
                        journal_files.push(abs.clone());
                        let mut writer = BufWriter::new(file);
                        let mut hasher = Sha256::new();
                        // Bounded streaming write: no per-chunk Vec, no whole
                        // file in memory. Writer errors propagate; truncation
                        // aborts here and rolls back the journaled partial
                        // file above. Bytes are hashed before the ACL lock
                        // below and never trusted before the hash check.
                        reader.consume_exact(entry.size, |chunk| {
                            hasher.update(chunk);
                            writer.write_all(chunk).map_err(|error| {
                                format!("Unable to recreate '{}': {error}", abs.display())
                            })?;
                            Ok(())
                        })?;
                        writer.flush().map_err(|error| {
                            format!("Unable to recreate '{}': {error}", abs.display())
                        })?;
                        writer.get_ref().sync_all().map_err(|error| {
                            format!("Unable to persist '{}': {error}", abs.display())
                        })?;
                        if hex_encode(&hasher.finalize()) != entry.sha256 {
                            let _ = std::fs::remove_file(&abs);
                            return Err(format!(
                                "Backup '{backup_id}' entry '{}' failed its restore hash",
                                entry.rel
                            ));
                        }
                        lock_to_current_user(&abs, false)?;
                    }
                    EntryKind::Profile => {
                        if manifest2.kind != ManifestKind::Account {
                            return Err(format!(
                             "Backup '{backup_id}' legacy snapshot must not carry registry metadata"
                         ));
                        }
                        // Proven-small metadata only: the kind-validated 1 MiB
                        // cap above is re-checked here so this take() can never
                        // materialize a file-sized span. File payloads always
                        // stream through consume_exact instead.
                        if entry.size > MAX_PROFILE_BYTES {
                            return Err(format!(
                                "Backup '{backup_id}' profile metadata exceeds size caps"
                            ));
                        }
                        let bytes = reader.take(entry.size as usize)?;
                        if sha256_hex(&bytes) != entry.sha256 {
                            return Err(format!(
                                "Backup '{backup_id}' profile metadata failed its hash"
                            ));
                        }
                        let parsed: crate::accounts::AccountProfile =
                            serde_json::from_slice(&bytes).map_err(|_| {
                                format!("Backup '{backup_id}' account metadata is corrupt")
                            })?;
                        if parsed.id != manifest2.account_id {
                            return Err(format!(
                                "Backup '{backup_id}' account metadata id mismatch"
                            ));
                        }
                        profile = Some(parsed);
                    }
                    EntryKind::Cred => {
                        if manifest2.kind != ManifestKind::Account {
                            return Err(format!(
                                "Backup '{backup_id}' legacy snapshot must not carry vault secrets"
                            ));
                        }
                        let secret_b64 = entry.secret_b64.as_ref().ok_or_else(|| {
                            format!(
                                "Backup '{backup_id}' credential '{}' lost its secret",
                                entry.rel
                            )
                        })?;
                        let target = entry.target.clone().ok_or_else(|| {
                            format!(
                                "Backup '{backup_id}' credential '{}' lost its target",
                                entry.rel
                            )
                        })?;
                        // Vault entries restore only for targets the authenticated
                        // profile owns: a swapped manifest cannot inject an
                        // arbitrary vault target.
                        let owned = profile
                            .as_ref()
                            .map(|profile| {
                                profile
                                    .credential_targets
                                    .iter()
                                    .any(|owned| owned == &target)
                            })
                            .unwrap_or(false);
                        if !owned {
                            return Err(format!(
                             "Refusing to restore credential '{target}': not owned by the snapshotted profile"
                         ));
                        }
                        let secret = super::credentials::base64_decode(secret_b64)?;
                        cred_records
                            .push(super::credentials::CredentialRecord::new(target, secret)?);
                    }
                    EntryKind::Dir => {
                        let abs = check_restore_abs(
                            entry,
                            backup_id,
                            manifest2.kind,
                            manifest2.legacy_original_path.as_ref(),
                            &shared_roots,
                        )?;
                        // The first entry for this destination already
                        // recreated it (possibly as a parent of another
                        // entry); one creation per physical path.
                        if duplicates.contains(&index) {
                            continue;
                        }
                        // Recreates owned empty directories and explicit roots;
                        // locks only levels this call creates, never pre-existing
                        // parents, and aborts on any link or file in the way.
                        journal_dirs.extend(ensure_created_tree(&abs)?);
                    }
                    EntryKind::Link => {
                        // Re-resolve the bound: the second stream is authenticated
                        // but independently checked, like the preflight.
                        let abs = check_restore_abs(
                            entry,
                            backup_id,
                            manifest2.kind,
                            manifest2.legacy_original_path.as_ref(),
                            &shared_roots,
                        )?;
                        let link_target = entry.link_target.as_ref().ok_or_else(|| {
                            format!("Backup '{backup_id}' link '{}' lost its target", entry.rel)
                        })?;
                        // Re-validate against the current shared library root: a
                        // library layout change must not redirect a restored link
                        // at an unrelated target.
                        let within = shared_roots.iter().any(|root| {
                            super::paths::is_within(root, link_target) || link_target == root
                        });
                        if !within {
                            return Err(format!(
                             "Refusing to recreate link '{}': target '{}' left the shared skills library",
                             abs.display(),
                             link_target.display()
                         ));
                        }
                        // The first entry for this destination already created
                        // the link; a repeat of the same authenticated link
                        // (overlapping root in another spelling) is not a
                        // second physical creation.
                        if duplicates.contains(&index) {
                            continue;
                        }
                        if let Some(parent) = abs.parent() {
                            journal_dirs.extend(ensure_created_tree(parent)?);
                        }
                        recreate_link(&abs, link_target, entry.link_is_dir).map_err(|error| {
                            format!("Unable to recreate link '{}': {error}", abs.display())
                        })?;
                        journal_links.push(abs);
                    }
                }
            }
            // The terminal marker is bound to the FIRST pass hash: a swapped
            // second stream fails here even if every entry parsed.
            let expected_marker = format!("UHBA-EOF:{verified_hash}");
            manifest_json.zeroize();
            let marker = reader.take(expected_marker.len())?;
            if marker != expected_marker.as_bytes() {
                return Err(format!(
                    "Backup '{backup_id}' lost its end marker during restore"
                ));
            }
            reader.expect_end()?;
            Ok((profile, cred_records))
        })();
        let (profile, cred_records) = match outcome {
            Ok(done) => done,
            Err(error) => {
                rollback_restore(&journal_files, &journal_links, &journal_dirs, &[]);
                manifest_json.zeroize();
                return Err(error);
            }
        };
        manifest_json.zeroize();
        if let Err(error) = super::credentials::restore_owned(&cred_records) {
            // restore_owned rolls back exactly what it wrote and refuses
            // before writing anything on a live-target conflict — so no
            // vault entry here belongs to this restore. Only files/links
            // created above are rolled back; live entries are never touched.
            rollback_restore(&journal_files, &journal_links, &journal_dirs, &[]);
            return Err(error);
        }
        if manifest2.kind == ManifestKind::Account {
            let profile =
                profile.ok_or_else(|| format!("Backup '{backup_id}' lost its account metadata"))?;
            // The reservation committed before the first write. Re-validate
            // it (do not re-insert): a record that vanished or changed under
            // the write must fail closed instead of replacing another
            // account's metadata.
            let current = super::state::load_fresh();
            let stored = current.accounts.get(target).and_then(|accounts| {
                accounts
                    .profiles
                    .iter()
                    .find(|existing| existing.id == profile.id)
            });
            let registry_outcome = match stored {
                Some(existing) if registered_record_matches(existing, snapshot_sha) => Ok(()),
                Some(_) => Err(format!(
                    "Refusing to restore backup '{backup_id}': registered account id '{}' no longer matches",
                    profile.id
                )),
                None => Err(format!(
                    "Refusing to restore backup '{backup_id}': account id '{}' lost its reservation during the restore",
                    profile.id
                )),
            };
            if let Err(error) = registry_outcome {
                let written: Vec<String> = manifest2
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == EntryKind::Cred)
                    .filter_map(|entry| entry.target.clone())
                    .collect();
                rollback_restore(&journal_files, &journal_links, &journal_dirs, &written);
                return Err(error);
            }
        }
        Ok(())
    })();
    if restore.is_err() {
        // Undo only this restore's own reservation; a matching record that
        // already existed (FullReset Default, logout) is never removed.
        if let Some((profile, true)) = &reserved {
            release_restored_profile(target, profile);
        }
    }
    key.zeroize();
    restore
}

#[cfg(windows)]
fn recreate_link(link: &Path, target: &Path, is_dir: bool) -> ManagerResult<()> {
    if is_dir {
        if std::os::windows::fs::symlink_dir(target, link).is_ok() {
            return Ok(());
        }
        // Directory symlinks need Developer Mode or privilege; junctions do
        // not. Fall back to the shared native mount-point helper (no shell,
        // so metacharacters and either slash style stay literal), which
        // cleanup removes identically.
        super::skills::create_junction(target, link)
            .map_err(|error| format!("link creation failed: {error}"))
    } else {
        std::os::windows::fs::symlink_file(target, link)
            .map_err(|error| format!("link creation failed: {error}"))
    }
}

#[cfg(not(windows))]
fn recreate_link(link: &Path, target: &Path, is_dir: bool) -> ManagerResult<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .map_err(|error| format!("link creation failed: {error}"))?;
        let _ = is_dir;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (link, target, is_dir);
        Err("Link recreation is only supported on Windows and Unix".into())
    }
}

/// Delete every backup past its 7-day undo window. Removes the whole backup
/// directory (ciphertext, wrapped key, metadata tombstone). Returns the
/// purged infos, oldest first. A single undeletable directory (AV lock,
/// open handle) is logged and skipped so it cannot block retention of all
/// later backups; the next poller pass retries it. Live restores are never
/// triggered.
pub fn purge_expired(now_unix: u64) -> ManagerResult<Vec<BackupInfo>> {
    let mut purged = Vec::new();
    // Group by account and probe each account guard with timeout 0: an
    // account busy with cleanup holds its guard, and the poller timer must
    // skip it and retry later rather than block or delete under it.
    let mut by_account: BTreeMap<(ProviderId, String), Vec<BackupInfo>> = BTreeMap::new();
    for info in list_all_backups()? {
        if info.is_expired(now_unix) {
            by_account
                .entry((info.provider, info.account_id.clone()))
                .or_default()
                .push(info);
        }
    }
    for ((provider, account_id), infos) in by_account {
        let Ok(_guard) = super::state::AccountOperationGuard::acquire(provider, &account_id, 0)
        else {
            crate::diagnose::log(format!("Skipping purge for busy account '{account_id}'"));
            continue;
        };
        for info in infos {
            match guarded_remove_backup_dir(&info.path, &info.backup_id) {
                Ok(()) => purged.push(info),
                Err(error) => crate::diagnose::log(error),
            }
        }
    }
    purge_orphan_backup_dirs(now_unix);
    Ok(purged)
}

/// Remove orphaned backup leaf dirs that can never verify: interrupted
/// seals, corrupt tombstones, or stale copies. Only directories that (a)
/// sit exactly at `<backups>/<provider>/<account>/<backup_id>`, (b) carry
/// a valid backup-id name, (c) are not links, and (d) are older than the
/// retention window are removed — never arbitrary user data, never fresh
/// (possibly in-flight) dirs. Failures are logged and skipped.
fn purge_orphan_backup_dirs(now_unix: u64) {
    let Some(root) = super::paths::fork_backups_dir() else {
        return;
    };
    let Ok(providers) = std::fs::read_dir(&root) else {
        return;
    };
    for provider_dir in providers.filter_map(|child| child.ok().map(|child| child.path())) {
        if super::paths::is_reparse_point(&provider_dir) {
            continue;
        }
        let provider_key = provider_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(provider) = ProviderId::from_key(&provider_key) else {
            continue;
        };
        let Ok(accounts) = std::fs::read_dir(&provider_dir) else {
            continue;
        };
        for account_dir in accounts.filter_map(|child| child.ok().map(|child| child.path())) {
            if !account_dir.is_dir() || super::paths::is_reparse_point(&account_dir) {
                continue;
            }
            let account_id = account_dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Try-only like the expired sweep: a busy account is retried
            // next poll, never deleted under.
            let Ok(_guard) = super::state::AccountOperationGuard::acquire(provider, &account_id, 0)
            else {
                continue;
            };
            let Ok(backups) = std::fs::read_dir(&account_dir) else {
                continue;
            };
            for backup_dir in backups.filter_map(|child| child.ok().map(|child| child.path())) {
                if !backup_dir.is_dir() || super::paths::is_reparse_point(&backup_dir) {
                    continue;
                }
                let name = backup_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Valid backups are listed above; anything failing the
                // enumerated identity binding (incomplete, corrupt, or
                // mismatched tombstone) is an orphan once it is old.
                if validate_backup_id(&name).is_err() {
                    continue;
                }
                if bound_backup_info(&backup_dir, provider, &account_id).is_some() {
                    continue;
                }
                if !dir_older_than_retention(&backup_dir, now_unix) {
                    continue;
                }
                if let Err(error) = guarded_remove_backup_dir(&backup_dir, &name) {
                    crate::diagnose::log(error);
                } else {
                    crate::diagnose::log(format!("Purged orphaned backup '{name}'"));
                }
            }
        }
    }
}

/// True when `dir` was last modified at least a full retention window
/// before `now_unix`. Unknown mtime fails closed (never purged).
fn dir_older_than_retention(dir: &Path, now_unix: u64) -> bool {
    let Ok(metadata) = std::fs::metadata(dir) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let modified_unix = since_epoch.as_secs();
    let Some(age_secs) = now_unix.checked_sub(modified_unix) else {
        return false;
    };
    age_secs >= super::BACKUP_RETENTION_SECS
}

/// Remove one enumerated backup directory after re-validating the bound:
/// it must sit inside the fork backups root and must not itself be a link.
/// `locate_backup` already returns enumerated identity-bound dirs; this is
/// the second net before any recursive delete.
fn guarded_remove_backup_dir(dir: &Path, backup_id: &str) -> ManagerResult<()> {
    let Some(root) = super::paths::fork_backups_dir() else {
        return Err(format!(
            "Unable to purge backup '{backup_id}': backups root unavailable"
        ));
    };
    if !super::paths::is_within(&root, dir) || super::paths::is_reparse_point(dir) {
        return Err(format!(
            "Refusing to purge backup '{backup_id}': '{}' escapes the backups root",
            dir.display()
        ));
    }
    std::fs::remove_dir_all(dir)
        .map_err(|error| format!("Unable to purge backup '{backup_id}': {error}"))?;
    Ok(())
}

/// Delete one backup unconditionally (ciphertext + wrapped key + metadata
/// tombstone). Never restores anything.
pub fn purge_backup(backup_id: &str) -> ManagerResult<()> {
    let (dir, meta) = locate_backup(backup_id)?;
    let _guard = super::state::AccountOperationGuard::acquire(
        meta.info.provider,
        &meta.info.account_id,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    guarded_remove_backup_dir(&dir, backup_id)
}

/// Encrypt a plaintext legacy backup directory (e.g. a discovered
/// `~/.claude_backup_*`) into a managed snapshot.
///
/// The caller supplies the provider and account id (migration derives
/// `legacy_<sanitized-dirname>`, charset `[A-Za-z0-9_]`); the id is
/// sanitized again here so hyphenated caller input still lands safely, and
/// the effective id is reported back in [`BackupInfo::account_id`]. Restore
/// writes a legacy snapshot back to its recorded original legacy path —
//  never into the active account — and touches neither the registry nor the
/// vault. This is the explicit import API the migration owner calls per
/// discovered legacy dir (encrypt / purge / leave choice stays with the
/// user; nothing is auto-discovered or auto-deleted here).
pub fn import_legacy_backup(
    path: &Path,
    provider: ProviderId,
    account_id: &str,
) -> ManagerResult<BackupInfo> {
    provider_key(provider)?;
    if !path.is_absolute() {
        return Err(format!(
            "Refusing to import relative legacy path '{}'",
            path.display()
        ));
    }
    let mut effective = sanitize_token(account_id, 48);
    if !effective.starts_with(LEGACY_ACCOUNT_PREFIX) {
        effective = format!("{LEGACY_ACCOUNT_PREFIX}{effective}");
    }
    let _guard = super::state::AccountOperationGuard::acquire(
        provider,
        &effective,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    let created = now_unix();
    let backup_id = generate_backup_id(created)?;
    // Record the canonical root: restore bounds every entry against this
    // spelling while walked entries are recorded canonical.
    let canonical_root = canonical_requested_root(path)?;
    let collector = collect_legacy_snapshot(&canonical_root)?;
    let (warnings, entries, payloads) = collector_to_seal_parts(collector);
    seal_archive(
        provider,
        &effective,
        &backup_id,
        created,
        ManifestKind::Legacy,
        Some(canonical_root),
        warnings,
        entries,
        payloads,
    )
}
/// Encrypt exactly the caller-selected legacy paths (e.g. the cutover
/// bridge export `profiles/Second/.credentials.json`) into a managed
/// legacy snapshot — not the whole old AppData tree.
///
/// Every target must be absolute and resolve inside the recorded legacy
/// `root` (validated here, then re-enforced at restore by
/// [`check_restore_abs`]); anything outside is refused before any hashing.
/// The archive is [`ManifestKind::Legacy`]: no registry metadata, no vault
/// entries, restore confined to the recorded root. Capture reuses
/// [`collect_paths_snapshot`] (link-safe, no traversal, no plaintext
/// staging). The existing whole-folder [`import_legacy_backup`] is
/// unchanged.
pub fn import_legacy_paths(
    root: &Path,
    targets: &[PathBuf],
    provider: ProviderId,
    account_id: &str,
) -> ManagerResult<BackupInfo> {
    provider_key(provider)?;
    if !root.is_absolute() {
        return Err(format!(
            "Refusing to import relative legacy root '{}'",
            root.display()
        ));
    }
    let canonical_root = canonical_requested_root(root)?;
    if targets.is_empty() {
        return Err("Refusing to import an empty legacy target list".into());
    }
    let mut seen_roots = BTreeSet::new();
    for target in targets {
        let canonical = canonical_requested_root(target).map_err(|error| {
            format!(
                "Legacy target '{}' is unavailable: {error}",
                target.display()
            )
        })?;
        if !seen_roots.insert(canonical.clone()) {
            return Err(format!(
                "Refusing to import duplicate legacy target '{}'",
                target.display()
            ));
        }
        if !super::paths::is_within(&canonical_root, &canonical) {
            return Err(format!(
                "Refusing to import legacy target '{}': outside the recorded root '{}'",
                target.display(),
                canonical_root.display()
            ));
        }
    }
    let mut effective = sanitize_token(account_id, 48);
    if !effective.starts_with(LEGACY_ACCOUNT_PREFIX) {
        effective = format!("{LEGACY_ACCOUNT_PREFIX}{effective}");
    }
    let _guard = super::state::AccountOperationGuard::acquire(
        provider,
        &effective,
        BACKUP_GUARD_TIMEOUT_MS,
    )?;
    let created = now_unix();
    let backup_id = generate_backup_id(created)?;
    let collector = collect_paths_snapshot(targets)?;
    let (warnings, entries, payloads) = collector_to_seal_parts(collector);
    seal_archive(
        provider,
        &effective,
        &backup_id,
        created,
        ManifestKind::Legacy,
        Some(canonical_root),
        warnings,
        entries,
        payloads,
    )
}
// ---------------------------------------------------------------------------
// Deterministic regression tests (fixture roots, no live secrets/vault).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "usagehub-backup-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ))
    }

    fn test_context(provider: ProviderId, id: &str) -> AccountContext {
        test_context_owned(
            provider,
            id,
            crate::accounts::AccountOwnership::Managed,
            Vec::new(),
        )
    }

    fn test_context_owned(
        provider: ProviderId,
        id: &str,
        ownership: crate::accounts::AccountOwnership,
        credential_targets: Vec<String>,
    ) -> AccountContext {
        AccountContext {
            provider,
            profile: crate::accounts::AccountProfile {
                id: id.to_string(),
                name: format!("Test {id}"),
                credential_targets,
                ownership,
                ..Default::default()
            },
            source: crate::accounts::SourceKind::ProviderDefault,
            config_dir: None,
            desktop_dir: None,
            credential_file: None,
            ownership,
        }
    }

    fn register_test_profile(ctx: &AccountContext) {
        super::super::state::mutate_settings(|settings| {
            let accounts = match ctx.provider {
                ProviderId::Claude => &mut settings.accounts.claude,
                ProviderId::Codex => &mut settings.accounts.codex,
                _ => return Err("test provider unsupported".into()),
            };
            accounts
                .profiles
                .retain(|profile| profile.id != ctx.profile.id);
            accounts.used_ids.insert(ctx.profile.id.clone());
            accounts.profiles.push(ctx.profile.clone());
            Ok(())
        })
        .unwrap();
    }

    fn unregister_test_profile(provider: ProviderId, id: &str) {
        super::super::state::mutate_settings(|settings| {
            let accounts = match provider {
                ProviderId::Claude => &mut settings.accounts.claude,
                ProviderId::Codex => &mut settings.accounts.codex,
                _ => return Err("test provider unsupported".into()),
            };
            accounts.profiles.retain(|profile| profile.id != id);
            Ok(())
        })
        .unwrap();
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
    }

    /// Create a directory junction into the shared library with `mklink /J`
    /// (the lifecycle spelling). Returns false where the platform refuses it,
    /// so link-specific assertions can skip vacuously.
    fn try_skill_junction(target: &Path, link: &Path) -> bool {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "mklink", "/J"]).arg(link).arg(target);
        crate::manager::process::run_bounded(
            &mut command,
            std::time::Duration::from_secs(10),
            64 * 1024,
            "test.mklink",
        )
        .map(|output| output.status.success())
        .unwrap_or(false)
            && crate::manager::paths::is_reparse_point(link)
    }

    #[test]
    fn targeted_snapshot_roundtrip_restores_exact_bytes() {
        let source = unique_dir("roundtrip");
        write_tree(
            &source,
            &[
                ("cli/.credentials.json", "{\"token\":\"abc\"}"),
                ("cli/history.jsonl", "{}\n"),
                ("nested/deep/notes.txt", "hello"),
            ],
        );
        // Overlapping roots: the skill link below is reached through the
        // walked `cli` tree *and* as its own explicit cleanup target, which
        // used to record the same inode twice in two spellings (canonical
        // walked child vs raw explicit root) and fail the restore with
        // os 183 AlreadyExists on the second creation.
        let shared = crate::manager::skills::ensure_shared_skills_dir().unwrap();
        std::fs::write(shared.join("roundtrip-link.txt"), "linked").unwrap();
        let link = source.join("cli/skills");
        let junction = try_skill_junction(&shared, &link);
        let ctx = test_context(ProviderId::Claude, "roundtrip_a1");
        register_test_profile(&ctx);
        let mut targets = vec![source.join("cli"), source.join("nested")];
        if junction {
            targets.push(link.clone());
        }
        let info =
            create_backup_for_paths_with_id(&ctx, &targets, "bk_roundtrip_01", 1_700_000_000)
                .unwrap();
        assert_eq!(info.backup_id, "bk_roundtrip_01");
        assert_eq!(
            info.expires_unix,
            1_700_000_000 + super::super::BACKUP_RETENTION_SECS
        );
        assert!(info.path.join("data.uhb").is_file());
        assert!(info.path.join("key.dpapi").is_file());
        assert!(info.path.join("meta.json").is_file());

        // Reread/decrypt verification passes before any deletion.
        let verified = verify_backup("bk_roundtrip_01").unwrap();
        assert_eq!(verified.manifest_hash, info.manifest_hash);

        // Delete live data, restore, compare bytes. The junction is
        // unlinked by inode (never traversed) so restore recreates it.
        if junction {
            crate::manager::skills::remove_link_only(&link).unwrap();
        }
        std::fs::remove_dir_all(&source).unwrap();
        assert!(!source.exists());
        restore_backup("bk_roundtrip_01").unwrap();
        assert_eq!(
            std::fs::read_to_string(source.join("cli/.credentials.json")).unwrap(),
            "{\"token\":\"abc\"}"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("nested/deep/notes.txt")).unwrap(),
            "hello"
        );
        if junction {
            // Exactly one physical link exists at the overlapping path.
            assert!(crate::manager::paths::is_reparse_point(&link));
            assert_eq!(
                std::fs::read_to_string(link.join("roundtrip-link.txt")).unwrap(),
                "linked"
            );
        }
        // Registry re-insert retained the stable id.
        let settings = super::super::state::load_fresh();
        assert!(settings
            .accounts
            .claude
            .profiles
            .iter()
            .any(|profile| profile.id == "roundtrip_a1"));
        assert!(settings.accounts.claude.used_ids.contains("roundtrip_a1"));
        purge_backup("bk_roundtrip_01").unwrap();
        assert!(!info.path.exists());
        // Leave the fixture registry tidy for sibling tests on this thread.
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "roundtrip_a1");
            Ok(())
        })
        .unwrap();
        if junction {
            crate::manager::skills::remove_link_only(&link).ok();
        }
        std::fs::remove_dir_all(&source).ok();
        std::fs::remove_file(shared.join("roundtrip-link.txt")).ok();
    }

    #[test]
    fn tampered_frame_fails_verification() {
        let source = unique_dir("tamper");
        write_tree(&source, &[("data.txt", "0123456789abcdef")]);
        let ctx = test_context(ProviderId::Claude, "tamper_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_tamper_01", 1_700_000_001)
            .unwrap();
        let data_path = super::super::paths::backup_dir_for(ProviderId::Claude, "tamper_a1")
            .unwrap()
            .join("bk_tamper_01/data.uhb");
        let mut bytes = std::fs::read(&data_path).unwrap();
        let flip = bytes.len().min(200).saturating_sub(10);
        bytes[flip] ^= 0xff;
        std::fs::write(&data_path, &bytes).unwrap();
        let error = verify_backup("bk_tamper_01").unwrap_err();
        assert!(
            error.contains("authentication") || error.contains("hash") || error.contains("marker"),
            "{error}"
        );
        purge_backup("bk_tamper_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "tamper_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn truncated_archive_fails_verification() {
        let source = unique_dir("trunc");
        write_tree(&source, &[("data.txt", &"x".repeat(5000))]);
        let ctx = test_context(ProviderId::Claude, "trunc_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_trunc_01", 1_700_000_002)
            .unwrap();
        let data_path = super::super::paths::backup_dir_for(ProviderId::Claude, "trunc_a1")
            .unwrap()
            .join("bk_trunc_01/data.uhb");
        let bytes = std::fs::read(&data_path).unwrap();
        std::fs::write(&data_path, &bytes[..bytes.len() / 2]).unwrap();
        let error = verify_backup("bk_trunc_01").unwrap_err();
        assert!(error.contains("truncat"), "{error}");
        purge_backup("bk_trunc_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "trunc_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn restore_refuses_occupied_live_data_without_touching_it() {
        let source = unique_dir("conflict");
        write_tree(
            &source,
            &[
                ("keep.txt", "original-live"),
                (".credentials.json", "{\"token\":\"snapshotted\"}"),
            ],
        );
        let ctx = test_context(ProviderId::Claude, "conflict_a1");
        register_test_profile(&ctx);
        // Snapshot the parent listing so `keep.txt` is covered, then delete
        // everything and recreate a *different* live file at the same path.
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_conflict_01", 1_700_000_003)
            .unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("keep.txt"), "do-not-overwrite").unwrap();
        // Recreate the sibling prefix dir the archive recorded.
        let error = restore_backup("bk_conflict_01").unwrap_err();
        assert!(error.contains("occupied"), "{error}");
        assert_eq!(
            std::fs::read_to_string(source.join("keep.txt")).unwrap(),
            "do-not-overwrite"
        );
        // Same deleted root, now re-adopted (while empty) by a DIFFERENT
        // registered profile: the root is that profile's source, so the old
        // snapshot must refuse before restoring any auth byte, and the
        // adopter's record must stay exactly as registered.
        std::fs::remove_file(source.join("keep.txt")).unwrap();
        let mut adopted = test_context(ProviderId::Claude, "conflict_b2");
        adopted.source = crate::accounts::SourceKind::CredentialFile;
        adopted.config_dir = Some(source.clone());
        adopted.profile.config_dir = source.to_string_lossy().into_owned();
        register_test_profile(&adopted);
        let error = restore_backup("bk_conflict_01").unwrap_err();
        assert!(
            error.contains("another registered account") && error.contains("conflict_b2"),
            "{error}"
        );
        assert!(
            !source.join(".credentials.json").exists(),
            "restore must not write auth into the adopted root"
        );
        assert!(
            std::fs::read_dir(&source).unwrap().next().is_none(),
            "restore must not create anything in the adopted root"
        );
        let settings = super::super::state::load_fresh();
        assert_eq!(
            settings
                .accounts
                .claude
                .profiles
                .iter()
                .filter(|profile| profile.id == "conflict_b2")
                .count(),
            1,
            "adopter record must neither vanish nor duplicate"
        );
        assert_eq!(
            settings.accounts.claude.find("conflict_b2"),
            Some(&adopted.profile),
            "restore must not alter the adopter's record"
        );
        unregister_test_profile(ProviderId::Claude, "conflict_b2");
        purge_backup("bk_conflict_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "conflict_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn traversal_entries_rejected_before_any_disk_write() {
        let entry = ManifestEntry {
            rel: "../evil.txt".to_string(),
            kind: EntryKind::File,
            abs: Some(PathBuf::from("C:/evil.txt")),
            target: None,
            link_target: None,
            link_is_dir: false,
            size: 1,
            sha256: sha256_hex(b"x"),
            secret_b64: None,
        };
        let root = super::super::paths::fork_backups_dir().unwrap();
        let mut before: Vec<PathBuf> = std::fs::read_dir(&root)
            .map(|listing| {
                listing
                    .filter_map(|child| child.ok().map(|child| child.path()))
                    .collect()
            })
            .unwrap_or_default();
        before.sort();
        let error = seal_archive(
            ProviderId::Claude,
            "traversal_a1",
            "bk_traversal_01",
            1_700_000_004,
            ManifestKind::Account,
            None,
            Vec::new(),
            vec![entry],
            vec![EntryPayload::Inline(b"x".to_vec())],
        )
        .unwrap_err();
        assert!(error.contains("Unsafe archive entry"), "{error}");
        let mut after: Vec<PathBuf> = std::fs::read_dir(&root)
            .map(|listing| {
                listing
                    .filter_map(|child| child.ok().map(|child| child.path()))
                    .collect()
            })
            .unwrap_or_default();
        after.sort();
        assert_eq!(before, after);
    }

    #[test]
    fn oversized_entry_rejected_by_cap() {
        let entry = ManifestEntry {
            rel: "big.bin".to_string(),
            kind: EntryKind::File,
            abs: Some(PathBuf::from("C:/big.bin")),
            target: None,
            link_target: None,
            link_is_dir: false,
            size: MAX_TOTAL_BYTES + 1,
            sha256: "0".repeat(64),
            secret_b64: None,
        };
        let error = seal_archive(
            ProviderId::Claude,
            "cap_a1",
            "bk_cap_01",
            1_700_000_005,
            ManifestKind::Account,
            None,
            Vec::new(),
            vec![entry],
            vec![EntryPayload::Inline(vec![0u8; 8])],
        )
        .unwrap_err();
        assert!(
            error.contains("caps") || error.contains("exceeds"),
            "{error}"
        );
    }

    #[test]
    fn expiry_purges_only_expired_backups() {
        let source = unique_dir("expiry");
        write_tree(&source, &[("a.txt", "a")]);
        let ctx = test_context(ProviderId::Claude, "expiry_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_expiry_old", 1000).unwrap();
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_expiry_new", 1_800_000_000)
            .unwrap();
        let purged = purge_expired(1000 + super::super::BACKUP_RETENTION_SECS + 1).unwrap();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].backup_id, "bk_expiry_old");
        assert!(verify_backup("bk_expiry_new").is_ok());
        assert!(verify_backup("bk_expiry_old").is_err());
        purge_backup("bk_expiry_new").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "expiry_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn retention_boundary_uses_modified_timestamp() {
        let dir = unique_dir("retention-boundary");
        std::fs::create_dir_all(&dir).unwrap();
        let modified_unix = std::fs::metadata(&dir)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let window = super::super::BACKUP_RETENTION_SECS;
        // Exact 7-day boundary in seconds expires.
        assert!(dir_older_than_retention(&dir, modified_unix + window));
        // One second shy of the window stays.
        assert!(!dir_older_than_retention(&dir, modified_unix + window - 1));
        // Past the window expires.
        assert!(dir_older_than_retention(&dir, modified_unix + window + 1));
        // `now` at or before the mtime (future/unknown clock) never purges.
        assert!(!dir_older_than_retention(
            &dir,
            modified_unix.saturating_sub(1)
        ));
        assert!(!dir_older_than_retention(&dir, 0));
        // Missing metadata fails closed.
        assert!(!dir_older_than_retention(
            &dir.join("missing"),
            modified_unix + window + 3600
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(windows)]
    fn quiescence_refuses_held_lock_but_not_stale_leftover() {
        use std::os::windows::ffi::OsStrExt;
        let desktop = unique_dir("desktop-live");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(desktop.join("Local State"), "{}").unwrap();
        // Stale zero-byte leftover from a previous run: opens fine, must
        // not block the snapshot.
        std::fs::write(desktop.join("SingletonLock"), "").unwrap();
        let mut ctx = test_context(ProviderId::Claude, "live_a1");
        ctx.source = crate::accounts::SourceKind::DesktopDir;
        ctx.desktop_dir = Some(desktop.clone());
        register_test_profile(&ctx);
        // Hold the lock file open with no sharing, like a running app.
        let wide: Vec<u16> = desktop
            .join("SingletonLock")
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0x8000_0000 | 0x4000_0000,
                0,
                std::ptr::null(),
                4,
                0,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        use std::os::windows::io::FromRawHandle;
        let _held = unsafe { std::fs::File::from_raw_handle(handle) };
        // A second exclusive probe must now fail: live lock held.
        assert!(lock_held_by_live_process(&desktop.join("SingletonLock")).unwrap());
        let error = create_backup_with_id(&ctx, "bk_live_01", 1_700_000_006).unwrap_err();
        assert!(error.contains("Stop it first"), "{error}");
        drop(_held);
        // Released: the stale leftover opens fine and the snapshot proceeds.
        assert!(!lock_held_by_live_process(&desktop.join("SingletonLock")).unwrap());
        create_backup_with_id(&ctx, "bk_live_02", 1_700_000_007).unwrap();
        verify_backup("bk_live_02").unwrap();
        purge_backup("bk_live_02").unwrap();
        unregister_test_profile(ProviderId::Claude, "live_a1");
        std::fs::remove_dir_all(&desktop).ok();
    }

    #[test]
    fn legacy_import_restores_to_original_path_not_registry() {
        let legacy = unique_dir("claude_backup_demo");
        write_tree(
            &legacy,
            &[("old.json", "{\"v\":1}"), ("sub/keep.txt", "keep")],
        );
        let info = import_legacy_backup(&legacy, ProviderId::Claude, "legacy-demo").unwrap();
        assert!(info.account_id.starts_with(LEGACY_ACCOUNT_PREFIX));
        assert!(!info.account_id.contains('-'));
        verify_backup(&info.backup_id).unwrap();
        std::fs::remove_dir_all(&legacy).unwrap();
        restore_backup(&info.backup_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(legacy.join("old.json")).unwrap(),
            "{\"v\":1}"
        );
        // No registry footprint for legacy snapshots.
        let settings = super::super::state::load_fresh();
        assert!(
            settings
                .accounts
                .claude
                .profiles
                .iter()
                .all(|profile| profile.id != info.account_id),
            "legacy restore must not register an account"
        );
        purge_backup(&info.backup_id).unwrap();
        std::fs::remove_dir_all(&legacy).ok();
    }

    /// Parent safety gate: a large multi-chunk file plus two same-length
    /// files with different content must round-trip byte-exact.
    #[test]
    fn multichunk_and_same_length_files_roundtrip() {
        let source = unique_dir("multichunk");
        std::fs::create_dir_all(&source).unwrap();
        // ~200 KiB deterministic pattern: spans several 64 KiB frames.
        let big: Vec<u8> = (0..204_800u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        std::fs::write(source.join("big.bin"), &big).unwrap();
        std::fs::write(source.join("same_a.bin"), &[0x41u8; 4096]).unwrap();
        std::fs::write(source.join("same_b.bin"), &[0x42u8; 4096]).unwrap();
        let ctx = test_context(ProviderId::Claude, "multi_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_multi_01", 1_700_000_010)
            .unwrap();
        verify_backup("bk_multi_01").unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        restore_backup("bk_multi_01").unwrap();
        assert_eq!(std::fs::read(source.join("big.bin")).unwrap(), big);
        assert_eq!(
            std::fs::read(source.join("same_a.bin")).unwrap(),
            vec![0x41u8; 4096]
        );
        assert_eq!(
            std::fs::read(source.join("same_b.bin")).unwrap(),
            vec![0x42u8; 4096]
        );
        purge_backup("bk_multi_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "multi_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    fn read_frames(data_path: &Path) -> (Vec<u8>, Vec<Vec<u8>>) {
        let bytes = std::fs::read(data_path).unwrap();
        let header = bytes[..22].to_vec();
        let mut frames = Vec::new();
        let mut cursor = 22usize;
        while cursor < bytes.len() {
            let length = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            frames.push(bytes[cursor..cursor + 4 + length].to_vec());
            cursor += 4 + length;
        }
        (header, frames)
    }

    fn rewrite_frames(data_path: &Path, header: &[u8], frames: &[Vec<u8>]) {
        let mut bytes = header.to_vec();
        for frame in frames {
            bytes.extend_from_slice(frame);
        }
        std::fs::write(data_path, &bytes).unwrap();
    }

    fn backup_data_path(account_id: &str, backup_id: &str) -> PathBuf {
        super::super::paths::backup_dir_for(ProviderId::Claude, account_id)
            .unwrap()
            .join(format!("{backup_id}/data.uhb"))
    }

    /// Reordered frames carry the wrong per-frame AAD/nonce and must fail.
    #[test]
    fn reordered_frames_fail_verification() {
        let source = unique_dir("reorder");
        write_tree(
            &source,
            &[
                ("one.txt", &"a".repeat(70_000)),
                ("two.txt", &"b".repeat(70_000)),
            ],
        );
        let ctx = test_context(ProviderId::Claude, "reorder_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_reorder_01", 1_700_000_011)
            .unwrap();
        let data_path = backup_data_path("reorder_a1", "bk_reorder_01");
        let (header, mut frames) = read_frames(&data_path);
        assert!(frames.len() > 2, "need several frames to reorder");
        frames.swap(0, 1);
        rewrite_frames(&data_path, &header, &frames);
        let error = verify_backup("bk_reorder_01").unwrap_err();
        assert!(
            error.contains("authentication")
                || error.contains("hash")
                || error.contains("manifest"),
            "{error}"
        );
        purge_backup("bk_reorder_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "reorder_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    /// A dropped terminal frame removes the EOF marker and must fail.
    #[test]
    fn missing_terminal_frame_fails_verification() {
        let source = unique_dir("noterm");
        write_tree(&source, &[("data.txt", &"z".repeat(90_000))]);
        let ctx = test_context(ProviderId::Claude, "noterm_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_noterm_01", 1_700_000_012)
            .unwrap();
        let data_path = backup_data_path("noterm_a1", "bk_noterm_01");
        let (header, mut frames) = read_frames(&data_path);
        assert!(!frames.is_empty());
        frames.pop();
        rewrite_frames(&data_path, &header, &frames);
        let error = verify_backup("bk_noterm_01").unwrap_err();
        assert!(
            error.contains("truncat") || error.contains("marker") || error.contains("hash"),
            "{error}"
        );
        purge_backup("bk_noterm_01").unwrap();
        super::super::state::mutate_settings(|settings| {
            settings
                .accounts
                .claude
                .profiles
                .retain(|profile| profile.id != "noterm_a1");
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn empty_directories_roundtrip_with_dir_markers() {
        let source = unique_dir("emptydirs");
        std::fs::create_dir_all(source.join("empty_a")).unwrap();
        std::fs::create_dir_all(source.join("full/sub")).unwrap();
        std::fs::write(source.join("full/sub/file.txt"), "x").unwrap();
        std::fs::create_dir_all(source.join("empty_b/nested")).unwrap();
        let ctx = test_context(ProviderId::Claude, "emptydir_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_empty_01", 1_700_000_020)
            .unwrap();
        let attestation = coverage_attestation("bk_empty_01").unwrap();
        let canonical_source = canonical_requested_root(&source).unwrap();
        assert!(
            attestation.roots.contains(&canonical_source),
            "{attestation:?}"
        );
        assert!(
            attestation
                .roots
                .contains(&canonical_source.join("empty_a")),
            "{attestation:?}"
        );
        std::fs::remove_dir_all(&source).unwrap();
        restore_backup("bk_empty_01").unwrap();
        assert!(source.join("empty_a").is_dir());
        assert!(source.join("empty_b/nested").is_dir());
        assert_eq!(
            std::fs::read_to_string(source.join("full/sub/file.txt")).unwrap(),
            "x"
        );
        purge_backup("bk_empty_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "emptydir_a1");
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn metadata_only_snapshot_bound_to_registered_managed() {
        // Unregistered account: refused, never an unbound archive.
        let ghost = test_context(ProviderId::Claude, "ghost_a1");
        let error =
            create_backup_for_paths_with_id(&ghost, &[], "bk_ghost_01", 1_700_000_021).unwrap_err();
        assert!(
            error.contains("unbound") || error.contains("register"),
            "{error}"
        );
        // Registered Managed: credential/metadata-only snapshot allowed.
        let ctx = test_context(ProviderId::Claude, "vaultonly_a1");
        register_test_profile(&ctx);
        let info =
            create_backup_for_paths_with_id(&ctx, &[], "bk_vaultonly_01", 1_700_000_022).unwrap();
        verify_backup("bk_vaultonly_01").unwrap();
        let attestation = coverage_attestation("bk_vaultonly_01").unwrap();
        assert!(attestation.roots.is_empty(), "{attestation:?}");
        purge_backup("bk_vaultonly_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "vaultonly_a1");
        let _ = info;
    }

    #[test]
    fn external_needs_declared_vault_for_metadata_only() {
        let bare = test_context_owned(
            ProviderId::Claude,
            "extbare_a1",
            crate::accounts::AccountOwnership::External,
            Vec::new(),
        );
        register_test_profile(&bare);
        let error = create_backup_for_paths_with_id(&bare, &[], "bk_extbare_01", 1_700_000_023)
            .unwrap_err();
        assert!(error.contains("unbound"), "{error}");
        unregister_test_profile(ProviderId::Claude, "extbare_a1");
        let vaulted = test_context_owned(
            ProviderId::Claude,
            "extvault_a1",
            crate::accounts::AccountOwnership::External,
            vec!["UsageHub/test-extvault".to_string()],
        );
        register_test_profile(&vaulted);
        create_backup_for_paths_with_id(&vaulted, &[], "bk_extvault_01", 1_700_000_024).unwrap();
        verify_backup("bk_extvault_01").unwrap();
        purge_backup("bk_extvault_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "extvault_a1");
    }

    #[test]
    fn logout_style_restore_skips_matching_registry_record() {
        // Registry kept (logout scope), auth files deleted: restore must
        // write the files back without touching the matching record.
        let source = unique_dir("logoutrestore");
        write_tree(&source, &[("auth.json", "{\"token\":\"t\"}")]);
        let ctx = test_context(ProviderId::Claude, "logout_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_logout_01", 1_700_000_025)
            .unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        restore_backup("bk_logout_01").unwrap();
        assert_eq!(
            std::fs::read_to_string(source.join("auth.json")).unwrap(),
            "{\"token\":\"t\"}"
        );
        let settings = super::super::state::load_fresh();
        let matches = settings
            .accounts
            .claude
            .profiles
            .iter()
            .filter(|profile| profile.id == "logout_a1")
            .count();
        assert_eq!(matches, 1);
        purge_backup("bk_logout_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "logout_a1");
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn listed_paths_come_from_enumeration_not_plaintext_meta() {
        let source = unique_dir("metapath");
        write_tree(&source, &[("f.txt", "v")]);
        let ctx = test_context(ProviderId::Claude, "metapath_a1");
        register_test_profile(&ctx);
        let info = create_backup_for_paths_with_id(
            &ctx,
            &[source.clone()],
            "bk_metapath_01",
            1_700_000_026,
        )
        .unwrap();
        // Tamper the plaintext tombstone path: listings must still report
        // the enumerated directory, so purge/restore cannot be redirected.
        let meta_path = info.path.join("meta.json");
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
        meta["info"]["path"] = serde_json::Value::String("C:/definitely/not/here".to_string());
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
        let listed = list_backups(ProviderId::Claude, "metapath_a1").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, info.path);
        assert!(listed[0].path.join("data.uhb").is_file());
        // Purge by id still removes the enumerated directory.
        purge_backup("bk_metapath_01").unwrap();
        assert!(!info.path.exists());
        unregister_test_profile(ProviderId::Claude, "metapath_a1");
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn canonical_requested_root_rejects_relative() {
        assert!(canonical_requested_root(Path::new("relative/path")).is_err());
    }

    #[test]
    fn streaming_consume_matches_hashes_in_bounded_chunks() {
        // Multi-frame file verified through consume_exact: requested sink
        // slices stay frame-bounded, the digest matches, and the stream
        // position advances exactly past every entry plus the terminal
        // marker — no whole-file materialization anywhere.
        let source = unique_dir("streamchunks");
        std::fs::create_dir_all(&source).unwrap();
        let big: Vec<u8> = (0..150_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
            .collect();
        std::fs::write(source.join("big.bin"), &big).unwrap();
        let ctx = test_context(ProviderId::Claude, "stream_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_stream_01", 1_700_000_040)
            .unwrap();
        let dir = super::super::paths::backup_dir_for(ProviderId::Claude, "stream_a1")
            .unwrap()
            .join("bk_stream_01");
        let key_blob = std::fs::read(dir.join("key.dpapi")).unwrap();
        let mut key = dpapi_unprotect(&key_blob).unwrap();
        let mut reader = FrameReader::open(&dir.join("data.uhb"), &key, "bk_stream_01").unwrap();
        let manifest_len = reader
            .take(8)
            .and_then(|bytes| {
                TryInto::<[u8; 8]>::try_into(bytes)
                    .map(u64::from_le_bytes)
                    .map_err(|_| "bad manifest length".to_string())
            })
            .unwrap();
        let manifest_json = reader.take(manifest_len as usize).unwrap();
        let manifest: FileManifest = serde_json::from_slice(&manifest_json).unwrap();
        assert!(manifest
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::File && entry.size > FRAME_CHUNK as u64));
        for entry in &manifest.entries {
            match entry.kind {
                EntryKind::File | EntryKind::Profile => {
                    let mut hasher = Sha256::new();
                    let mut total = 0u64;
                    let mut max_chunk = 0usize;
                    reader
                        .consume_exact(entry.size, |chunk| {
                            max_chunk = max_chunk.max(chunk.len());
                            total += chunk.len() as u64;
                            hasher.update(chunk);
                            Ok(())
                        })
                        .unwrap();
                    assert_eq!(total, entry.size);
                    assert!(
                        max_chunk <= FRAME_CHUNK,
                        "chunk {max_chunk} exceeds one frame"
                    );
                    assert_eq!(hex_encode(&hasher.finalize()), entry.sha256);
                }
                EntryKind::Cred | EntryKind::Link | EntryKind::Dir => {}
            }
        }
        let expected_marker = format!("UHBA-EOF:{}", sha256_hex(&manifest_json));
        let marker = reader.take(expected_marker.len()).unwrap();
        assert_eq!(marker, expected_marker.as_bytes());
        reader.expect_end().unwrap();
        key.zeroize();
        purge_backup("bk_stream_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "stream_a1");
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn identical_preserved_link_restores_without_conflict() {
        // Junction skill link into the shared library, preserved in place:
        // restore must tolerate the identical inode and still recreate
        // deleted files. Needs junction creation; skip vacuously where the
        // platform refuses it.
        let shared = crate::manager::skills::ensure_shared_skills_dir().unwrap();
        std::fs::write(shared.join("tolerant-skill.txt"), "shared").unwrap();
        let account = unique_dir("linktolerant");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("data.txt"), "live-bytes").unwrap();
        let link = account.join("skills");
        if !try_skill_junction(&shared, &link) {
            std::fs::remove_dir_all(&account).ok();
            return;
        }
        let ctx = test_context(ProviderId::Claude, "linktol_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[account.clone()], "bk_linktol_01", 1_700_000_041)
            .unwrap();
        // Delete only the file; the junction stays, exactly as cleanup's
        // preserve-in-place leaves it.
        std::fs::remove_file(account.join("data.txt")).unwrap();
        assert!(crate::manager::paths::is_reparse_point(&link));
        restore_backup("bk_linktol_01").unwrap();
        assert_eq!(
            std::fs::read_to_string(account.join("data.txt")).unwrap(),
            "live-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(link.join("tolerant-skill.txt")).unwrap(),
            "shared"
        );
        purge_backup("bk_linktol_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "linktol_a1");
        std::fs::remove_dir_all(&account).ok();
        std::fs::remove_file(shared.join("tolerant-skill.txt")).ok();
    }

    #[test]
    fn divergent_live_link_still_refuses() {
        let target = unique_dir("divergent-target");
        std::fs::create_dir_all(&target).unwrap();
        assert!(!identical_preserved_link(
            &target,
            &target.join("nope"),
            &[]
        ));
        assert!(!identical_preserved_link(&target, &target, &[]));
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn trailing_garbage_bytes_fail_verification() {
        let source = unique_dir("trail");
        write_tree(&source, &[("data.txt", "payload")]);
        let ctx = test_context(ProviderId::Claude, "trail_a1");
        register_test_profile(&ctx);
        create_backup_for_paths_with_id(&ctx, &[source.clone()], "bk_trail_01", 1_700_000_030)
            .unwrap();
        let data_path = backup_data_path("trail_a1", "bk_trail_01");
        // Even a 1-3 byte remnant (a torn frame length) must fail.
        for tail in [
            &[0x41u8][..],
            &[0x41u8, 0x42u8][..],
            &[0x41u8, 0x42u8, 0x43u8][..],
        ] {
            let mut bytes = std::fs::read(&data_path).unwrap();
            bytes.extend_from_slice(tail);
            std::fs::write(&data_path, &bytes).unwrap();
            let error = verify_backup("bk_trail_01").unwrap_err();
            assert!(error.contains("trailing"), "{error}");
            // Restore the pristine archive for the next tail length.
            std::fs::write(&data_path, &bytes[..bytes.len() - tail.len()]).unwrap();
        }
        verify_backup("bk_trail_01").unwrap();
        purge_backup("bk_trail_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "trail_a1");
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn rollback_removes_only_what_restore_created() {
        let root = unique_dir("rollback");
        let keep = root.join("keep.txt");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(&keep, "live").unwrap();
        // Pre-existing child: the journal never contains `sub` itself, so
        // rollback must leave it (and its original content) in place even
        // when running to a fixpoint.
        std::fs::write(root.join("sub").join("orig.txt"), "orig").unwrap();
        let created_file = root.join("sub").join("new.txt");
        std::fs::write(&created_file, "new").unwrap();
        let created_dir = root.join("sub").join("fresh");
        std::fs::create_dir(&created_dir).unwrap();
        rollback_restore(
            &[created_file.clone()],
            &[],
            &[created_dir.clone()],
            &["UsageHub/definitely-not-a-real-target-rollback".to_string()],
        );
        assert!(!created_file.exists());
        assert!(!created_dir.exists());
        // Pre-existing content and parents are untouched.
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "live");
        assert_eq!(
            std::fs::read_to_string(root.join("sub").join("orig.txt")).unwrap(),
            "orig"
        );
        assert!(root.join("sub").is_dir());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn foreign_vault_target_refused_on_restore() {
        let profile = crate::accounts::AccountProfile {
            id: "vaultbind_a1".to_string(),
            name: "Test".to_string(),
            credential_targets: vec!["UsageHub/owned-target".to_string()],
            ..Default::default()
        };
        let profile_json = serde_json::to_vec(&profile).unwrap();
        let profile_sha = sha256_hex(&profile_json);
        let secret = crate::manager::credentials::CredentialRecord::new(
            "UsageHub/foreign-target".to_string(),
            b"sekrit".to_vec(),
        )
        .unwrap();
        let secret_b64 = crate::manager::credentials::base64_encode(secret.secret_bytes());
        let secret_sha = sha256_hex(secret.secret_bytes());
        drop(secret);
        let entries = vec![
            ManifestEntry {
                rel: "meta/profile.json".to_string(),
                kind: EntryKind::Profile,
                abs: None,
                target: None,
                link_target: None,
                link_is_dir: false,
                size: profile_json.len() as u64,
                sha256: profile_sha,
                secret_b64: None,
            },
            ManifestEntry {
                rel: "cred/0000.cred".to_string(),
                kind: EntryKind::Cred,
                abs: None,
                target: Some("UsageHub/foreign-target".to_string()),
                link_target: None,
                link_is_dir: false,
                size: 6,
                sha256: secret_sha,
                secret_b64: Some(secret_b64),
            },
        ];
        let payloads = vec![EntryPayload::Inline(profile_json), EntryPayload::None];
        let info = seal_archive(
            ProviderId::Claude,
            "vaultbind_a1",
            "bk_vaultbind_01",
            1_700_000_031,
            ManifestKind::Account,
            None,
            Vec::new(),
            entries,
            payloads,
        )
        .unwrap();
        let error = restore_backup("bk_vaultbind_01").unwrap_err();
        assert!(error.contains("not owned"), "{error}");
        purge_backup("bk_vaultbind_01").unwrap();
        unregister_test_profile(ProviderId::Claude, "vaultbind_a1");
        let _ = info;
    }

    #[test]
    fn legacy_restore_confined_to_recorded_root() {
        let root = unique_dir("legacyroot");
        let outside = unique_dir("legacyoutside");
        write_tree(&root, &[("in.txt", "in")]);
        write_tree(&outside, &[("out.txt", "out")]);
        let canonical_outside = canonical_requested_root(&outside.join("out.txt")).unwrap();
        let entries = vec![ManifestEntry {
            rel: "legacy/evil.txt".to_string(),
            kind: EntryKind::File,
            abs: Some(canonical_outside.clone()),
            target: None,
            link_target: None,
            link_is_dir: false,
            size: 3,
            sha256: sha256_hex(b"out"),
            secret_b64: None,
        }];
        let payloads = vec![EntryPayload::Inline(b"out".to_vec())];
        let canonical_root = canonical_requested_root(&root).unwrap();
        let info = seal_archive(
            ProviderId::Claude,
            "legacy_escape_a1",
            "bk_legacy_escape_01",
            1_700_000_032,
            ManifestKind::Legacy,
            Some(canonical_root),
            Vec::new(),
            entries,
            payloads,
        )
        .unwrap();
        let error = restore_backup("bk_legacy_escape_01").unwrap_err();
        assert!(
            error.contains("recorded root") || error.contains("outside"),
            "{error}"
        );
        assert!(std::fs::read_to_string(outside.join("out.txt")).unwrap() == "out");
        purge_backup("bk_legacy_escape_01").unwrap();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&outside).ok();
        let _ = info;
    }

    #[test]
    fn library_bytes_refused_at_collect() {
        let library = crate::manager::skills::ensure_shared_skills_dir().unwrap();
        let probe = library.join("probe-skill-file.txt");
        std::fs::write(&probe, "library content").unwrap();
        let mut ctx = test_context(ProviderId::Claude, "librefuse_a1");
        ctx.source = crate::accounts::SourceKind::CredentialFile;
        ctx.config_dir = Some(library.clone());
        register_test_profile(&ctx);
        let error = create_backup_with_id(&ctx, "bk_librefuse_01", 1_700_000_033).unwrap_err();
        assert!(error.contains("shared-library"), "{error}");
        unregister_test_profile(ProviderId::Claude, "librefuse_a1");
        std::fs::remove_file(&probe).ok();
    }

    #[test]
    fn targeted_legacy_import_restores_only_missing() {
        let root = unique_dir("legacybridge");
        write_tree(
            &root,
            &[
                ("profiles/Second/.credentials.json", "{\"bridge\":true}"),
                ("settings.json", "{\"keep\":true}"),
            ],
        );
        let info = import_legacy_paths(
            &root,
            &[root.join("profiles/Second/.credentials.json")],
            ProviderId::Claude,
            "legacy-bridge-demo",
        )
        .unwrap();
        assert!(info.account_id.starts_with(LEGACY_ACCOUNT_PREFIX));
        // Simulate the cutover: only the bridge export is deleted.
        std::fs::remove_file(root.join("profiles/Second/.credentials.json")).unwrap();
        restore_backup(&info.backup_id).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("profiles/Second/.credentials.json")).unwrap(),
            "{\"bridge\":true}"
        );
        // Untouched files stay put — and a repeat restore now refuses
        // instead of silently claiming success: occupied live data is
        // never overwritten.
        assert_eq!(
            std::fs::read_to_string(root.join("settings.json")).unwrap(),
            "{\"keep\":true}"
        );
        let error = restore_backup(&info.backup_id).unwrap_err();
        assert!(error.contains("occupied"), "{error}");
        // Unknown registry footprint, no vault writes.
        let settings = super::super::state::load_fresh();
        assert!(settings
            .accounts
            .claude
            .profiles
            .iter()
            .all(|profile| profile.id != info.account_id));
        purge_backup(&info.backup_id).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }
}
