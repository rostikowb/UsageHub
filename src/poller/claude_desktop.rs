//! Reads the OAuth token that the Claude desktop app keeps for its bundled
//! Claude Code build.
//!
//! Machines that only ever ran Claude Code through the desktop app have no
//! `~/.claude/.credentials.json`, because that file is written by the
//! standalone CLI login flow. The desktop app is an Electron application and
//! stores its token cache with Chromium's OSCrypt scheme instead: an
//! AES-256-GCM key sits DPAPI-wrapped in `Local State`, and each encrypted
//! value is `"v10" || nonce || ciphertext || tag`.
//!
//! Everything here is read-only, runs as the signed-in user, and degrades to
//! `None` whenever the layout is not what we expect.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::diagnose;

/// Newest layout first. The desktop app migrated its cache to
/// `oauth:tokenCacheV2` and leaves the older `oauth:tokenCache` key in place,
/// so both are tried and the first that yields a usable token wins.
const TOKEN_CACHE_KEYS: &[&str] = &["oauth:tokenCacheV2", "oauth:tokenCache"];
const DPAPI_KEY_PREFIX: &[u8] = b"DPAPI";
const OS_CRYPT_PREFIX: &[u8] = b"v10";
const GCM_NONCE_LEN: usize = 12;
const GCM_TAG_LEN: usize = 16;
/// Desktop entries are keyed `"<install>:<user>:<base url>:<scopes>"`; the
/// usage endpoint needs both inference and profile scopes.
const INFERENCE_SCOPE: &str = "user:inference";
const PROFILE_SCOPE: &str = "user:profile";
const BCRYPT_INIT_AUTH_MODE_INFO_VERSION: u32 = 1;

pub(super) struct DesktopToken {
    pub(super) access_token: String,
    pub(super) expires_at: Option<i64>,
}

pub(super) fn config_paths() -> Vec<PathBuf> {
    data_directories()
        .into_iter()
        .map(|path| path.join("config.json"))
        .collect()
}

pub(super) fn data_directories() -> Vec<PathBuf> {
    data_directories_in(
        crate::known_folders::roaming_app_data_dir().as_deref(),
        crate::known_folders::local_app_data_dir().as_deref(),
    )
}

/// Prefer the migrated desktop data over legacy caches left behind by updates.
/// Keep ordinary and MSIX paths for older installs, with deterministic discovery.
/// Missing config files remain watchable candidates.
fn data_directories_in(roaming: Option<&Path>, local: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = local
        .map(|path| publisher_directories_in(&path.join("Claude-Data"), "claude_"))
        .unwrap_or_default();
    paths.extend(roaming.map(|path| path.join("Claude")));
    if let Some(local) = local {
        paths.extend(
            publisher_directories_in(&local.join("Packages"), "Claude_")
                .into_iter()
                .map(|path| path.join("LocalCache").join("Roaming").join("Claude")),
        );
    }
    paths
}

fn publisher_directories_in(root: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_prefix(prefix).is_some_and(|publisher| {
                    publisher.len() == 13
                        && publisher
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
            })
        })
        .map(|entry| entry.path())
        .collect();
    let preferred = format!("{prefix}pzs8sxrjxfjjc");
    paths.sort_by_key(|path| {
        (
            path.file_name()
                .is_none_or(|name| name != preferred.as_str()),
            path.clone(),
        )
    });
    paths
}

fn local_state_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("Local State")
}

pub(super) fn read_token(config_path: &Path) -> Option<DesktopToken> {
    let config = match std::fs::read_to_string(config_path) {
        Ok(config) => config,
        Err(error) => {
            if diagnose::is_enabled() {
                diagnose::log_error(
                    &format!(
                        "unable to read Claude desktop config at {}",
                        config_path.display()
                    ),
                    error,
                );
            }
            return None;
        }
    };

    let caches = token_cache_values(&config);
    if caches.is_empty() {
        diagnose::log("Claude desktop config held no OAuth token cache");
        return None;
    }
    // The config's own identity binding: when present, only cache entries
    // for this account UUID may be selected. Any other UUID's entries are
    // never a fallback, even when fresher or better scoped.
    let current = current_account_uuid(&config);
    let key = os_crypt_key(&local_state_path(config_path))?;

    for (name, cache) in &caches {
        let Some(plaintext) = decrypt_os_crypt_value(cache, &key) else {
            diagnose::log(format!("unable to decrypt Claude desktop {name}"));
            continue;
        };
        let Ok(plaintext) = String::from_utf8(plaintext) else {
            diagnose::log(format!("Claude desktop {name} was not valid UTF-8"));
            continue;
        };
        if let Some(token) = select_token_for(&plaintext, current.as_deref()) {
            return Some(token);
        }
        diagnose::log(format!(
            "Claude desktop {name} held no usable inference token"
        ));
    }

    None
}

/// The config's `lastKnownAccountUuid` identity binding, when present and
/// non-empty. Missing or blank means the install predates the binding and
/// keeps the legacy scope/expiry selection.
fn current_account_uuid(config: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(config)
        .ok()?
        .get("lastKnownAccountUuid")?
        .as_str()
        .filter(|uuid| !uuid.trim().is_empty())
        .map(str::to_string)
}

/// Config + Local State signature for one explicit desktop data directory.
/// The token-cache contents identify a login change; the `Local State` file
/// carries the OSCrypt key, so a key rotation must read as a change too.
/// Both are content-based: unrelated `config.json` rewrites (window
/// placement) stay invisible while a token rotation resumes only this account.
pub(super) fn signature_for_dir(dir: &Path) -> String {
    let config_path = dir.join("config.json");
    let mut signature = watch_signature(&config_path);
    signature.push_str("|local-state:");
    signature.push_str(&crate::accounts::file_signature(&local_state_path(
        &config_path,
    )));
    signature
}

/// The `config.json` holding the OAuth token cache for an explicit desktop
/// data directory (`SourceKind::DesktopDir` profiles).
pub(super) fn config_path_for_dir(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

/// Read-only token read for an explicit desktop data directory. Never falls
/// back to CLI credential discovery or another directory's cache.
pub(super) fn read_token_for_dir(dir: &Path) -> Option<DesktopToken> {
    read_token(&config_path_for_dir(dir))
}

/// Signature over the encrypted cache rather than the file's mtime: the
/// desktop app rewrites `config.json` for unrelated state such as window
/// placement, and that must not read as a credential change.
pub(super) fn watch_signature(config_path: &Path) -> String {
    let key = format!("desktop:{}", config_path.display());
    let config = std::fs::read_to_string(config_path).ok();
    let caches = config
        .as_deref()
        .map(token_cache_values)
        .unwrap_or_default();
    // The identity binding is credential material: an account switch that
    // reuses cache bytes must still read as a change.
    let uuid = config
        .as_deref()
        .and_then(current_account_uuid)
        .map(|uuid| format!("|uuid:{}", fnv1a(uuid.as_bytes())))
        .unwrap_or_else(|| "|uuid:missing".to_string());
    if caches.is_empty() {
        return format!("{key}|missing{uuid}");
    }

    let mut signature = format!("{key}|present{uuid}");
    for (name, cache) in caches {
        signature.push_str(&format!("|{name}:{}", fnv1a(cache.as_bytes())));
    }
    signature
}

/// Every token cache the config carries, newest layout first.
fn token_cache_values(config: &str) -> Vec<(&'static str, String)> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(config) else {
        return Vec::new();
    };
    TOKEN_CACHE_KEYS
        .iter()
        .filter_map(|key| {
            let value = json.get(*key)?.as_str()?;
            (!value.is_empty()).then(|| (*key, value.to_string()))
        })
        .collect()
}

/// Legacy unbound selection: scope/expiry ranking only (tests only).
#[cfg(test)]
fn select_token(plaintext: &str) -> Option<DesktopToken> {
    select_token_for(plaintext, None)
}

/// Prefer both scopes required for usage, then inference-only, then unknown
/// layouts for compatibility. Expiry breaks ties within each scope group.
///
/// When the config binds an account UUID (`current`), ONLY entries for that
/// UUID may be selected: any other UUID's entries are never a fallback, even
/// when fresher or better scoped. A bound UUID with no matching entry reads
/// as no login; a matching but expired entry still surfaces so the caller
/// reports the owning account's expiry (`TokenExpired`). Without a binding
/// the legacy scope/expiry selection applies.
fn select_token_for(plaintext: &str, current: Option<&str>) -> Option<DesktopToken> {
    let json: serde_json::Value = serde_json::from_str(plaintext).ok()?;
    let entries = json.as_object()?;

    struct Candidate<'a> {
        user: &'a str,
        scopes: Vec<&'a str>,
        access_token: &'a str,
        expires_at: Option<i64>,
    }
    let mut candidates = Vec::new();
    for (key, entry) in entries {
        let Some(access_token) = entry.get("token").and_then(|value| value.as_str()) else {
            continue;
        };
        if access_token.trim().is_empty() {
            continue;
        }
        let user = entry_user(key);
        if current.is_some_and(|current| user != current) {
            continue;
        }
        candidates.push(Candidate {
            user,
            scopes: entry_scopes(key),
            access_token,
            expires_at: entry.get("expiresAt").and_then(|value| value.as_i64()),
        });
    }
    // A missing `expiresAt` matches the CLI reader: no known expiry means
    // the token is still usable.
    let any_current = candidates
        .iter()
        .any(|candidate| !is_expired(candidate.expires_at));
    let mut best: Option<((bool, bool, i64, &str), DesktopToken)> = None;
    for candidate in candidates {
        if any_current && is_expired(candidate.expires_at) {
            continue;
        }
        let inference = candidate.scopes.contains(&INFERENCE_SCOPE);
        let rank = (
            inference && candidate.scopes.contains(&PROFILE_SCOPE),
            inference,
            candidate.expires_at.unwrap_or(i64::MIN),
            candidate.user,
        );
        if best
            .as_ref()
            .is_some_and(|(best_rank, _)| *best_rank >= rank)
        {
            continue;
        }
        best = Some((
            rank,
            DesktopToken {
                access_token: candidate.access_token.to_string(),
                expires_at: candidate.expires_at,
            },
        ));
    }

    best.map(|(_, token)| token)
}

/// Account identity in a desktop cache key. Current caches use
/// `acct:<account>|<organization>:<client>:<base URL>:<scopes>`; older
/// caches use `<install>:<account>:<base URL>:<scopes>`.
fn entry_user(key: &str) -> &str {
    let first = key.split_whitespace().next().unwrap_or_default();
    let Some(scheme) = first.rfind("://") else {
        return "";
    };
    let Some(url_colon) = first[..scheme].rfind(':') else {
        return "";
    };
    let Some(first_colon) = first.find(':') else {
        return "";
    };
    if first_colon >= url_colon {
        return "";
    }
    let identity = &first[first_colon + 1..url_colon];
    if &first[..first_colon] == "acct" {
        let Some((account, organization_client)) = identity.split_once('|') else {
            return "";
        };
        let Some((organization, client)) = organization_client.split_once(':') else {
            return "";
        };
        if account.is_empty() || organization.is_empty() || client.is_empty() {
            return "";
        }
        account
    } else {
        identity
    }
}

fn is_expired(expires_at: Option<i64>) -> bool {
    expires_at.is_some_and(|expires_at| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        now >= expires_at
    })
}
fn entry_scopes(key: &str) -> Vec<&str> {
    let mut words = key.split_whitespace();
    let first = words.next().unwrap_or_default();
    // The first scope follows the ids and base URL and contains one colon.
    let first_scope = first
        .rmatch_indices(':')
        .nth(1)
        .map(|(index, _)| &first[index + 1..]);
    first_scope.into_iter().chain(words).collect()
}

fn os_crypt_key(local_state_path: &Path) -> Option<Vec<u8>> {
    let local_state = std::fs::read_to_string(local_state_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&local_state).ok()?;
    let encoded = json.get("os_crypt")?.get("encrypted_key")?.as_str()?;
    let wrapped = base64_decode(encoded)?;
    let wrapped = wrapped.strip_prefix(DPAPI_KEY_PREFIX)?;
    dpapi_unprotect(wrapped)
}

fn decrypt_os_crypt_value(value: &str, key: &[u8]) -> Option<Vec<u8>> {
    let blob = base64_decode(value)?;
    let body = blob.strip_prefix(OS_CRYPT_PREFIX)?;
    if body.len() < GCM_NONCE_LEN + GCM_TAG_LEN {
        return None;
    }
    let (nonce, rest) = body.split_at(GCM_NONCE_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - GCM_TAG_LEN);
    aes_gcm_decrypt(key, nonce, ciphertext, tag)
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let input = input.trim_end_matches('=');
    if input.len() % 4 == 1 {
        return None;
    }
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    let padding_mask = (1u32 << bits).saturating_sub(1);
    (buffer & padding_mask == 0).then_some(output)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[repr(C)]
struct CryptIntegerBlob {
    cb_data: u32,
    pb_data: *mut u8,
}

#[repr(C)]
struct AuthenticatedCipherModeInfo {
    cb_size: u32,
    dw_info_version: u32,
    pb_nonce: *mut u8,
    cb_nonce: u32,
    pb_auth_data: *mut u8,
    cb_auth_data: u32,
    pb_tag: *mut u8,
    cb_tag: u32,
    pb_mac_context: *mut u8,
    cb_mac_context: u32,
    cb_aad: u32,
    cb_data: u64,
    dw_flags: u32,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptUnprotectData(
        data_in: *const CryptIntegerBlob,
        data_description: *mut *mut u16,
        optional_entropy: *const CryptIntegerBlob,
        reserved: *mut c_void,
        prompt_struct: *mut c_void,
        flags: u32,
        data_out: *mut CryptIntegerBlob,
    ) -> i32;
}

extern "system" {
    fn LocalFree(mem: *mut c_void) -> *mut c_void;
}

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(
        algorithm: *mut *mut c_void,
        id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    fn BCryptGetProperty(
        object: *mut c_void,
        property: *const u16,
        output: *mut u8,
        output_len: u32,
        result: *mut u32,
        flags: u32,
    ) -> i32;
    fn BCryptSetProperty(
        object: *mut c_void,
        property: *const u16,
        input: *const u8,
        input_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptGenerateSymmetricKey(
        algorithm: *mut c_void,
        key: *mut *mut c_void,
        key_object: *mut u8,
        key_object_len: u32,
        secret: *const u8,
        secret_len: u32,
        flags: u32,
    ) -> i32;
    fn BCryptDestroyKey(key: *mut c_void) -> i32;
    fn BCryptDecrypt(
        key: *mut c_void,
        input: *const u8,
        input_len: u32,
        padding_info: *const c_void,
        iv: *mut u8,
        iv_len: u32,
        output: *mut u8,
        output_len: u32,
        result: *mut u32,
        flags: u32,
    ) -> i32;
}

fn dpapi_unprotect(data: &[u8]) -> Option<Vec<u8>> {
    let input = CryptIntegerBlob {
        cb_data: u32::try_from(data.len()).ok()?,
        pb_data: data.as_ptr() as *mut u8,
    };
    let mut output = CryptIntegerBlob {
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
            0,
            &mut output,
        )
    };

    if ok == 0 || output.pb_data.is_null() {
        diagnose::log("unable to unwrap the Claude desktop OSCrypt key with DPAPI");
        return None;
    }

    Some(unsafe {
        let key = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize).to_vec();
        LocalFree(output.pb_data as *mut c_void);
        key
    })
}

fn aes_gcm_decrypt(key: &[u8], nonce: &[u8], ciphertext: &[u8], tag: &[u8]) -> Option<Vec<u8>> {
    let algorithm_id = wide("AES");
    let mut algorithm: *mut c_void = std::ptr::null_mut();
    if unsafe {
        BCryptOpenAlgorithmProvider(&mut algorithm, algorithm_id.as_ptr(), std::ptr::null(), 0)
    } != 0
    {
        diagnose::log("unable to open the AES provider for the Claude desktop token cache");
        return None;
    }

    let plaintext = with_gcm_key(algorithm, key, |key_handle| {
        decrypt_with_key(key_handle, nonce, ciphertext, tag)
    });

    unsafe { BCryptCloseAlgorithmProvider(algorithm, 0) };
    plaintext
}

fn with_gcm_key(
    algorithm: *mut c_void,
    key: &[u8],
    decrypt: impl FnOnce(*mut c_void) -> Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let chaining_property = wide("ChainingMode");
    let chaining_gcm = wide("ChainingModeGCM");
    if unsafe {
        BCryptSetProperty(
            algorithm,
            chaining_property.as_ptr(),
            chaining_gcm.as_ptr() as *const u8,
            u32::try_from(std::mem::size_of_val(chaining_gcm.as_slice())).ok()?,
            0,
        )
    } != 0
    {
        diagnose::log("unable to select GCM chaining for the Claude desktop token cache");
        return None;
    }

    let object_length_property = wide("ObjectLength");
    let mut object_length = 0u32;
    let mut written = 0u32;
    if unsafe {
        BCryptGetProperty(
            algorithm,
            object_length_property.as_ptr(),
            &mut object_length as *mut u32 as *mut u8,
            u32::try_from(std::mem::size_of::<u32>()).ok()?,
            &mut written,
            0,
        )
    } != 0
    {
        return None;
    }

    // The key object buffer must outlive the key handle it backs.
    let mut key_object = vec![0u8; object_length as usize];
    let mut key_handle: *mut c_void = std::ptr::null_mut();
    if unsafe {
        BCryptGenerateSymmetricKey(
            algorithm,
            &mut key_handle,
            key_object.as_mut_ptr(),
            object_length,
            key.as_ptr(),
            u32::try_from(key.len()).ok()?,
            0,
        )
    } != 0
    {
        diagnose::log("unable to import the Claude desktop OSCrypt key");
        return None;
    }

    let plaintext = decrypt(key_handle);
    unsafe { BCryptDestroyKey(key_handle) };
    drop(key_object);
    plaintext
}

fn decrypt_with_key(
    key_handle: *mut c_void,
    nonce: &[u8],
    ciphertext: &[u8],
    tag: &[u8],
) -> Option<Vec<u8>> {
    let mut nonce = nonce.to_vec();
    let mut tag = tag.to_vec();
    let mode_info = AuthenticatedCipherModeInfo {
        cb_size: u32::try_from(std::mem::size_of::<AuthenticatedCipherModeInfo>()).ok()?,
        dw_info_version: BCRYPT_INIT_AUTH_MODE_INFO_VERSION,
        pb_nonce: nonce.as_mut_ptr(),
        cb_nonce: u32::try_from(nonce.len()).ok()?,
        pb_auth_data: std::ptr::null_mut(),
        cb_auth_data: 0,
        pb_tag: tag.as_mut_ptr(),
        cb_tag: u32::try_from(tag.len()).ok()?,
        pb_mac_context: std::ptr::null_mut(),
        cb_mac_context: 0,
        cb_aad: 0,
        cb_data: 0,
        dw_flags: 0,
    };

    let mut plaintext = vec![0u8; ciphertext.len()];
    let mut written = 0u32;
    let status = unsafe {
        BCryptDecrypt(
            key_handle,
            ciphertext.as_ptr(),
            u32::try_from(ciphertext.len()).ok()?,
            &mode_info as *const AuthenticatedCipherModeInfo as *const c_void,
            std::ptr::null_mut(),
            0,
            plaintext.as_mut_ptr(),
            u32::try_from(plaintext.len()).ok()?,
            &mut written,
            0,
        )
    };

    if status != 0 {
        diagnose::log("Claude desktop token cache failed AES-GCM authentication");
        return None;
    }

    plaintext.truncate(written as usize);
    Some(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_regular_and_store_installs_in_stable_order() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let roaming = root.join("Roaming");
        let local = root.join("Local");
        let packages = local.join("Packages");
        for name in [
            "Claude_aaaaaaaaaaaaa",
            "Claude_pzs8sxrjxfjjc",
            "Claude_short",
            "Other_pzs8sxrjxfjjc",
        ] {
            std::fs::create_dir_all(packages.join(name)).unwrap();
        }
        // A file with a plausible package name is not an installation.
        std::fs::write(packages.join("Claude_bbbbbbbbbbbbb"), "").unwrap();
        assert_eq!(
            data_directories_in(Some(&roaming), Some(&local)),
            vec![
                roaming.join("Claude"),
                packages.join("Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude"),
                packages.join("Claude_aaaaaaaaaaaaa/LocalCache/Roaming/Claude"),
            ]
        );
        assert_eq!(
            data_directories_in(Some(&roaming), Some(&root.join("absent"))),
            vec![roaming.join("Claude")]
        );
        assert!(data_directories_in(None, None).is_empty());
    }

    #[test]
    fn discovers_migrated_data_before_legacy_installs() {
        let temporary = tempfile::tempdir().unwrap();
        let roaming = temporary.path().join("Roaming");
        let local = temporary.path().join("Local");
        let migrated = local.join("Claude-Data");
        for name in [
            "claude_zzzzzzzzzzzzz",
            "claude_aaaaaaaaaaaaa",
            "claude_pzs8sxrjxfjjc",
            "claude_short",
            "claude_aaaaaaaaaaaaaa",
            "claude_aaaaaaaaaaaaA",
            "claude_aaaaaaaaaaaa-",
            "claude_aaaaaaaaaaaaé",
            "Claude_ccccccccccccc",
            "other_pzs8sxrjxfjjc",
        ] {
            std::fs::create_dir_all(migrated.join(name)).unwrap();
        }
        std::fs::write(migrated.join("claude_bbbbbbbbbbbbb"), "").unwrap();
        let expected = vec![
            migrated.join("claude_pzs8sxrjxfjjc"),
            migrated.join("claude_aaaaaaaaaaaaa"),
            migrated.join("claude_zzzzzzzzzzzzz"),
        ];
        // Migrated-only installs do not require an old package directory or config.
        assert_eq!(data_directories_in(None, Some(&local)), expected);
        let legacy_store = local.join("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
        std::fs::create_dir_all(&legacy_store).unwrap();
        let expected: Vec<_> = expected
            .into_iter()
            .chain([roaming.join("Claude"), legacy_store])
            .collect();
        assert_eq!(data_directories_in(Some(&roaming), Some(&local)), expected);
        // Local State must be read alongside each discovered config.
        let config = expected[0].join("config.json");
        assert_eq!(local_state_path(&config), expected[0].join("Local State"));
    }

    #[test]
    fn usage_scopes_outrank_a_newer_inference_only_token() {
        let token = select_token(r#"{
            "install:user:https://api.anthropic.com:user:inference": {"token":"inference-only","expiresAt":9000000000000},
            "install:user:https://api.anthropic.com:user:profile user:inference": {"token":"usage","expiresAt":8000000000000},
            "install:user:https://api.anthropic.com:user:inference user:profile": {"token":"older-usage","expiresAt":7000000000000}
        }"#).unwrap();
        assert_eq!(token.access_token, "usage");
    }

    #[test]
    fn scope_matching_uses_whole_scopes_and_keeps_unknown_layout_fallback() {
        let token = select_token(r#"{
            "install:user:inference:https://api.anthropic.com:user:profile": {"token":"misleading-id","expiresAt":9000000000000},
            "install:user:https://api.anthropic.com:user:inference_extra user:profile": {"token":"scope-prefix","expiresAt":9000000000000},
            "install:user:https://api.anthropic.com:user:inference": {"token":"inference","expiresAt":8000000000000}
        }"#).unwrap();
        assert_eq!(token.access_token, "inference");
        assert_eq!(
            select_token(r#"{"future-layout":{"token":"fallback"}}"#)
                .unwrap()
                .access_token,
            "fallback"
        );
    }

    #[test]
    fn selects_the_freshest_inference_scoped_token() {
        let plaintext = r#"{
            "install:user:https://api.anthropic.com:user:profile": {
                "token": "profile-only",
                "expiresAt": 9000000000000
            },
            "install:user:https://api.anthropic.com:user:inference user:profile": {
                "token": "current",
                "expiresAt": 1818644595762
            },
            "install:old:https://api.anthropic.com:user:inference": {
                "token": "stale",
                "expiresAt": 1518644595762
            }
        }"#;

        let token = select_token(plaintext).expect("an inference token should be selected");
        assert_eq!(token.access_token, "current");
        assert_eq!(token.expires_at, Some(1818644595762));
    }

    #[test]
    fn stale_other_account_entries_lose_to_a_current_identity() {
        // The previous account's entry is expired; the current account holds
        // a usable token. Scope preference still applies within the current
        // set: the usage-scoped token wins over a fresher inference-only one.
        let token = select_token(
            r#"{
            "install:stale-uuid:https://api.anthropic.com:user:inference user:profile": {"token":"stale-other","expiresAt":1518644595762},
            "install:current-uuid:https://api.anthropic.com:user:inference": {"token":"current-inference","expiresAt":9000000000000},
            "install:current-uuid:https://api.anthropic.com:user:inference user:profile": {"token":"current-usage","expiresAt":8000000000000}
        }"#,
        )
        .expect("a current identity should be selected");
        assert_eq!(token.access_token, "current-usage");
    }

    #[test]
    fn all_expired_cache_still_reports_the_best_token() {
        // With no usable identity left, selection falls back to scope ranking
        // so the caller reports an expired login instead of no login.
        let token = select_token(
            r#"{
            "install:old-a:https://api.anthropic.com:user:inference": {"token":"old-inference","expiresAt":1518644595762},
            "install:old-b:https://api.anthropic.com:user:inference user:profile": {"token":"old-usage","expiresAt":1418644595762}
        }"#,
        )
        .expect("an expired cache still selects a token");
        assert_eq!(token.access_token, "old-usage");
    }
    #[test]
    fn bound_uuid_wins_over_better_scoped_fresher_other_uuid() {
        // Both entries are unexpired; the wrong account holds both scopes
        // with a far-future expiry. The bound current UUID must still win,
        // with scope preference applying only within it.
        let cache = r#"{
            "acct:wrong-uuid|current-uuid:client-id:https://api.anthropic.com:user:inference user:profile": {"token":"wrong-usage","expiresAt":9000000000000},
            "acct:current-uuid|other-org:client-id:https://api.anthropic.com:user:inference": {"token":"current-inference","expiresAt":8000000000000}
        }"#;
        let token = select_token_for(cache, Some("current-uuid"))
            .expect("the bound UUID must select its own entry");
        assert_eq!(token.access_token, "current-inference");
        // No entry for the bound UUID: refuse, never borrow another login.
        assert!(select_token_for(cache, Some("absent-uuid")).is_none());
        // A matching but expired entry still surfaces so the caller reports
        // the owning account's expiry instead of another account's token.
        let stale = r#"{
            "install:wrong-uuid:https://api.anthropic.com:user:inference user:profile": {"token":"wrong-usage","expiresAt":9000000000000},
            "install:current-uuid:https://api.anthropic.com:user:inference": {"token":"current-stale","expiresAt":1518644595762}
        }"#;
        let token = select_token_for(stale, Some("current-uuid"))
            .expect("the owning account's stale entry must surface");
        assert_eq!(token.access_token, "current-stale");
    }

    #[test]
    fn current_account_uuid_reads_the_config_binding() {
        assert_eq!(
            current_account_uuid(r#"{"lastKnownAccountUuid":"abc-123"}"#).as_deref(),
            Some("abc-123")
        );
        assert!(current_account_uuid(r#"{"locale":"en-US"}"#).is_none());
        assert!(current_account_uuid(r#"{"lastKnownAccountUuid":""}"#).is_none());
        assert!(current_account_uuid(r#"{"lastKnownAccountUuid":"   "}"#).is_none());
        assert!(current_account_uuid("not json").is_none());
    }

    #[test]

    fn ignores_entries_without_a_usable_token() {
        assert!(select_token(r#"{"install:user:scope": {"expiresAt": 1}}"#).is_none());
        assert!(select_token(r#"{"install:user:scope": {"token": ""}}"#).is_none());
        assert!(select_token("not json").is_none());
    }

    #[test]
    fn reads_the_token_cache_out_of_a_desktop_config() {
        let config = r#"{"locale": "en-US", "oauth:tokenCache": "djEwYWJj"}"#;
        assert_eq!(
            token_cache_values(config),
            vec![("oauth:tokenCache", "djEwYWJj".to_string())]
        );
        assert!(token_cache_values(r#"{"locale": "en-US"}"#).is_empty());
        assert!(token_cache_values("not json").is_empty());
    }

    #[test]
    fn prefers_the_v2_cache_but_keeps_the_legacy_one_as_a_fallback() {
        let config = r#"{"oauth:tokenCache": "djEwb2xk", "oauth:tokenCacheV2": "djEwbmV3"}"#;
        assert_eq!(
            token_cache_values(config),
            vec![
                ("oauth:tokenCacheV2", "djEwbmV3".to_string()),
                ("oauth:tokenCache", "djEwb2xk".to_string()),
            ]
        );
    }

    #[test]
    fn ignores_emptied_token_caches() {
        // The desktop app leaves the key in place with an empty value after a
        // migration; that must not mask a populated cache under the other key.
        let config = r#"{"oauth:tokenCacheV2": "", "oauth:tokenCache": "djEwb2xk"}"#;
        assert_eq!(
            token_cache_values(config),
            vec![("oauth:tokenCache", "djEwb2xk".to_string())]
        );
    }

    #[test]
    fn rejects_blobs_that_are_not_os_crypt_v10() {
        // Valid base64, but the version prefix is not "v10".
        assert!(decrypt_os_crypt_value("bm90LXYxMC1kYXRh", &[0u8; 32]).is_none());
        // Right prefix, too short to hold a nonce and a tag.
        assert!(decrypt_os_crypt_value("djEwc2hvcnQ", &[0u8; 32]).is_none());
        assert!(decrypt_os_crypt_value("!!!", &[0u8; 32]).is_none());
    }

    #[test]
    fn decodes_standard_base64_with_and_without_padding() {
        assert_eq!(base64_decode("djEw").unwrap(), b"v10");
        assert_eq!(base64_decode("YWJjZA==").unwrap(), b"abcd");
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a-b_").is_none());
    }

    /// Ignored by default: this one proves the real DPAPI + AES-GCM path
    /// against whatever the Claude desktop app has on the current machine.
    /// Run it with `cargo test -- --ignored` while signed in to the app.
    #[test]
    #[ignore = "requires a signed-in Claude desktop app on this machine"]
    fn reads_a_token_from_the_installed_desktop_app() {
        let token = config_paths()
            .iter()
            .find_map(|path| read_token(path))
            .expect("the desktop app should expose a token");
        assert!(token.access_token.starts_with("sk-ant-"));
        assert!(token.expires_at.unwrap_or_default() > 0);
    }

    #[test]
    fn watch_signature_reports_missing_config() {
        let signature = watch_signature(Path::new("C:/nonexistent/Claude/config.json"));
        assert!(signature.ends_with("|missing|uuid:missing"), "{signature}");
    }
}
