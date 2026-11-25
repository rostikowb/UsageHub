//! Read-only access to omp's own provider sign-in store.
//!
//! omp keeps OAuth credentials in `~/.omp/agent/agent.db` (SQLite table
//! `auth_credentials`). Reusing an existing ChatGPT sign-in avoids a second
//! browser authorization. This module only reads: UsageHub never refreshes or
//! rewrites omp's tokens, so omp stays their owner and is not signed out.

use std::path::PathBuf;

use serde::Deserialize;

use crate::manager::ManagerResult;

/// Absolute override for the omp database (tests and portable setups).
pub const OMP_DB_ENV: &str = "USAGEHUB_OMP_DB";

/// Normalize an omp/JWT expiration timestamp (seconds or milliseconds) to Unix seconds.
pub fn normalize_expires_unix(expires: u64) -> u64 {
    if expires > 10_000_000_000 {
        expires / 1000
    } else {
        expires
    }
}

/// One ChatGPT sign-in recorded by omp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OmpCodexCredential {
    pub email: String,
    pub account_id: Option<String>,
    pub access: String,
    pub expires_unix: Option<u64>,
}

impl OmpCodexCredential {
    /// Expiration normalized to Unix seconds, if known.
    pub fn expires_unix_secs(&self) -> Option<u64> {
        self.expires_unix.map(normalize_expires_unix)
    }

    /// True while the access token has not expired. Refreshing is never done
    /// here: omp owns the rotating token.
    pub fn usable_at(&self, now_unix: u64) -> bool {
        self.expires_unix_secs()
            .is_none_or(|expires_sec| expires_sec > now_unix)
            && !self.access.trim().is_empty()
    }
}

/// Cached non-secret metadata extracted from a Codex profile's `auth.json`
/// off the UI thread.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OmpAuthMetadata {
    pub email: Option<String>,
    pub expires_unix: Option<u64>,
    pub is_omp_imported: bool,
}

/// Extract the `exp` claim (Unix seconds) from a JWT access token payload, if present.
pub fn jwt_exp_unix(access_token: &str) -> Option<u64> {
    let mut parts = access_token.trim().split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _signature = parts.next()?;
    if parts.next().is_some() || payload.is_empty() {
        return None;
    }
    let mut standard = String::with_capacity(payload.len() + 3);
    for ch in payload.chars() {
        standard.push(match ch {
            '-' => '+',
            '_' => '/',
            other => other,
        });
    }
    while standard.len() % 4 != 0 {
        standard.push('=');
    }
    let bytes = crate::manager::credentials::base64_decode(&standard).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("exp")
        .and_then(|v| v.as_u64())
        .map(normalize_expires_unix)
}

/// Read omp import metadata (`usagehub_omp_email`, `usagehub_omp_expires_unix`,
/// or JWT `exp` fallback) from a Codex `auth.json` file. Call from worker
/// threads only, never inside immediate-mode UI rendering.
pub fn read_auth_file_omp_metadata(auth_path: &std::path::Path) -> OmpAuthMetadata {
    let Ok(content) = std::fs::read_to_string(auth_path) else {
        return OmpAuthMetadata::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return OmpAuthMetadata::default();
    };
    let is_omp_imported = value.get("usagehub_omp_email").is_some();
    let email = value
        .get("usagehub_omp_email")
        .and_then(|e| e.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let expires_unix = value
        .get("usagehub_omp_expires_unix")
        .and_then(|e| e.as_u64())
        .map(normalize_expires_unix)
        .or_else(|| {
            if is_omp_imported {
                value
                    .get("tokens")
                    .and_then(|tokens| tokens.get("access_token"))
                    .and_then(|token| token.as_str())
                    .and_then(jwt_exp_unix)
            } else {
                None
            }
        });
    OmpAuthMetadata {
        email,
        expires_unix,
        is_omp_imported,
    }
}

/// Stable identity used to avoid re-importing the same omp account.
pub fn discovery_key(email: &str) -> String {
    format!("omp:{}", email.trim().to_ascii_lowercase())
}

#[derive(Deserialize)]
struct RawCredential {
    access: String,
    #[serde(default)]
    expires: Option<u64>,
    #[serde(rename = "accountId", default)]
    account_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

/// omp's database location: `%USERPROFILE%\.omp\agent\agent.db` by default.
pub fn database_path() -> ManagerResult<PathBuf> {
    if let Some(path) = std::env::var_os(OMP_DB_ENV).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!("{OMP_DB_ENV} must be an absolute path"));
        }
        return Ok(path);
    }
    let home = crate::known_folders::home_dir()
        .ok_or_else(|| "Unable to resolve the user profile directory".to_string())?;
    Ok(home.join(".omp").join("agent").join("agent.db"))
}

/// Every ChatGPT sign-in omp knows about, most recently updated first.
pub fn codex_credentials() -> ManagerResult<Vec<OmpCodexCredential>> {
    let path = database_path()?;
    if !path.is_file() {
        return Err(format!(
            "omp sign-in database not found at {}",
            path.display()
        ));
    }
    let rows = crate::winsqlite::query_text_values(
        &path,
        "SELECT data FROM auth_credentials \
         WHERE provider = 'openai-codex' AND disabled_cause IS NULL \
         ORDER BY updated_at DESC, id DESC",
    )
    .map_err(|error| format!("Unable to read omp credentials: {error}"))?;
    let credentials = rows
        .iter()
        .filter_map(|row| serde_json::from_str::<RawCredential>(row).ok())
        .filter(|raw| !raw.access.trim().is_empty())
        .map(|raw| OmpCodexCredential {
            email: raw.email.unwrap_or_default(),
            account_id: raw.account_id,
            access: raw.access,
            expires_unix: raw.expires,
        })
        .collect();
    Ok(credentials)
}

/// Pick one credential: an exact/containing email wins, otherwise the most
/// recently updated entry. Case-insensitive so a typed mail works as expected.
pub fn select_credential<'a>(
    credentials: &'a [OmpCodexCredential],
    email: Option<&str>,
) -> ManagerResult<&'a OmpCodexCredential> {
    if credentials.is_empty() {
        return Err("omp has no ChatGPT sign-in stored; sign in once in omp".to_string());
    }
    let Some(email) = email.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(&credentials[0]);
    };
    let needle = email.to_ascii_lowercase();
    credentials
        .iter()
        .find(|credential| credential.email.eq_ignore_ascii_case(&needle))
        .or_else(|| {
            credentials
                .iter()
                .find(|credential| credential.email.to_ascii_lowercase().contains(&needle))
        })
        .ok_or_else(|| {
            let known: Vec<&str> = credentials
                .iter()
                .map(|credential| credential.email.as_str())
                .collect();
            format!(
                "No omp ChatGPT sign-in matches '{email}'; known: {}",
                known.join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(email: &str, expires_unix: Option<u64>) -> OmpCodexCredential {
        OmpCodexCredential {
            email: email.into(),
            account_id: Some(format!("{email}-account")),
            access: "token".into(),
            expires_unix,
        }
    }

    #[test]
    fn selection_prefers_an_email_match_and_defaults_to_the_newest() {
        let credentials = vec![
            credential("new@example.com", Some(200)),
            credential("old@example.com", Some(200)),
        ];
        assert_eq!(
            select_credential(&credentials, None).unwrap().email,
            "new@example.com"
        );
        assert_eq!(
            select_credential(&credentials, Some("OLD@example.com"))
                .unwrap()
                .email,
            "old@example.com"
        );
        assert_eq!(
            select_credential(&credentials, Some("old@")).unwrap().email,
            "old@example.com"
        );
        assert!(select_credential(&credentials, Some("missing@example.com")).is_err());
        assert!(select_credential(&[], None).is_err());
    }

    #[test]
    fn expired_or_empty_access_tokens_are_not_usable() {
        assert!(credential("a@example.com", Some(100)).usable_at(99));
        assert!(!credential("a@example.com", Some(100)).usable_at(100));
        assert!(credential("a@example.com", Some(100_000)).usable_at(99));
        assert!(!credential("a@example.com", Some(100_000_000_000)).usable_at(100_000_001));
        assert!(credential("a@example.com", Some(100_000_000_000)).usable_at(99_999_999));
        assert!(credential("a@example.com", None).usable_at(u64::MAX));
        let mut blank = credential("a@example.com", None);
        blank.access = "  ".into();
        assert!(!blank.usable_at(0));
    }

    #[test]
    fn single_credential_rejects_mismatched_email_hint() {
        let credentials = vec![credential("only@example.com", Some(200))];
        assert_eq!(
            select_credential(&credentials, None).unwrap().email,
            "only@example.com"
        );
        assert!(select_credential(&credentials, Some("nonmatching")).is_err());
    }

    #[test]
    fn discovery_keys_are_case_insensitive_and_stable() {
        assert_eq!(discovery_key(" A@Example.com "), "omp:a@example.com");
    }

    #[test]
    fn jwt_exp_unix_extracts_normalized_exp_claim() {
        // {"alg":"none"}.{"exp":1800000000}.sig (base64url without padding)
        let payload = "eyJleHAiOjE4MDAwMDAwMDB9";
        let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        assert_eq!(jwt_exp_unix(&token), Some(1_800_000_000));

        // Millisecond exp gets normalized to seconds.
        let ms_payload = "eyJleHAiOjE4MDAwMDAwMDAwMDB9";
        let ms_token = format!("hdr.{ms_payload}.sig");
        assert_eq!(jwt_exp_unix(&ms_token), Some(1_800_000_000));

        assert_eq!(jwt_exp_unix("not-a-jwt"), None);
        assert_eq!(jwt_exp_unix("a.!!!.c"), None);
    }

    #[test]
    fn read_auth_file_omp_metadata_handles_explicit_expiry_and_jwt_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "hdr.eyJleHAiOjE5MDAwMDAwMDB9.sig" },
                "usagehub_omp_email": " user@example.com ",
                "usagehub_omp_expires_unix": 1750000000000
            }"#,
        )
        .unwrap();
        let meta = read_auth_file_omp_metadata(&path);
        assert!(meta.is_omp_imported);
        assert_eq!(meta.email.as_deref(), Some("user@example.com"));
        assert_eq!(meta.expires_unix, Some(1_750_000_000));

        // Fallback to JWT exp when usagehub_omp_expires_unix is absent.
        std::fs::write(
            &path,
            r#"{
                "auth_mode": "chatgpt",
                "tokens": { "access_token": "hdr.eyJleHAiOjE5MDAwMDAwMDB9.sig" },
                "usagehub_omp_email": "user@example.com"
            }"#,
        )
        .unwrap();
        let fallback = read_auth_file_omp_metadata(&path);
        assert!(fallback.is_omp_imported);
        assert_eq!(fallback.expires_unix, Some(1_900_000_000));

        // Non-omp auth file does not report as omp-imported.
        std::fs::write(
            &path,
            r#"{"auth_mode": "chatgpt", "tokens": { "access_token": "hdr.eyJleHAiOjE5MDAwMDAwMDB9.sig" }}"#,
        )
        .unwrap();
        let own = read_auth_file_omp_metadata(&path);
        assert!(!own.is_omp_imported);
        assert_eq!(own.email, None);
        assert_eq!(own.expires_unix, None);
    }
}
