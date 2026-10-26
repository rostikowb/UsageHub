//! Confirmed-owned Windows Credential Manager access for account snapshots.
//!
//! Only credential targets explicitly listed on an [`AccountContext`]
//! (`AccountProfile.credential_targets`) are ever read, deleted, or restored.
//! Unknown targets are reported by callers, never guessed; there is no
//! prefix enumeration or vault-wide wipe here. Secrets live in
//! [`CredentialRecord`] (redacted [`Debug`](std::fmt::Debug), zeroized on
//! drop) and are persisted only inside the encrypted backup stream owned by
//! `manager::backup` — never in settings JSON, logs, or CLI output.
//!
//! Backend: the supported Windows Credential Manager generic-credential APIs
//! (`CredReadW` / `CredWriteW` / `CredDeleteW`) in the current user's vault.
//! Non-Windows builds return a plain error; no fallback store is provided so
//! secrets can never silently land in an unapproved location.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroize;

use crate::manager::{AccountContext, ManagerResult};

/// Maximum accepted credential-target name length. The Win32 limit is far
/// larger; this bound only rejects garbage before it reaches the vault API.
const MAX_TARGET_LEN: usize = 512;

/// A snapshot of one confirmed-owned generic credential.
///
/// The secret is held as raw vault bytes (UTF-8 or UTF-16LE payloads are both
/// preserved verbatim) and is redacted from [`Debug`](std::fmt::Debug) output
/// and zeroized when the record drops.
pub struct CredentialRecord {
    /// Exact vault target name, e.g. `UsageHub/claude/<account-id>`.
    pub target: String,
    secret: Vec<u8>,
}

impl CredentialRecord {
    /// Build a record from explicitly owned material. Fails on empty,
    /// NUL-containing, or over-long targets and on empty secrets so neither
    /// half-written captures nor vault writes can smuggle invalid entries.
    pub fn new(target: String, secret: Vec<u8>) -> ManagerResult<Self> {
        validate_target(&target)?;
        if secret.is_empty() {
            return Err(format!("Refusing to store an empty secret for '{target}'"));
        }
        if secret.len() > MAX_VAULT_SECRET_BYTES {
            return Err(format!(
                "Refusing to store an oversized secret for '{target}'"
            ));
        }
        Ok(Self { target, secret })
    }
    /// Raw secret bytes. Callers must not log, serialize to diagnostics, or
    /// otherwise disclose the returned slice.
    pub fn secret_bytes(&self) -> &[u8] {
        &self.secret
    }
}

impl fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("target", &self.target)
            .field(
                "secret",
                &format_args!("[redacted {} bytes]", self.secret.len()),
            )
            .finish()
    }
}

impl Drop for CredentialRecord {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

impl Clone for CredentialRecord {
    fn clone(&self) -> Self {
        Self {
            target: self.target.clone(),
            secret: self.secret.clone(),
        }
    }
}

impl PartialEq for CredentialRecord {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.secret == other.secret
    }
}

impl Eq for CredentialRecord {}

/// Wire form: target plus standard-base64 secret. Used for the encrypted
/// archive manifest (which is itself AES-GCM sealed) — never for logs or
/// settings.
#[derive(Serialize, Deserialize)]
struct CredentialWire {
    target: String,
    secret_b64: String,
}

fn scrub_string(secret: &mut String) {
    // Volatile, fence-guarded wipe via the `zeroize` crate; the allocation
    // itself is freed only after scrubbing.
    secret.zeroize();
}

impl Serialize for CredentialRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut wire = CredentialWire {
            target: self.target.clone(),
            secret_b64: base64_encode(&self.secret),
        };
        let result = wire.serialize(serializer);
        scrub_string(&mut wire.secret_b64);
        result
    }
}

impl<'de> Deserialize<'de> for CredentialRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut wire = CredentialWire::deserialize(deserializer)?;
        let outcome: Result<Self, String> = (|| {
            let secret = base64_decode(&wire.secret_b64)?;
            CredentialRecord::new(wire.target.clone(), secret)
        })();
        scrub_string(&mut wire.secret_b64);
        outcome.map_err(serde::de::Error::custom)
    }
}

fn validate_target(target: &str) -> ManagerResult<()> {
    if target.trim().is_empty() {
        return Err("Refusing to touch an empty credential target".into());
    }
    if target.bytes().any(|byte| byte == 0) {
        return Err("Refusing to touch a NUL-containing credential target".into());
    }
    if target.len() > MAX_TARGET_LEN {
        return Err(format!(
            "Refusing to touch an over-long credential target ({} bytes)",
            target.len()
        ));
    }
    Ok(())
}

fn dedup_targets(ctx: &AccountContext) -> ManagerResult<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for raw in &ctx.profile.credential_targets {
        let target = raw.trim().to_string();
        if target.is_empty() {
            continue;
        }
        validate_target(&target)?;
        if seen.insert(target.clone()) {
            out.push(target);
        }
    }
    Ok(out)
}

/// Read every confirmed-owned target that currently exists in the vault.
/// Missing targets are skipped (a signed-out account simply has nothing to
/// snapshot); this is read-only and never creates or modifies vault entries.
pub fn capture_owned(ctx: &AccountContext) -> ManagerResult<Vec<CredentialRecord>> {
    let mut records = Vec::new();
    for target in dedup_targets(ctx)? {
        match cred_read_raw(&target)? {
            // Empty-blob targets exist (conflict-relevant) but carry no
            // bytes; there is nothing to snapshot for them.
            Some(secret) if !secret.is_empty() => {
                records.push(CredentialRecord::new(target, secret)?);
            }
            _ => {}
        }
    }
    Ok(records)
}

/// Best-effort removal of one exact target, for restore rollback only.
/// Never enumerates; returns `Ok(())` when the target is already absent.
pub(crate) fn delete_single(target: &str) -> ManagerResult<()> {
    validate_target(target)?;
    cred_delete_raw(target).map(|_| ())
}
/// True when a generic credential with this exact target exists in the
/// current user's vault. Read-only; used for pre-restore conflict checks.
pub fn credential_exists(target: &str) -> ManagerResult<bool> {
    validate_target(target)?;
    Ok(cred_read_raw(target)?.is_some())
}

/// Delete every confirmed-owned target that exists. Returns the targets that
/// were actually removed. Only targets listed on the account profile are
/// touched — there is no enumeration and no prefix wipe.
pub fn delete_owned(ctx: &AccountContext) -> ManagerResult<Vec<String>> {
    let mut deleted = Vec::new();
    for target in dedup_targets(ctx)? {
        if cred_delete_raw(&target)? {
            deleted.push(target);
        }
    }
    Ok(deleted)
}

/// Restore previously captured records. Refuses the whole restore when any
/// target already exists in the vault (live credential conflict) so a
/// restore can never silently overwrite a current login. Existence is
/// re-checked immediately before each write to close the preflight/write
/// race; on any failure the targets this call already wrote are removed
/// again (best-effort) so callers can roll back files without touching
/// live pre-existing entries.
pub fn restore_owned(records: &[CredentialRecord]) -> ManagerResult<()> {
    let mut conflicts = Vec::new();
    for record in records {
        validate_target(&record.target)?;
        if cred_read_raw(&record.target)?.is_some() {
            conflicts.push(record.target.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(format!(
            "Refusing credential restore: live credentials already exist for {}",
            conflicts.join(", ")
        ));
    }
    let mut written: Vec<&str> = Vec::new();
    for record in records {
        // Re-check under the write: a target that appeared after the
        // preflight belongs to a live login and must never be overwritten.
        if cred_read_raw(&record.target)?.is_some() {
            rollback_written(&written);
            return Err(format!(
                "Refusing credential restore: live credential appeared for '{}'",
                record.target
            ));
        }
        if let Err(error) = cred_write_raw(&record.target, &record.secret) {
            rollback_written(&written);
            return Err(format!(
                "Unable to store credential '{}': {error}",
                record.target
            ));
        }
        written.push(&record.target);
    }
    Ok(())
}

/// Best-effort removal of exactly the targets this restore wrote, in
/// reverse order. Never touches entries that pre-date the call.
fn rollback_written(written: &[&str]) {
    for target in written.iter().rev() {
        let _ = cred_delete_raw(target);
    }
}

// ---------------------------------------------------------------------------
// Windows Credential Manager backend (generic credentials, current user).
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[repr(C)]
struct CredentialW {
    flags: u32,
    type_: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: u64,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut std::ffi::c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[cfg(windows)]
#[link(name = "Advapi32")]
extern "system" {
    fn CredReadW(
        target_name: *const u16,
        type_: u32,
        reserved_flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
    fn CredDeleteW(target_name: *const u16, type_: u32, flags: u32) -> i32;
    fn CredFree(buffer: *mut std::ffi::c_void);
}

#[cfg(windows)]
extern "system" {
    fn GetLastError() -> u32;
}

#[cfg(windows)]
const CRED_TYPE_GENERIC: u32 = 1;
#[cfg(windows)]
const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
#[cfg(windows)]
const ERROR_NOT_FOUND: u32 = 1168;

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Maximum accepted vault secret size (4 MiB). Windows generic-credential
/// blobs are small tokens; anything larger is rejected before allocation
/// so a corrupt vault entry cannot force a huge buffer.
pub(crate) const MAX_VAULT_SECRET_BYTES: usize = 4 * 1024 * 1024;

#[cfg(windows)]
fn cred_read_raw(target: &str) -> ManagerResult<Option<Vec<u8>>> {
    let target_wide = wide(target);
    let mut credential: *mut CredentialW = std::ptr::null_mut();
    let ok = unsafe { CredReadW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if ok == 0 || credential.is_null() {
        // A failed read must never masquerade as "no credential": restore
        // refuses to overwrite live targets, and that refusal is only sound
        // when absence is proven by ERROR_NOT_FOUND.
        return match unsafe { GetLastError() } {
            ERROR_NOT_FOUND => Ok(None),
            code => Err(format!(
                "Unable to read credential '{target}' (Win32 {code})"
            )),
        };
    }
    let secret = unsafe {
        let reference = &*credential;
        if reference.credential_blob_size == 0 || reference.credential_blob.is_null() {
            // An existing target with an empty blob counts as present for
            // conflict purposes (never overwrite it); capture skips it
            // because there are no bytes to carry.
            Some(Vec::new())
        } else {
            let size = reference.credential_blob_size as usize;
            if size > MAX_VAULT_SECRET_BYTES {
                CredFree(credential as *mut std::ffi::c_void);
                return Err(format!("Credential '{target}' blob exceeds size caps"));
            }
            Some(std::slice::from_raw_parts(reference.credential_blob, size).to_vec())
        }
    };
    unsafe { CredFree(credential as *mut std::ffi::c_void) };
    Ok(secret)
}

#[cfg(windows)]
fn cred_write_raw(target: &str, secret: &[u8]) -> ManagerResult<()> {
    if secret.is_empty() {
        return Err(format!("Refusing to write an empty secret for '{target}'"));
    }
    let target_wide = wide(target);
    // The blob pointer is read by `CredWriteW` synchronously; the record's
    // own allocation outlives the call.
    let credential = CredentialW {
        flags: 0,
        type_: CRED_TYPE_GENERIC,
        target_name: target_wide.as_ptr() as *mut u16,
        comment: std::ptr::null_mut(),
        last_written: 0,
        credential_blob_size: u32::try_from(secret.len())
            .map_err(|_| format!("Secret too large for '{target}'"))?,
        credential_blob: secret.as_ptr() as *mut u8,
        persist: CRED_PERSIST_LOCAL_MACHINE,
        attribute_count: 0,
        attributes: std::ptr::null_mut(),
        target_alias: std::ptr::null_mut(),
        user_name: std::ptr::null_mut(),
    };
    let ok = unsafe { CredWriteW(&credential, 0) };
    if ok == 0 {
        return Err(format!("Unable to store credential '{target}'"));
    }
    Ok(())
}

#[cfg(windows)]
fn cred_delete_raw(target: &str) -> ManagerResult<bool> {
    let target_wide = wide(target);
    let ok = unsafe { CredDeleteW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0) };
    if ok != 0 {
        return Ok(true);
    }
    match unsafe { GetLastError() } {
        ERROR_NOT_FOUND => Ok(false),
        _ => Err(format!("Unable to delete credential '{target}'")),
    }
}

#[cfg(not(windows))]
fn cred_read_raw(_target: &str) -> ManagerResult<Option<Vec<u8>>> {
    Err("Windows Credential Manager is only available on Windows".into())
}

#[cfg(not(windows))]
fn cred_write_raw(_target: &str, _secret: &[u8]) -> ManagerResult<()> {
    Err("Windows Credential Manager is only available on Windows".into())
}

#[cfg(not(windows))]
fn cred_delete_raw(_target: &str) -> ManagerResult<bool> {
    Err("Windows Credential Manager is only available on Windows".into())
}

// ---------------------------------------------------------------------------
// Minimal base64 (standard alphabet) so credential bytes survive JSON
// without a new dependency. Shared with the backup manifest only.
// ---------------------------------------------------------------------------

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer: u32 = 0;
        for (index, byte) in chunk.iter().enumerate() {
            buffer |= (*byte as u32) << (16 - 8 * index);
        }
        let padding = 3 - chunk.len();
        for index in 0..4 - padding {
            out.push(ALPHABET[((buffer >> (18 - 6 * index)) & 0x3f) as usize] as char);
        }
        for _ in 0..padding {
            out.push('=');
        }
    }
    out
}

pub(crate) fn base64_decode(input: &str) -> ManagerResult<Vec<u8>> {
    let mut values = Vec::with_capacity(input.len());
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err("Invalid base64 in credential record".into()),
        };
        values.push(value);
    }
    if values.len() % 4 == 1 {
        return Err("Invalid base64 length in credential record".into());
    }
    let padding = input.bytes().rev().take_while(|byte| *byte == b'=').count();
    if padding > 2 {
        return Err("Invalid base64 padding in credential record".into());
    }
    let mut out = Vec::with_capacity(values.len() / 4 * 3);
    for chunk in values.chunks(4) {
        if chunk.len() < 2 {
            break;
        }
        let mut buffer: u32 = 0;
        for (index, value) in chunk.iter().enumerate() {
            buffer |= (*value as u32) << (18 - 6 * index);
        }
        let bytes = if chunk.len() == 4 { 3 } else { chunk.len() - 1 };
        for index in 0..bytes {
            out.push(((buffer >> (16 - 8 * index)) & 0xff) as u8);
        }
    }
    // A trailing `=` run must only pad the final quantum.
    if input.contains('=')
        && !input
            .trim_end_matches('=')
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
    {
        return Err("Invalid base64 in credential record".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secret_bytes() {
        let record =
            CredentialRecord::new("UsageHub/test".into(), b"super-secret".to_vec()).unwrap();
        let rendered = format!("{record:?}");
        assert!(rendered.contains("UsageHub/test"), "{rendered}");
        assert!(rendered.contains("[redacted 12 bytes]"), "{rendered}");
        assert!(!rendered.contains("super-secret"), "{rendered}");
    }

    #[test]
    fn serde_roundtrip_preserves_bytes_without_raw_secret_text() {
        let secret = b"token-\x00\xff-bytes".to_vec();
        let record = CredentialRecord::new("UsageHub/claude/a1".into(), secret.clone()).unwrap();
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("token-"), "{json}");
        let back: CredentialRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);
        assert_eq!(back.secret_bytes(), secret.as_slice());
    }

    #[test]
    fn target_validation_rejects_garbage() {
        assert!(CredentialRecord::new(String::new(), b"x".to_vec()).is_err());
        assert!(CredentialRecord::new("   ".into(), b"x".to_vec()).is_err());
        assert!(CredentialRecord::new("a\0b".into(), b"x".to_vec()).is_err());
        assert!(CredentialRecord::new("t".into(), Vec::new()).is_err());
        assert!(CredentialRecord::new("t".repeat(513), b"x".to_vec()).is_err());
        assert!(CredentialRecord::new("UsageHub/ok".into(), b"x".to_vec()).is_ok());
    }

    #[test]
    fn base64_roundtrip_vectors() {
        for input in [&[][..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let encoded = base64_encode(input);
            let decoded = base64_decode(&encoded).unwrap();
            assert_eq!(decoded, input, "{encoded}");
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert!(base64_decode("!!!").is_err());
        assert!(base64_decode("A").is_err());
    }

    fn test_context(targets: Vec<String>) -> AccountContext {
        AccountContext {
            provider: crate::providers::ProviderId::Claude,
            profile: crate::accounts::AccountProfile {
                id: "test_a1".to_string(),
                name: "Test".to_string(),
                credential_targets: targets,
                ..Default::default()
            },
            source: crate::accounts::SourceKind::ProviderDefault,
            config_dir: None,
            desktop_dir: None,
            credential_file: None,
            ownership: crate::accounts::AccountOwnership::Managed,
        }
    }

    #[cfg(windows)]
    #[test]
    fn capture_missing_target_returns_empty_without_mutation() {
        let ctx = test_context(vec![
            "UsageHub/definitely-not-a-real-target-9f3c1a".to_string()
        ]);
        let captured = capture_owned(&ctx).unwrap();
        assert!(captured.is_empty());
    }

    #[test]
    fn duplicates_and_blanks_deduplicated() {
        let ctx = test_context(vec![
            "  UsageHub/a  ".to_string(),
            String::new(),
            "UsageHub/a".to_string(),
        ]);
        let deduped = dedup_targets(&ctx).unwrap();
        assert_eq!(deduped, vec!["UsageHub/a".to_string()]);
    }
}
