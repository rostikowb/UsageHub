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

/// One ChatGPT sign-in recorded by omp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OmpCodexCredential {
    pub email: String,
    pub account_id: Option<String>,
    pub access: String,
    pub expires_unix: Option<u64>,
}

impl OmpCodexCredential {
    /// True while the access token has not expired. Refreshing is never done
    /// here: omp owns the rotating token.
    pub fn usable_at(&self, now_unix: u64) -> bool {
        self.expires_unix.is_none_or(|expires| expires > now_unix) && !self.access.trim().is_empty()
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
        assert!(credential("a@example.com", None).usable_at(u64::MAX));
        let mut blank = credential("a@example.com", None);
        blank.access = "  ".into();
        assert!(!blank.usable_at(0));
    }

    #[test]
    fn discovery_keys_are_case_insensitive_and_stable() {
        assert_eq!(discovery_key(" A@Example.com "), "omp:a@example.com");
    }
}
