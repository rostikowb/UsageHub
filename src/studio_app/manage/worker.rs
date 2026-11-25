//! Worker-thread execution for `--manage` operations, in-process read-only
//! dispatch, and background `omp` auth metadata caching.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::{ManageState, ManageValue, OpKind};
use crate::accounts::AccountProfile;
use crate::providers::ProviderId;
use crate::studio_app::StudioApp;

/// Max captured bytes per `--manage` backend stream (success JSON rides on
/// stdout, the failure envelope on stderr).
const MANAGE_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Ordinary backend queries (open/status/stop/poll/...) must answer quickly.
const MANAGE_ORDINARY_TIMEOUT: Duration = Duration::from_secs(60);
/// Backup/cleanup/migration/skills walk user data and may take much longer.
const MANAGE_DATA_TIMEOUT: Duration = Duration::from_secs(900);

pub(super) struct ManageJob {
    pub(super) kind: OpKind,
    pub(super) rx: mpsc::Receiver<ManageDone>,
    pub(super) started: Instant,
    pub(super) deadline: Duration,
    /// JSON-lines phase file for cleanup preview/apply progress.
    pub(super) progress: Option<std::path::PathBuf>,
}

pub(super) struct ManageDone {
    pub(super) result: Result<ManageValue, String>,
}

/// Non-secret omp import metadata cached off the UI thread per Codex profile ID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct OmpProfileStatus {
    pub(super) email: Option<String>,
    pub(super) expires_unix: Option<u64>,
    pub(super) is_omp_imported: bool,
}

/// Parse one `--manage` stdout envelope into its data payload.
pub(super) fn parse_manage_output(stdout: &str) -> Result<ManageValue, String> {
    let value: ManageValue = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("Manager returned non-JSON output: {error}"))?;
    if value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false) {
        return Ok(value.get("data").cloned().unwrap_or(ManageValue::Null));
    }
    let error = value.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_str())
        .unwrap_or("operation_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .unwrap_or("Unknown manager error");
    Err(format!("{code}: {message}"))
}

/// Backend deadline for one `--manage` argv: data operations get 900s,
/// ordinary queries 60s. Pure argv inspection, no process spawn.
pub(super) fn manage_timeout_for(argv: &[String]) -> Duration {
    match argv.first().map(String::as_str) {
        Some("backup" | "cleanup" | "skills" | "migration") => MANAGE_DATA_TIMEOUT,
        _ => MANAGE_ORDINARY_TIMEOUT,
    }
}

/// Split `--manage` process output into its data payload. Success JSON rides
/// on stdout; failures carry the single JSON envelope on stderr (exit 2/3),
/// parsed with the same shape so code/message stay exact. Non-JSON stderr is
/// an OS-level failure and surfaces raw.
pub(super) fn manage_output_result(
    stdout: &str,
    stderr: &str,
    code: Option<i32>,
) -> Result<ManageValue, String> {
    if !stdout.trim().is_empty() {
        return parse_manage_output(stdout);
    }
    let trimmed = stderr.trim();
    if !trimmed.is_empty() {
        if serde_json::from_str::<ManageValue>(trimmed).is_ok() {
            return parse_manage_output(trimmed);
        }
        return Err(format!(
            "Manager failed: {}",
            trimmed.chars().take(300).collect::<String>()
        ));
    }
    Err(format!(
        "Manager produced no output: exit code {}",
        code.unwrap_or(-1)
    ))
}

/// Parse simple `--flag value` / `--flag=value` pairs for safe in-process
/// read-only dispatch. Returns `None` if an unrecognized token is present so
/// the caller falls back to the `--manage` subprocess.
fn parse_read_only_flags(
    tokens: &[String],
    allowed_value_flags: &[&str],
    allowed_bool_flags: &[&str],
) -> Option<(HashMap<String, String>, Vec<String>)> {
    let mut values = HashMap::new();
    let mut bools = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if allowed_bool_flags.contains(&token.as_str()) {
            bools.push(token.clone());
            index += 1;
            continue;
        }
        if let Some((flag, val)) = token.split_once('=') {
            if !allowed_value_flags.contains(&flag) || val.is_empty() {
                return None;
            }
            values.insert(flag.to_string(), val.to_string());
            index += 1;
            continue;
        }
        if allowed_value_flags.contains(&token.as_str()) {
            let val = tokens.get(index + 1)?;
            if val.starts_with("--") {
                return None;
            }
            values.insert(token.clone(), val.clone());
            index += 2;
            continue;
        }
        return None;
    }
    Some((values, bools))
}

fn parse_provider_arg(text: &str) -> Result<ProviderId, String> {
    match text.to_ascii_lowercase().as_str() {
        "claude" => Ok(ProviderId::Claude),
        "codex" => Ok(ProviderId::Codex),
        other => Err(format!(
            "usage_error: unknown provider '{other}'; expected claude or codex"
        )),
    }
}

fn op_err(error: String) -> String {
    format!("operation_error: {error}")
}

/// Safe read-only in-process dispatch for local queries (`status`,
/// read-only `discover`, `backup list`, `migration preview`, `signin --list-omp`).
/// Returns `None` for mutations or unrecognized arguments so the caller runs
/// the authoritative `usagehub.exe --manage` subprocess instead.
pub(super) fn try_run_read_only_in_process(argv: &[String]) -> Option<Result<ManageValue, String>> {
    let command = argv.first().map(String::as_str)?;
    let rest = &argv[1..];
    match command {
        "status" => {
            let (flags, _) = parse_read_only_flags(rest, &["--provider", "--id"], &[])?;
            let provider_str = flags.get("--provider")?;
            let id = flags.get("--id")?.clone();
            Some((|| {
                crate::manager::paths::validate_root_overrides()
                    .map_err(|e| format!("usage_error: {e}"))?;
                let provider = parse_provider_arg(provider_str)?;
                let report =
                    crate::manager::lifecycle::account_status(provider, &id).map_err(op_err)?;
                Ok(json!({
                    "provider": provider.descriptor().key,
                    "account_id": id,
                    "status": report,
                }))
            })())
        }
        "discover" => {
            if !rest.is_empty() {
                // `discover --apply` mutates settings; keep it on the subprocess path.
                return None;
            }
            Some((|| {
                crate::manager::paths::validate_root_overrides()
                    .map_err(|e| format!("usage_error: {e}"))?;
                let report = crate::manager::discovery::run(false).map_err(op_err)?;
                serde_json::to_value(report)
                    .map_err(|e| op_err(format!("Unable to encode discovery: {e}")))
            })())
        }
        "backup" if rest.first().map(String::as_str) == Some("list") => {
            let (flags, _) = parse_read_only_flags(&rest[1..], &["--provider", "--id"], &[])?;
            Some((|| {
                crate::manager::paths::validate_root_overrides()
                    .map_err(|e| format!("usage_error: {e}"))?;
                let filter = flags
                    .get("--provider")
                    .map(|p| parse_provider_arg(p))
                    .transpose()?;
                let id_filter = flags.get("--id").map(String::as_str);
                let mut infos = crate::manager::backup::list_all_backups().map_err(op_err)?;
                infos.retain(|info| {
                    filter.is_none_or(|provider| info.provider == provider)
                        && id_filter.is_none_or(|id| info.account_id == id)
                });
                Ok(json!({ "backups": infos }))
            })())
        }
        "migration" if rest.first().map(String::as_str) == Some("preview") && rest.len() == 1 => {
            Some((|| {
                crate::manager::paths::validate_root_overrides()
                    .map_err(|e| format!("usage_error: {e}"))?;
                crate::manager::migration::preview().map_err(op_err)
            })())
        }
        "signin" => {
            let (flags, bools) = parse_read_only_flags(rest, &["--provider"], &["--list-omp"])?;
            if !bools.iter().any(|b| b == "--list-omp") {
                return None;
            }
            let provider_str = flags.get("--provider")?;
            Some((|| {
                crate::manager::paths::validate_root_overrides()
                    .map_err(|e| format!("usage_error: {e}"))?;
                let _provider = parse_provider_arg(provider_str)?;
                let credentials = crate::manager::omp_auth::codex_credentials().map_err(op_err)?;
                Ok(json!({
                    "accounts": credentials
                        .iter()
                        .map(|credential| json!({
                            "email": credential.email,
                            "account_id": credential.account_id,
                            "expires_unix": credential.expires_unix,
                        }))
                        .collect::<Vec<_>>(),
                }))
            })())
        }
        _ => None,
    }
}

/// Run `current_exe --manage <argv>` synchronously; worker threads only.
/// The caller passes `manage_timeout_for(argv)`; the job tree is killed on
/// timeout so the UI worker can never block past the deadline.
fn run_manage_blocking(argv: &[String], timeout: Duration) -> Result<ManageValue, String> {
    if let Some(result) = try_run_read_only_in_process(argv) {
        return result;
    }

    use crate::manager::process::{run_bounded, ConsoleError};

    let executable = std::env::current_exe()
        .map_err(|error| format!("Unable to locate the application: {error}"))?;
    let mut command = std::process::Command::new(executable);
    command.arg("--manage");
    for argument in argv {
        command.arg(argument);
    }
    // run_bounded owns stdio pipes, the hidden console, the 8MiB cap and
    // the kill-on-timeout job tree. No argv in errors (paths may be private).
    let output =
        run_bounded(&mut command, timeout, MANAGE_CAPTURE_BYTES, "--manage").map_err(|error| {
            match error {
                ConsoleError::TimedOut { timeout, .. } => {
                    format!("Manager timed out after {}s", timeout.as_secs())
                }
                ConsoleError::Failed { detail, .. } => detail,
            }
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    manage_output_result(&stdout, &stderr, output.status.code())
}

pub(super) fn load_omp_statuses_from_profiles(
    profiles: &[AccountProfile],
) -> HashMap<String, OmpProfileStatus> {
    let mut out = HashMap::new();
    for profile in profiles {
        let root = if !profile.config_dir.trim().is_empty() {
            Some(std::path::PathBuf::from(profile.config_dir.trim()))
        } else if !profile.desktop_dir.trim().is_empty() {
            Some(std::path::PathBuf::from(profile.desktop_dir.trim()))
        } else {
            None
        };
        let Some(root) = root else { continue };
        let meta = crate::manager::omp_auth::read_auth_file_omp_metadata(&root.join("auth.json"));
        if meta.is_omp_imported || meta.email.is_some() || meta.expires_unix.is_some() {
            out.insert(
                profile.id.clone(),
                OmpProfileStatus {
                    email: meta.email,
                    expires_unix: meta.expires_unix,
                    is_omp_imported: meta.is_omp_imported,
                },
            );
        }
    }
    out
}

pub(super) fn ensure_omp_cache_refresh(manage: &mut ManageState, profiles: &[AccountProfile]) {
    if manage.omp_cache_loaded || manage.omp_refresh_rx.is_some() {
        return;
    }
    let profiles = profiles.to_vec();
    let (tx, rx) = mpsc::channel();
    manage.omp_refresh_rx = Some(rx);
    std::thread::spawn(move || {
        let map = load_omp_statuses_from_profiles(&profiles);
        let _ = tx.send(map);
    });
}

impl StudioApp {
    pub(super) fn manage_busy(&self) -> bool {
        self.manage.job.is_some()
    }

    pub(super) fn spawn_manage(&mut self, kind: OpKind, argv: Vec<String>) {
        let language = self.language();
        if self.manage.job.is_some() {
            self.manage.notice = Some(language.text("Operation running...").into());
            return;
        }
        let timeout = manage_timeout_for(&argv);
        let mut argv = argv;
        let progress = if matches!(kind, OpKind::CleanupPreview { .. } | OpKind::CleanupApply) {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "usagehub-cleanup-progress-{}-{}.jsonl",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_file(&path);
            argv.push("--progress-file".into());
            argv.push(path.to_string_lossy().to_string());
            Some(path)
        } else {
            None
        };
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = run_manage_blocking(&argv, timeout);
            let _ = tx.send(ManageDone { result });
        });
        self.manage.job = Some(ManageJob {
            kind,
            rx,
            started: Instant::now(),
            deadline: timeout,
            progress,
        });
        self.manage.error = None;
    }
}
