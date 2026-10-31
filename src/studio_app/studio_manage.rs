//! Account manager: native cards, cleanup, backups, skills and migration.
//!
//! Every mutation shells out to this same executable's `--manage` JSON surface
//! on a worker thread (egui never blocks). On completion the fresh settings
//! registry is reloaded, usage is refreshed and the monitor is notified; the
//! UI never writes managed registry state through a stale normal save.
//!
//! Displayed limits and resets come from the live usage cache (the original
//! server poller only). The CLI `poll`/`snapshot` outputs are shown as
//! status/provenance, never as estimates, and unknown/expired resets stay
//! explicit. Destructive actions confirm the exact backend-derived target
//! list of one account operation; there are no per-target checkboxes because
//! the backend does not filter targets.

use super::*;

/// Latest `--manage` JSON payload by operation.
use crate::providers::ProviderId;
pub(super) type ManageValue = serde_json::Value;

/// Cleanup scopes, mirroring the fixed `--manage cleanup` spellings
/// (`cache` | `logout` | `delete` | `reset`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CleanupScopeSel {
    Cache,
    Logout,
    LocalData,
    FullReset,
}

impl CleanupScopeSel {
    fn cli_name(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Logout => "logout",
            Self::LocalData => "delete",
            Self::FullReset => "reset",
        }
    }

    fn label(self, language: LanguageId) -> &'static str {
        match self {
            Self::Cache => language.text("Clean cached files"),
            Self::Logout => language.text("Sign out, keep chats"),
            Self::LocalData => language.text("Delete profile"),
            Self::FullReset => language.text("Reset for a new sign-in"),
        }
    }
}

/// Legacy-backup disposition for `migration legacy-backup --action`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LegacyAction {
    Encrypt,
    Purge,
    Leave,
}

impl LegacyAction {
    fn cli_name(self) -> &'static str {
        match self {
            Self::Encrypt => "encrypt",
            Self::Purge => "purge",
            Self::Leave => "leave",
        }
    }
}

/// Identity of the exact reviewed deletion: provider, account, scope and the
/// external-Default opt-in. Preview results and Apply both bind to this key;
/// any selector change invalidates the review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CleanupPreviewKey {
    provider: String,
    account: String,
    scope: CleanupScopeSel,
    include_external: bool,
}
/// What a finished worker-thread job carries back to the UI thread.
#[derive(Clone, Debug)]
pub(super) enum OpKind {
    /// Mutation whose only visible outcome is fresh registry/usage.
    Simple,
    /// `status` output rendered into the card status line.
    Status {
        key: String,
    },
    CleanupPreview {
        key: CleanupPreviewKey,
    },
    CleanupApply,
    /// Identity (provider tab + account filter) the backup list was requested
    /// for; a late result for a superseded filter is discarded.
    BackupList {
        provider: String,
        account: String,
    },
    /// Backup verify/create/restore/purge; completion refreshes whatever
    /// filter is selected when the worker lands.
    BackupOp,
    SkillsImport,
    Migration,
}

struct ManageJob {
    kind: OpKind,
    rx: std::sync::mpsc::Receiver<ManageDone>,
    started: Instant,
    deadline: Duration,
    /// JSON-lines phase file for cleanup preview/apply progress.
    progress: Option<std::path::PathBuf>,
}

struct ManageDone {
    result: Result<ManageValue, String>,
}

pub(super) struct ManageState {
    pub provider_tab: ProviderId,
    job: Option<ManageJob>,
    error: Option<String>,
    notice: Option<String>,
    new_name: String,
    rename: HashMap<String, String>,
    import_path: String,
    import_name: String,
    color_drafts: HashMap<String, String>,
    status: HashMap<String, String>,
    cleanup_account: String,
    cleanup_provider: ProviderId,
    cleanup_name: String,
    cleanup_scope: CleanupScopeSel,
    navigate_cleanup: bool,
    cleanup_preview: Option<ManageValue>,
    cleanup_preview_key: Option<CleanupPreviewKey>,
    cleanup_confirm: String,
    include_external_default: bool,
    no_backup_ack: bool,
    error_from_preview: bool,
    /// Cleanup dialog step: 0 = choose action, 1 = review/apply.
    cleanup_step: u8,
    skip_backup: bool,
    cleanup_result: Option<ManageValue>,
    confirm_force_stop: Option<String>,
    confirm_purge: Option<String>,
    confirm_restore: Option<String>,
    backups: Option<ManageValue>,
    /// Provider tab + account filter that produced `backups`; the inventory
    /// is hidden and retired whenever the visible filter differs.
    backup_list_filter: Option<(String, String)>,
    backup_account: String,
    skills_path: String,
    skills_replace: bool,
    skills_preview: Option<ManageValue>,
    migration_preview: Option<ManageValue>,
    legacy_path: String,
    legacy_action: LegacyAction,
    legacy_confirm: String,
    pub(super) last_snapshot_unix: Option<u64>,
}

impl Default for ManageState {
    fn default() -> Self {
        Self {
            provider_tab: ProviderId::Claude,
            job: None,
            error: None,
            notice: None,
            new_name: String::new(),
            rename: HashMap::new(),
            import_path: String::new(),
            import_name: String::new(),
            color_drafts: HashMap::new(),
            status: HashMap::new(),
            cleanup_account: String::new(),
            cleanup_provider: ProviderId::Claude,
            cleanup_name: String::new(),
            cleanup_scope: CleanupScopeSel::Cache,
            navigate_cleanup: false,
            cleanup_preview: None,
            cleanup_preview_key: None,
            cleanup_confirm: String::new(),
            include_external_default: false,
            no_backup_ack: false,
            error_from_preview: false,
            cleanup_step: 0,
            skip_backup: false,
            cleanup_result: None,
            confirm_force_stop: None,
            confirm_purge: None,
            confirm_restore: None,
            backups: None,
            backup_list_filter: None,
            backup_account: String::new(),
            skills_path: String::new(),
            skills_replace: false,
            skills_preview: None,
            migration_preview: None,
            legacy_path: String::new(),
            legacy_action: LegacyAction::Encrypt,
            legacy_confirm: String::new(),
            last_snapshot_unix: None,
        }
    }
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

/// Max captured bytes per `--manage` backend stream (success JSON rides on
/// stdout, the failure envelope on stderr).
const MANAGE_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// Ordinary backend queries (open/status/stop/poll/...) must answer quickly.
const MANAGE_ORDINARY_TIMEOUT: Duration = Duration::from_secs(60);
/// Backup/cleanup/migration/skills walk user data and may take much longer.
const MANAGE_DATA_TIMEOUT: Duration = Duration::from_secs(900);

/// Backend deadline for one `--manage` argv: data operations get 900s,
/// ordinary queries 60s. Pure argv inspection, no process spawn.
pub(super) fn manage_timeout_for(argv: &[String]) -> Duration {
    match argv.first().map(String::as_str) {
        Some("backup" | "cleanup" | "skills" | "migration") => MANAGE_DATA_TIMEOUT,
        _ => MANAGE_ORDINARY_TIMEOUT,
    }
}

/// Run `current_exe --manage <argv>` synchronously; worker threads only.
/// The caller passes `manage_timeout_for(argv)`; the job tree is killed on
/// timeout so the UI worker can never block past the deadline.
fn run_manage_blocking(argv: &[String], timeout: Duration) -> Result<ManageValue, String> {
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

/// Split `--manage` process output into its data payload. Success JSON rides
/// on stdout; failures carry the single JSON envelope on stderr (exit 2/3),
/// parsed with the same shape so code/message stay exact. Non-JSON stderr is
/// an OS-level failure and surfaces raw.
fn manage_output_result(
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

/// Absolute UTC timestamp (`YYYY-MM-DD HH:MM UTC`) for server reset dates.
pub(super) fn format_unix_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3_600,
        rem % 3_600 / 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Short countdown for the authoritative reset path (never an estimate).
pub(super) fn format_countdown(language: LanguageId, duration: Duration) -> String {
    let suffix = language.strings();
    let secs = duration.as_secs();
    let days = secs / 86_400;
    let hours = secs % 86_400 / 3_600;
    let minutes = secs % 3_600 / 60;
    let seconds = secs % 60;
    if days > 0 {
        format!(
            "{days}{}{hours:02}{}{minutes:02}{}",
            suffix.day_suffix, suffix.hour_suffix, suffix.minute_suffix
        )
    } else if hours > 0 {
        format!(
            "{hours}{}{minutes:02}{}",
            suffix.hour_suffix, suffix.minute_suffix
        )
    } else if minutes > 0 {
        format!(
            "{minutes}{}{seconds:02}{}",
            suffix.minute_suffix, suffix.second_suffix
        )
    } else {
        format!("{seconds}{}", suffix.second_suffix)
    }
}

/// Authoritative reset line for one usage window: absolute server date plus
/// countdown, or explicit unknown/expired. `None` when the provider did not
/// report the window at all.
pub(super) fn reset_display(
    language: LanguageId,
    label: &str,
    section: &crate::models::UsageSection,
    now: SystemTime,
) -> Option<String> {
    if !section.available {
        return None;
    }
    let head = format!("{label}: {:.0}%", section.percentage);
    match section.resets_at {
        None => Some(format!("{head} · {}", language.text("Unknown reset"))),
        Some(resets_at) => match resets_at.duration_since(now) {
            Err(_) => Some(format!(
                "{head} · {}",
                language.text("Expired; waiting for next poll")
            )),
            Ok(left) => {
                let absolute = format_unix_utc(
                    resets_at
                        .duration_since(UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_secs())
                        .unwrap_or(0),
                );
                Some(format!(
                    "{head} · {} {absolute} ({})",
                    language.text("Resets at"),
                    format_countdown(language, left)
                ))
            }
        },
    }
}

/// Destructive-apply gate: the exact typed account id. Backup coverage is the
/// backend's job (automatic 7-day snapshot after stop, except cache scope or
/// an explicit no-backup); metadata-only registry deletes with no asset
/// targets still require this confirmation. The backend revalidates
/// independently; this only drives the button.
/// Last phase written by a running cleanup preview/apply.
fn cleanup_progress_phase(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let line = content.lines().rev().find(|line| !line.trim().is_empty())?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    value
        .get("phase")
        .and_then(|phase| phase.as_str())
        .map(str::to_string)
}

pub(super) fn cleanup_confirm_ready(typed: &str, account_id: &str) -> bool {
    !account_id.is_empty() && typed.trim() == account_id
}

fn str_field<'a>(value: &'a ManageValue, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .unwrap_or("")
}

/// Backend preview nests the actionable plan under `plan`
/// (`{provider, account_id, plan: {targets, warnings, backup_id, ...}}`);
/// tolerate a bare plan object for forward compatibility.
fn cleanup_plan<'a>(preview: &'a ManageValue) -> &'a ManageValue {
    preview.get("plan").unwrap_or(preview)
}

/// UI-selected preview identity: provider tab, account combo, scope radios
/// and the external-Default opt-in. Any change invalidates the review.
fn current_cleanup_key(manage: &ManageState, provider: &str) -> CleanupPreviewKey {
    CleanupPreviewKey {
        provider: provider.to_string(),
        account: manage.cleanup_account.clone(),
        scope: manage.cleanup_scope,
        include_external: manage.include_external_default,
    }
}

/// Full reset of the external Default account is canonical data: block
/// preview and apply until the opt-in is explicitly checked. The CLI takes
/// no safety flag, so this UI gate is the enforcement.
fn external_default_reset_blocked(
    scope: CleanupScopeSel,
    is_external_default: bool,
    opt_in: bool,
) -> bool {
    scope == CleanupScopeSel::FullReset && is_external_default && !opt_in
}

fn preview_targets(plan: &ManageValue) -> &[ManageValue] {
    plan.get("targets")
        .and_then(|targets| targets.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn summarize_data(data: &ManageValue) -> String {
    if let Some(message) = data.get("message").and_then(|message| message.as_str()) {
        return message.to_string();
    }
    if data.is_null() {
        return "ok".to_string();
    }
    let compact = serde_json::to_string(data).unwrap_or_else(|_| "ok".into());
    if compact.len() > 500 {
        format!("{}…", compact.chars().take(200).collect::<String>())
    } else {
        compact
    }
}

fn additional_limit(
    provider: ProviderId,
    usage: &crate::models::UsageData,
    limit: &crate::models::UsageLimit,
) -> bool {
    if !limit.is_active {
        return false;
    }
    // Only unscoped Claude aggregate buckets duplicate the main rows.
    // Model/scoped quotas remain visible even when their values match.
    if provider == ProviderId::Claude
        && limit.scope.is_none()
        && limit.model.is_none()
        && limit.model_id.is_none()
    {
        match limit.kind.as_str() {
            "session" if usage.session.available => return false,
            "weekly_all" if usage.weekly.available => return false,
            _ => {}
        }
    }
    true
}

fn account_status_summary(data: &ManageValue, language: LanguageId) -> String {
    if let Some(error) = data
        .get("error")
        .and_then(ManageValue::as_str)
        .filter(|s| !s.is_empty())
    {
        return error.to_string();
    }
    if let Some(status) = data.get("status") {
        let state = match (
            status.get("running").and_then(ManageValue::as_bool),
            status.get("running_unknown").and_then(ManageValue::as_bool),
        ) {
            (_, Some(true)) | (None, _) => "Account status unknown",
            (Some(true), _) => "Account running",
            (Some(false), Some(false)) => "Account not running",
            _ => "Account status unknown",
        };
        return language.text(state).to_string();
    }
    if let Some(focused) = data.get("focused").and_then(ManageValue::as_bool) {
        return language
            .text(if focused {
                "Window focused"
            } else {
                "Window not found"
            })
            .to_string();
    }
    if data.get("report").is_some() {
        return language.text("Account stopped").to_string();
    }
    language.text("Operation completed").to_string()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl StudioApp {
    fn provider_key(provider: ProviderId) -> &'static str {
        match provider {
            ProviderId::Claude => "claude",
            ProviderId::Codex => "codex",
            _ => "claude",
        }
    }

    fn manage_busy(&self) -> bool {
        self.manage.job.is_some()
    }

    fn spawn_manage(&mut self, kind: OpKind, argv: Vec<String>) {
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
        let (tx, rx) = std::sync::mpsc::channel();
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

    /// Drain a finished worker job: reload the fresh registry, refresh usage
    /// from the cache the monitor owns, and notify the monitor process.
    fn poll_manage(&mut self) {
        let done = match &self.manage.job {
            Some(job) => job.rx.try_recv().ok(),
            None => None,
        };
        let Some(done) = done else { return };
        let (kind, progress) = match self.manage.job.take() {
            Some(job) => (job.kind, job.progress),
            None => return,
        };
        if let Some(path) = &progress {
            let _ = std::fs::remove_file(path);
        }
        match done.result {
            Ok(data) => {
                self.manage.error = None;
                self.apply_manage_result(kind, data);
            }
            Err(error) => {
                if matches!(&kind, OpKind::CleanupApply) {
                    self.manage.cleanup_preview = None;
                    self.manage.cleanup_preview_key = None;
                    self.manage.cleanup_confirm.clear();
                }
                self.manage.error = Some(error);
            }
        }
        // A failed second stage (for example opening a newly created account)
        // can still have committed registry changes.
        self.reload_after_manage();
    }

    fn apply_manage_result(&mut self, kind: OpKind, data: ManageValue) {
        match kind {
            OpKind::Simple => {
                let discovery = data.get("applied").is_some() && data.get("added").is_some();
                self.manage.notice = Some(if discovery {
                    let added = data
                        .get("added")
                        .and_then(|value| value.as_array())
                        .map_or(0, Vec::len);
                    let removed = data
                        .get("removed")
                        .and_then(|value| value.as_array())
                        .map_or(0, Vec::len);
                    format!(
                        "{}: {added} · {}: {removed}",
                        self.language().text("New accounts"),
                        self.language().text("Removed")
                    )
                } else {
                    summarize_data(&data)
                });
            }
            OpKind::Status { key } => {
                let summary = account_status_summary(&data, self.language());
                self.manage.status.insert(key, summary);
            }
            OpKind::CleanupPreview { key } => {
                // Discard late results for a superseded selection: only the
                // currently selected identity may populate the review.
                let current = current_cleanup_key(
                    &self.manage,
                    Self::provider_key(self.manage.cleanup_provider),
                );
                if key == current {
                    self.manage.cleanup_preview = Some(data);
                    self.manage.cleanup_preview_key = Some(key);
                    self.manage.cleanup_confirm.clear();
                    self.manage.no_backup_ack = false;
                }
            }
            OpKind::CleanupApply => {
                self.manage.cleanup_preview = None;
                self.manage.cleanup_preview_key = None;
                self.manage.cleanup_confirm.clear();
                self.manage.no_backup_ack = false;
                self.manage.cleanup_result = Some(data);
            }
            OpKind::BackupList { provider, account } => {
                // Only the inventory for the still-visible filter may land:
                // a worker that outlived a tab/account change must never
                // install another account's backups under this filter.
                if self.backup_filter() == (provider.clone(), account.clone()) {
                    self.manage.backup_list_filter = Some((provider, account));
                    self.manage.backups = Some(data);
                }
            }
            OpKind::BackupOp => {
                self.manage.notice = Some(summarize_data(&data));
                // Refresh the filter selected NOW; the captured spawn-time
                // tab/account may be stale after a long backup operation.
                let (provider, account) = self.backup_filter();
                self.refresh_backup_list(provider, account);
            }
            OpKind::SkillsImport => {
                self.manage.skills_preview = Some(data);
            }
            OpKind::Migration => {
                self.manage.migration_preview = Some(data);
            }
        }
    }

    /// Adopt the authoritative managed registry after any CLI transaction and
    /// refresh the usage view; the monitor owns the cache file itself.
    fn reload_after_manage(&mut self) {
        let fresh = app_settings::load_settings();
        self.settings.accounts = fresh.accounts;
        // Committed rename/color drafts now match the fresh registry; drop
        // them so cards re-seed from authoritative values. Uncommitted edits
        // (draft differs) survive reloads.
        for accounts in [
            &self.settings.accounts.claude,
            &self.settings.accounts.codex,
        ] {
            for profile in &accounts.profiles {
                if self
                    .manage
                    .rename
                    .get(&profile.id)
                    .is_some_and(|draft| *draft == profile.name)
                {
                    self.manage.rename.remove(&profile.id);
                }
                if self
                    .manage
                    .color_drafts
                    .get(&profile.id)
                    .is_some_and(|draft| *draft == profile.color)
                {
                    self.manage.color_drafts.remove(&profile.id);
                }
            }
        }
        if let Some(cache) = app_settings::load_usage_cache() {
            self.manage.last_snapshot_unix = Some(cache.updated_unix);
            self.update_usage_cache(cache);
        }
        self.notify_owner();
    }

    fn status_key(provider: ProviderId, id: &str) -> String {
        format!("{}:{}", Self::provider_key(provider), id)
    }

    /// Visible inventory filter: the provider tab plus the account id field.
    /// The empty account intentionally lists every account, including a
    /// deleted one whose backup is the restore/undo path; the filter is never
    /// resolved against the current profiles.
    fn backup_filter(&self) -> (String, String) {
        (
            Self::provider_key(self.manage.provider_tab).to_string(),
            self.manage.backup_account.clone(),
        )
    }

    fn refresh_backup_list(&mut self, provider: String, account: String) {
        let mut argv = vec![
            "backup".to_string(),
            "list".to_string(),
            "--provider".to_string(),
            provider.clone(),
        ];
        if !account.is_empty() {
            argv.push("--id".to_string());
            argv.push(account.clone());
        }
        self.spawn_manage(OpKind::BackupList { provider, account }, argv);
    }

    /// Account manager: provider tabs, live cards, cleanup, backups, skills,
    /// migration. Rendered at the top of the Settings page so the dashboard
    /// opens directly on account navigation.
    pub(super) fn manage_section(&mut self, ui: &mut egui::Ui) {
        self.poll_manage();
        let language = self.language();
        section(ui, language.text("Account manager"), |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.manage.provider_tab,
                    ProviderId::Claude,
                    language.text("Claude Code"),
                );
                ui.selectable_value(
                    &mut self.manage.provider_tab,
                    ProviderId::Codex,
                    language.text("Codex"),
                );
                if let Some(job) = self.manage.job.as_ref() {
                    // Numeric elapsed/deadline beside the already-localized
                    // pending label; the worker thread keeps the UI responsive
                    // and a one-second repaint ticks the counter.
                    let elapsed = job.started.elapsed().as_secs();
                    let deadline = job.deadline.as_secs();
                    ui.weak(format!(
                        "{} {elapsed}s / {deadline}s",
                        language.text("Operation running...")
                    ));
                    ui.ctx().request_repaint_after(Duration::from_secs(1));
                }
            });
            if let Some(error) = self.manage.error.clone() {
                ui.colored_label(egui::Color32::from_rgb(196, 64, 64), error);
            }
            if let Some(notice) = self.manage.notice.clone() {
                ui.weak(notice);
            }
            let provider = self.manage.provider_tab;
            self.account_cards(ui, provider, language);
            self.cleanup_ui(ui, provider, language);
            self.backups_ui(ui, provider, language);
            self.skills_ui(ui, language);
            self.migration_ui(ui, language);
        });
    }

    fn account_cards(&mut self, ui: &mut egui::Ui, provider: ProviderId, language: LanguageId) {
        let key = Self::provider_key(provider);
        let profiles: Vec<crate::accounts::AccountProfile> = self
            .settings
            .accounts
            .get(provider)
            .map(|accounts| accounts.ordered().into_iter().cloned().collect())
            .unwrap_or_default();
        let now = SystemTime::now();
        let snapshot_unix = self.manage.last_snapshot_unix;
        ui.horizontal_wrapped(|ui| {
            let mut enabled = self.settings.auto_discovery_enabled;
            if ui
                .checkbox(
                    &mut enabled,
                    language.text("Discover accounts automatically"),
                )
                .changed()
            {
                self.settings.auto_discovery_enabled = enabled;
                self.save_settings();
            }
            ui.label(language.text("Every"));
            let mut hours = self.settings.discovery_interval_hours;
            if ui
                .add(egui::DragValue::new(&mut hours).range(1..=168).suffix(" h"))
                .changed()
            {
                self.settings.discovery_interval_hours = hours;
                self.save_settings();
            }
            if ui
                .add_enabled(
                    !self.manage_busy(),
                    egui::Button::new(language.text("Scan for accounts now")),
                )
                .clicked()
            {
                self.spawn_manage(
                    OpKind::Simple,
                    vec!["discover".to_string(), "--apply".to_string()],
                );
            }
        });
        if self.settings.last_discovery_unix != 0 {
            ui.weak(format!(
                "{}: {} {} · {} {} · {} {}",
                language.text("Last scan"),
                language.text("New accounts"),
                self.settings.last_discovery_added,
                language.text("Removed"),
                self.settings.last_discovery_removed,
                language.text("Pending"),
                self.settings.last_discovery_pending
            ));
        }
        ui.separator();
        // Add / import: ordinary operations, no confirmation.
        ui.horizontal_wrapped(|ui| {
            ui.add(
                crate::ui::components::text_field::singleline(&mut self.manage.new_name)
                    .desired_width(180.0)
                    .hint_text(language.text("Account name")),
            );
            if ui
                .add_enabled(
                    !self.manage_busy() && !self.manage.new_name.trim().is_empty(),
                    egui::Button::new(language.text("Add account")),
                )
                .clicked()
            {
                let mut argv = vec![
                    "create".to_string(),
                    "--provider".to_string(),
                    key.to_string(),
                    "--name".to_string(),
                    std::mem::take(&mut self.manage.new_name),
                ];
                if provider == ProviderId::Codex {
                    argv.push("--no-open".to_string());
                }
                self.spawn_manage(OpKind::Simple, argv);
            }
        });
        for profile in &profiles {
            let id = profile.id.clone();
            ui.push_id(format!("manage-card-{key}-{id}"), |ui| {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(&profile.name).strong());
                });
                // Live usage from the monitor cache (server poller only).
                self.account_usage_lines(ui, provider, profile, language, now, snapshot_unix);
                let desktop_available = provider != ProviderId::Codex
                    || (id.eq_ignore_ascii_case("default")
                        && profile.ownership != crate::accounts::AccountOwnership::Managed);
                // Ordinary operations: no confirmation.
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            desktop_available,
                            egui::Button::new(language.text("Open account")),
                        )
                        .clicked()
                    {
                        self.spawn_manage(
                            OpKind::Simple,
                            vec![
                                "open".to_string(),
                                "--provider".to_string(),
                                key.to_string(),
                                "--id".to_string(),
                                id.clone(),
                            ],
                        );
                    }
                    if provider == ProviderId::Codex
                        && !desktop_available
                        && profile.ownership == crate::accounts::AccountOwnership::Managed
                    {
                        if ui
                            .add_enabled(
                                !self.manage_busy(),
                                egui::Button::new(language.text("Sign in through CLI")),
                            )
                            .on_hover_text(
                                language.text(
                                    "Opens a Codex sign-in console for this CLI-only profile.",
                                ),
                            )
                            .clicked()
                        {
                            self.spawn_manage(
                                OpKind::Simple,
                                vec![
                                    "signin".to_string(),
                                    "--provider".to_string(),
                                    key.to_string(),
                                    "--id".to_string(),
                                    id.clone(),
                                ],
                            );
                        }
                        if ui
                            .add_enabled(
                                !self.manage_busy(),
                                egui::Button::new(language.text("Import sign-in from omp")),
                            )
                            .on_hover_text(language.text(
                                "Reuses an existing omp ChatGPT sign-in. UsageHub never refreshes omp's token.",
                            ))
                            .clicked()
                        {
                            self.spawn_manage(
                                OpKind::Simple,
                                vec![
                                    "signin".to_string(),
                                    "--provider".to_string(),
                                    key.to_string(),
                                    "--id".to_string(),
                                    id.clone(),
                                    "--from-omp".to_string(),
                                ],
                            );
                        }
                    }
                    let delete_label = "Review account cleanup...";
                    if ui
                        .add_enabled(
                            !self.manage_busy(),
                            egui::Button::new(language.text(delete_label)),
                        )
                        .clicked()
                    {
                        self.manage.cleanup_account = id.clone();
                        self.manage.cleanup_provider = provider;
                        self.manage.cleanup_name = profile.name.clone();
                        self.manage.cleanup_scope =
                            if profile.ownership == crate::accounts::AccountOwnership::Managed {
                                CleanupScopeSel::LocalData
                            } else {
                                CleanupScopeSel::FullReset
                            };
                        self.manage.cleanup_step = 0;
                        self.manage.navigate_cleanup = true;
                        self.manage.cleanup_preview = None;
                        self.manage.cleanup_preview_key = None;
                        self.manage.include_external_default = false;
                        self.manage.no_backup_ack = false;
                        self.manage.cleanup_confirm.clear();
                        self.manage.skip_backup = false;
                        self.manage.cleanup_result = None;
                        self.manage.error = None;
                    }
                });
                ui.collapsing(
                    language.text("Account details and advanced actions"),
                    |ui| {
                        ui.weak(format!("id: {id}"));
                        self.account_usage_details(
                            ui,
                            provider,
                            profile,
                            language,
                            snapshot_unix,
                            now,
                        );
                        if ui
                            .add_enabled(
                                desktop_available,
                                egui::Button::new(language.text("Focus window")),
                            )
                            .clicked()
                        {
                            self.spawn_manage(
                                OpKind::Status {
                                    key: Self::status_key(provider, &id),
                                },
                                vec![
                                    "focus".to_string(),
                                    "--provider".to_string(),
                                    key.to_string(),
                                    "--id".to_string(),
                                    id.clone(),
                                ],
                            );
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui.button(language.text("Stop account")).clicked() {
                                self.spawn_manage(
                                    OpKind::Status {
                                        key: Self::status_key(provider, &id),
                                    },
                                    vec![
                                        "stop".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                    ],
                                );
                            }
                            let stop_key = Self::status_key(provider, &id);
                            if self.manage.confirm_force_stop.as_deref() == Some(&stop_key) {
                                if ui
                                    .button(language.text("Force stop"))
                                    .on_hover_text(language.text("Force stop"))
                                    .clicked()
                                {
                                    self.manage.confirm_force_stop = None;
                                    self.spawn_manage(
                                        OpKind::Status { key: stop_key },
                                        vec![
                                            "stop".to_string(),
                                            "--provider".to_string(),
                                            key.to_string(),
                                            "--id".to_string(),
                                            id.clone(),
                                            "--force".to_string(),
                                        ],
                                    );
                                }
                                if ui.button(language.text("Cancel")).clicked() {
                                    self.manage.confirm_force_stop = None;
                                }
                            } else if ui
                                .button(format!("{}…", language.text("Force stop")))
                                .clicked()
                            {
                                self.manage.confirm_force_stop = Some(stop_key);
                            }
                            if ui.button(language.text("Check account status")).clicked() {
                                self.spawn_manage(
                                    OpKind::Status {
                                        key: Self::status_key(provider, &id),
                                    },
                                    vec![
                                        "status".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                    ],
                                );
                            }
                            if ui.button(language.text("Refresh usage now")).clicked() {
                                let result = self.request_live_usage_refresh();
                                let line = match result {
                                    Ok(()) => language.text("Usage refresh requested").to_string(),
                                    Err(error) => error,
                                };
                                self.manage
                                    .status
                                    .insert(Self::status_key(provider, &id), line);
                            }
                        });
                        if let Some(line) = self
                            .manage
                            .status
                            .get(&Self::status_key(provider, &id))
                            .cloned()
                        {
                            ui.weak(line);
                        }
                        // Rename + pin/order/color: ordinary display edits, no confirmation.
                        ui.horizontal_wrapped(|ui| {
                            let buffer = self
                                .manage
                                .rename
                                .entry(id.clone())
                                .or_insert_with(|| profile.name.clone());
                            ui.add(
                                crate::ui::components::text_field::singleline(buffer)
                                    .desired_width(180.0),
                            );
                            if ui.button(language.text("Rename account")).clicked() {
                                let name = self.manage.rename.get(&id).cloned().unwrap_or_default();
                                self.spawn_manage(
                                    OpKind::Simple,
                                    vec![
                                        "rename".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                        "--name".to_string(),
                                        name,
                                    ],
                                );
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            let pinned_label = if profile.pinned {
                                language.text("Unpin from widget")
                            } else {
                                language.text("Pin to widget")
                            };
                            if ui.button(pinned_label).clicked() {
                                self.spawn_manage(
                                    OpKind::Simple,
                                    vec![
                                        "pin".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                        "--value".to_string(),
                                        (!profile.pinned).to_string(),
                                        "--order".to_string(),
                                        profile.sort_order.to_string(),
                                    ],
                                );
                            }
                            ui.label(language.text("Display order"));
                            let mut order = profile.sort_order;
                            if crate::ui::components::number_field::NumberField::new(&mut order)
                                .range(-999..=999)
                                .speed(1.0)
                                .show(ui, 90.0)
                                .changed()
                            {
                                self.spawn_manage(
                                    OpKind::Simple,
                                    vec![
                                        "pin".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                        "--value".to_string(),
                                        profile.pinned.to_string(),
                                        "--order".to_string(),
                                        order.to_string(),
                                    ],
                                );
                            }
                            ui.label(language.text("Colour"));
                            let commit = {
                                let draft = self
                                    .manage
                                    .color_drafts
                                    .entry(id.clone())
                                    .or_insert_with(|| profile.color.clone());
                                ui.add(
                                    crate::ui::components::text_field::singleline(draft)
                                        .desired_width(90.0),
                                )
                                .lost_focus()
                                    && *draft != profile.color
                            };
                            if commit {
                                let color = self
                                    .manage
                                    .color_drafts
                                    .get(&id)
                                    .cloned()
                                    .unwrap_or_default();
                                self.spawn_manage(
                                    OpKind::Simple,
                                    vec![
                                        "pin".to_string(),
                                        "--provider".to_string(),
                                        key.to_string(),
                                        "--id".to_string(),
                                        id.clone(),
                                        "--value".to_string(),
                                        profile.pinned.to_string(),
                                        "--order".to_string(),
                                        profile.sort_order.to_string(),
                                        "--color".to_string(),
                                        color,
                                    ],
                                );
                            }
                        });
                    },
                );
            });
        }
        ui.separator();
        ui.collapsing(language.text("Import an existing account"), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    crate::ui::components::text_field::singleline(&mut self.manage.import_path)
                        .desired_width(260.0)
                        .hint_text(language.text("Exact source")),
                );
                ui.add(
                    crate::ui::components::text_field::singleline(&mut self.manage.import_name)
                        .desired_width(140.0)
                        .hint_text(language.text("Account name")),
                );
                if ui.button(language.text("Import account")).clicked() {
                    let mut argv = vec![
                        "import".to_string(),
                        "--provider".to_string(),
                        key.to_string(),
                        "--path".to_string(),
                        std::mem::take(&mut self.manage.import_path),
                    ];
                    let name = std::mem::take(&mut self.manage.import_name);
                    if !name.trim().is_empty() {
                        argv.push("--name".to_string());
                        argv.push(name);
                    }
                    self.spawn_manage(OpKind::Simple, argv);
                }
            });
        });
    }

    fn account_usage_lines(
        &self,
        ui: &mut egui::Ui,
        provider: ProviderId,
        profile: &crate::accounts::AccountProfile,
        language: LanguageId,
        now: SystemTime,
        _snapshot_unix: Option<u64>,
    ) {
        let account = self.usage.as_ref().and_then(|usage| {
            usage.accounts.iter().find(|candidate| {
                candidate.provider == provider && candidate.profile.same_source(profile)
            })
        });
        let Some(account) = account else {
            ui.weak(language.text("Waiting for usage"));
            return;
        };
        if let Some(error) = account.error {
            if matches!(error, crate::poller::PollError::NoCredentials) {
                ui.colored_label(
                    egui::Color32::from_rgb(196, 112, 32),
                    language.text("Sign in required"),
                );
                ui.weak(
                    language
                        .text("Open this account and sign in through the official application."),
                );
                ui.collapsing(language.text("Technical details"), |ui| {
                    ui.label(error.message(language));
                });
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(196, 112, 32),
                    error.message(language),
                );
            }
        }
        if let Some(usage) = &account.usage {
            if usage.stale {
                ui.weak(language.text("Last known usage"));
            }
            egui::Grid::new("quota-grid")
                .spacing(egui::vec2(16.0, 2.0))
                .show(ui, |ui| {
                    ui.colored_label(muted(), language.text("Period"));
                    ui.colored_label(muted(), language.text("Used"))
                        .on_hover_text(language.text("This table shows used quota; the main bar may show remaining quota. An unknown reset means the provider did not report one, not that refresh failed."));
                    ui.colored_label(muted(), language.text("Reset"))
                        .on_hover_text(language.text("This table shows used quota; the main bar may show remaining quota. An unknown reset means the provider did not report one, not that refresh failed."));
                    ui.end_row();
                    for (label, section) in [
                        ("Session", Some(&usage.session)),
                        ("Weekly", Some(&usage.weekly)),
                        ("Monthly", usage.monthly.as_ref()),
                    ] {
                        let Some(section) = section.filter(|section| section.available) else {
                            continue;
                        };
                        ui.label(language.text(label));
                        ui.label(format!("{:.0}%", section.percentage));
                        let reset = match section.resets_at {
                            None => language.text("Unknown reset").to_string(),
                            Some(reset) => match reset.duration_since(now) {
                                Ok(left) => format_countdown(language, left),
                                Err(_) => {
                                    language.text("Expired; waiting for next poll").to_string()
                                }
                            },
                        };
                        let response = ui.label(reset);
                        if let Some(details) =
                            reset_display(language, language.text(label), section, now)
                        {
                            response.on_hover_text(details);
                        }
                        ui.end_row();
                    }
                });
        } else if account.error.is_none() {
            ui.weak(language.text("Waiting for usage"));
        }
    }

    fn account_usage_details(
        &self,
        ui: &mut egui::Ui,
        provider: ProviderId,
        profile: &crate::accounts::AccountProfile,
        language: LanguageId,
        snapshot_unix: Option<u64>,
        now: SystemTime,
    ) {
        let Some(account) = self.usage.as_ref().and_then(|usage| {
            usage.accounts.iter().find(|candidate| {
                candidate.provider == provider && candidate.profile.same_source(profile)
            })
        }) else {
            return;
        };
        if let Some(usage) = &account.usage {
            if let Some(credits) = &usage.credits {
                ui.label(format!(
                    "{}: {:.0}%",
                    language.text("Credits"),
                    credits.percentage
                ));
            }
            for limit in &usage.limits {
                if additional_limit(provider, usage, limit) {
                    if let Some(line) = reset_display(language, &limit.label, &limit.usage, now) {
                        ui.weak(line);
                    }
                }
            }
        }
        let mut provenance = format!(
            "{}: {}",
            language.text("Exact source"),
            account
                .source_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| account.source_signature.clone())
        );
        if let Some(updated) = snapshot_unix {
            provenance.push_str(&format!(
                " · {}: {}",
                language.text("Last snapshot"),
                format_unix_utc(updated)
            ));
        }
        ui.collapsing(language.text("Technical details"), |ui| {
            ui.weak(provenance);
        });
    }

    fn cleanup_ui(&mut self, ui: &mut egui::Ui, _provider: ProviderId, language: LanguageId) {
        let provider = self.manage.cleanup_provider;
        let key = Self::provider_key(provider);
        if !self.manage.navigate_cleanup {
            return;
        }
        let mut open = true;
        let mut close = false;
        let busy = self.manage_busy();
        egui::Window::new(language.text("Account cleanup"))
            .id(egui::Id::new(("cleanup-preview", key)))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .max_width((ui.ctx().content_rect().width() - 40.0).max(240.0))
            .default_width(620.0)
            .default_height((ui.ctx().content_rect().height() - 40.0).clamp(420.0, 720.0))
            .max_height((ui.ctx().content_rect().height() - 40.0).max(240.0))
            .vscroll(true)
            .show(ui.ctx(), |ui| {
                // Keep every action and its consequence inside one screenful.
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.heading(&self.manage.cleanup_name);
                ui.label(format!("{}: {}", language.text("Stable account ID"), self.manage.cleanup_account));
                if busy {
                    let elapsed = self
                        .manage
                        .job
                        .as_ref()
                        .map(|job| job.started.elapsed().as_secs())
                        .unwrap_or(0);
                    let phase = self
                        .manage
                        .job
                        .as_ref()
                        .and_then(|job| job.progress.as_ref())
                        .and_then(|path| cleanup_progress_phase(path));
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        let label = match phase.as_deref() {
                            Some("Collecting targets") => language.text("Collecting targets"),
                            Some("Preparing") => language.text("Preparing"),
                            Some("Stopping the account") => language.text("Stopping the account"),
                            Some("Creating encrypted backup") => {
                                language.text("Creating encrypted backup")
                            }
                            Some("Removing credentials") => {
                                language.text("Removing credentials")
                            }
                            Some("Removing files") => language.text("Removing files"),
                            Some("Removing folders") => language.text("Removing folders"),
                            Some("Updating the account list") => {
                                language.text("Updating the account list")
                            }
                            Some("Ready") => language.text("Ready"),
                            _ => language.text("Starting..."),
                        };
                        ui.label(egui::RichText::new(label).strong());
                        ui.weak(format!("· {elapsed} s"));
                    });
                    ui.weak(language.text(
                        "This can take a few minutes for large accounts. Keep this window open.",
                    ));
                }
                if let Some(error) = self.manage.error.clone() {
                    let preview = self.manage.error_from_preview;
                    let headline = if preview {
                        "The preview could not be completed. Nothing was changed."
                    } else {
                        "The operation did not complete. Some changes may already have occurred."
                    };
                    ui.colored_label(egui::Color32::from_rgb(196, 64, 64), language.text(headline));
                    if preview && error.contains("canonical alias") {
                        ui.label(language.text(
                            "Claude's data folder no longer has config.json. Open the account once, then retry.",
                        ));
                        if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(language.text("Open account")),
                            )
                            .clicked()
                        {
                            self.spawn_manage(
                                OpKind::Simple,
                                vec![
                                    "open".to_string(),
                                    "--provider".to_string(),
                                    key.to_string(),
                                    "--id".to_string(),
                                    self.manage.cleanup_account.clone(),
                                ],
                            );
                        }
                    }
                    ui.collapsing(language.text("Technical details"), |ui| { ui.label(&error); });
                }
                if let Some(data) = &self.manage.cleanup_result {
                    let report = data.get("report").unwrap_or(data);
                    let removed = report.get("removed_from_registry").and_then(|v| v.as_bool()) == Some(true);
                    let failed_targets = report.get("results").and_then(|v| v.as_array())
                        .map(|results| results.iter().filter(|r| r.get("ok").and_then(|v| v.as_bool()) == Some(false)).count())
                        .unwrap_or(0);
                    let success = report.get("success").and_then(|v| v.as_bool()) == Some(true)
                        && failed_targets == 0
                        && (self.manage.cleanup_scope != CleanupScopeSel::LocalData || removed);
                    if success {
                        ui.label(egui::RichText::new(language.text("Cleanup completed.")).strong());
                        if removed {
                            ui.label(language.text("The profile was removed from UsageHub and the bar."));
                        } else if self.manage.cleanup_scope == CleanupScopeSel::FullReset || self.manage.cleanup_scope == CleanupScopeSel::Logout {
                            ui.label(language.text("The profile remains. Open it and sign in through the official application."));
                        }
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(196, 64, 64), language.text("Cleanup was not completed; some changes may already have occurred."));
                        if self.manage.cleanup_scope == CleanupScopeSel::LocalData && !removed {
                            ui.label(language.text("The profile was not removed from UsageHub or the bar."));
                        }
                        ui.label(language.text("Review the details, then preview again to retry."));
                    }
                    if failed_targets > 0 {
                        ui.label(format!("{}: {failed_targets}", language.text("Failed items")));
                    }
                    if report.get("warnings").and_then(|v| v.as_array()).is_some_and(|w| !w.is_empty()) {
                        ui.label(language.text("Details include warnings from the cleanup operation."));
                    }
                    let backup = str_field(report, "backup_id");
                    if !backup.is_empty() {
                        ui.label(format!("{}: {backup}", language.text("Encrypted backup ID")));
                    } else if self.manage.skip_backup {
                        ui.label(language.text("No backup was requested."));
                    } else {
                        ui.label(language.text("No backup was recorded in this report."));
                    }
                    ui.collapsing(language.text("Technical details"), |ui| {
                        if let Some(warnings) = report.get("warnings").and_then(|v| v.as_array()) {
                            for warning in warnings { ui.label(warning.as_str().unwrap_or("?")); }
                        }
                        if let Some(results) = report.get("results").and_then(|v| v.as_array()) {
                            for result in results {
                                ui.label(format!("{} · {}", str_field(result, "path"), str_field(result, "message")));
                            }
                        }
                    });
                    if !success && ui.add_enabled(!busy, egui::Button::new(language.text("Review and retry"))).clicked() {
                        self.manage.cleanup_result = None;
                        self.manage.error = None;
                    }
                } else {
                    let profile = self.settings.accounts.get(provider)
                        .and_then(|accounts| accounts.find(&self.manage.cleanup_account));
                    let managed = profile.is_some_and(|p| p.ownership == crate::accounts::AccountOwnership::Managed);
                    let external_default = profile.is_some_and(|p| p.ownership != crate::accounts::AccountOwnership::Managed && p.id.eq_ignore_ascii_case("default"));
                    if profile.is_none() {
                        ui.colored_label(egui::Color32::from_rgb(196, 64, 64), language.text("This profile is no longer registered. Close this window and choose another account."));
                    }
                    let current = current_cleanup_key(&self.manage, key);
                    if self.manage.cleanup_preview_key.as_ref() != Some(&current) {
                        self.manage.cleanup_preview = None;
                        self.manage.cleanup_preview_key = None;
                        self.manage.cleanup_confirm.clear();
                        self.manage.no_backup_ack = false;
                        self.manage.skip_backup = false;
                    }
                    let blocked = external_default_reset_blocked(current.scope, external_default, current.include_external);
                    let allowed = profile.is_some() && (current.scope != CleanupScopeSel::LocalData || managed);
                    ui.weak(language.text("Nothing changes until you confirm."));
                    if self.manage.cleanup_step == 0 {
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.radio_value(&mut self.manage.cleanup_scope, CleanupScopeSel::Logout, CleanupScopeSel::Logout.label(language));
                            ui.radio_value(&mut self.manage.cleanup_scope, CleanupScopeSel::FullReset, CleanupScopeSel::FullReset.label(language));
                            if ui.add_enabled(managed, egui::RadioButton::new(self.manage.cleanup_scope == CleanupScopeSel::LocalData, CleanupScopeSel::LocalData.label(language))).clicked() {
                                self.manage.cleanup_scope = CleanupScopeSel::LocalData;
                            }
                            ui.weak(language.text(match self.manage.cleanup_scope {
                                CleanupScopeSel::Logout => "Removes sign-in data only. Chats stay.",
                                CleanupScopeSel::FullReset => "Clears sign-in, history and settings. The profile stays.",
                                CleanupScopeSel::LocalData => "The same, and the profile leaves UsageHub and the bar.",
                                CleanupScopeSel::Cache => "Regenerable cache only, no backup.",
                            }));
                            if external_default {
                                ui.weak(language.text("Default belongs to the official app and cannot be removed from UsageHub."));
                            }
                            if external_default && self.manage.cleanup_scope == CleanupScopeSel::FullReset {
                                ui.checkbox(&mut self.manage.include_external_default, language.text("I explicitly agree to reset the main Default account's local data."));
                            }
                        });
                        if blocked {
                            ui.weak(language.text("Select the main-account consent above before previewing."));
                        }
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!busy && !blocked && allowed, egui::Button::new(language.text("Continue"))).clicked() {
                                self.manage.cleanup_preview = None;
                                self.manage.cleanup_preview_key = None;
                                self.manage.cleanup_confirm.clear();
                                self.manage.cleanup_step = 1;
                                let argv = vec!["cleanup".into(), "preview".into(), "--provider".into(), current.provider.clone(), "--id".into(), current.account.clone(), "--scope".into(), current.scope.cli_name().into()];
                                self.spawn_manage(OpKind::CleanupPreview { key: current.clone() }, argv);
                            }
                            if ui.add_enabled(!busy, egui::Button::new(language.text("Cancel"))).clicked() {
                                close = true;
                            }
                        });
                        ui.collapsing(language.text("Details"), |ui| {
                            ui.weak(language.text("These actions affect local data only; they do not erase cloud or system-wide records."));
                            if ui.add_enabled(!busy, egui::Button::new(language.text("Clear cache only"))).clicked() {
                                self.manage.cleanup_scope = CleanupScopeSel::Cache;
                                self.manage.cleanup_preview = None;
                                self.manage.cleanup_preview_key = None;
                                self.manage.cleanup_confirm.clear();
                                self.manage.cleanup_step = 1;
                                let cache = current_cleanup_key(&self.manage, key);
                                let argv = vec!["cleanup".into(), "preview".into(), "--provider".into(), cache.provider.clone(), "--id".into(), cache.account.clone(), "--scope".into(), "cache".into()];
                                self.spawn_manage(OpKind::CleanupPreview { key: cache }, argv);
                            }
                        });
                    } else {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(!busy, egui::Button::new(language.text("Back"))).clicked() {
                                self.manage.cleanup_step = 0;
                                self.manage.cleanup_preview = None;
                                self.manage.cleanup_preview_key = None;
                                self.manage.cleanup_confirm.clear();
                            }
                            ui.label(egui::RichText::new(current.scope.label(language)).strong());
                        });
                        if blocked {
                            ui.weak(language.text("Select the main-account consent above before previewing."));
                        }
                        if let Some(preview) = self.manage.cleanup_preview.clone() {
                            Self::render_cleanup_preview(ui, &preview, language, current.scope, self.manage.skip_backup);
                            if current.scope != CleanupScopeSel::Cache {
                                ui.add_enabled_ui(!busy, |ui| {
                                    ui.collapsing(language.text("Advanced"), |ui| {
                                        if ui.checkbox(&mut self.manage.skip_backup, language.text("Skip the encrypted backup")).changed() {
                                            self.manage.no_backup_ack = false;
                                        }
                                        if self.manage.skip_backup {
                                            ui.checkbox(&mut self.manage.no_backup_ack, language.text("I understand there will be no backup to undo this cleanup."));
                                        }
                                    });
                                });
                            }
                            ui.label(format!("{}: {}", language.text("Type this exact account ID to confirm"), current.account));
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.add(crate::ui::components::text_field::singleline(&mut self.manage.cleanup_confirm).desired_width(220.0));
                            });
                            let key_ok = self.manage.cleanup_preview_key.as_ref() == Some(&current);
                            let typed = cleanup_confirm_ready(&self.manage.cleanup_confirm, &current.account);
                            let backup_ok = !self.manage.skip_backup || self.manage.no_backup_ack;
                            let ready = key_ok && typed && backup_ok && !blocked && allowed && !busy;
                            if !ready {
                                let reason = if busy { "Wait for the operation to finish." } else if !key_ok { "Preparing the review…" } else if blocked { "Select the main-account consent above before previewing." } else if !allowed { "This account cannot use the selected action." } else if !typed { "Type the exact account ID shown above." } else { "Acknowledge the no-backup choice above." };
                                ui.weak(language.text(reason));
                            }
                            ui.weak(language.text("Nothing has been deleted yet."));
                            if ui.add_enabled(ready, egui::Button::new(language.text("Apply cleanup"))).clicked() {
                                let mut argv = vec!["cleanup".into(), "apply".into(), "--provider".into(), current.provider.clone(), "--id".into(), current.account.clone(), "--scope".into(), current.scope.cli_name().into(), "--confirm".into(), current.account.clone()];
                                if self.manage.skip_backup { argv.push("--no-backup".into()); }
                                self.spawn_manage(OpKind::CleanupApply, argv);
                            }
                        } else {
                            ui.weak(language.text("Preparing the review…"));
                        }
                    }
                }
                if ui.add_enabled(!busy, egui::Button::new(language.text("Close"))).clicked() {
                    close = true;
                }
            });
        if (!open || close) && !busy {
            self.manage.navigate_cleanup = false;
            self.manage.cleanup_account.clear();
            self.manage.cleanup_preview = None;
            self.manage.cleanup_preview_key = None;
            self.manage.cleanup_confirm.clear();
            self.manage.no_backup_ack = false;
            self.manage.skip_backup = false;
            self.manage.cleanup_result = None;
            self.manage.error = None;
            self.manage.error_from_preview = false;
            self.manage.cleanup_step = 0;
        }
    }

    fn render_cleanup_preview(
        ui: &mut egui::Ui,
        preview: &ManageValue,
        language: LanguageId,
        scope: CleanupScopeSel,
        no_backup: bool,
    ) {
        let plan = cleanup_plan(preview);
        let targets = preview_targets(plan);
        let bytes: u64 = targets
            .iter()
            .filter_map(|t| t.get("size_bytes").and_then(|s| s.as_u64()))
            .sum();
        ui.label(
            egui::RichText::new(format!(
                "{}: {} · {}",
                language.text("Will be deleted"),
                targets.len(),
                human_bytes(bytes)
            ))
            .strong(),
        );
        if scope != CleanupScopeSel::Cache {
            if no_backup {
                ui.colored_label(
                    egui::Color32::from_rgb(196, 64, 64),
                    language.text("No backup will be created."),
                );
            } else {
                ui.label(language.text("Encrypted backup: 7 days"));
            }
        }
        let warning_count = plan
            .get("warnings")
            .and_then(|value| value.as_array())
            .map_or(0, Vec::len);
        if warning_count > 0 {
            ui.weak(format!("{}: {warning_count}", language.text("Warnings")));
        }
        ui.collapsing(language.text("Details"), |ui| {
            if let Some(warnings) = plan.get("warnings").and_then(|value| value.as_array()) {
                for warning in warnings {
                    ui.label(warning.as_str().unwrap_or("?"));
                }
            }
            for target in targets.iter().take(64) {
                ui.label(format!(
                    "{} · {}",
                    str_field(target, "path"),
                    str_field(target, "kind")
                ));
            }
            if targets.len() > 64 {
                ui.weak(format!("… +{}", targets.len() - 64));
            }
        });
    }

    fn backups_ui(&mut self, ui: &mut egui::Ui, provider: ProviderId, language: LanguageId) {
        let key = Self::provider_key(provider);
        ui.collapsing(language.text("Backup inventory"), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(language.text("Account name"));
                let mut account = self.manage.backup_account.clone();
                ui.add(
                    crate::ui::components::text_field::singleline(&mut account)
                        .desired_width(140.0)
                        .hint_text(language.text("Account name")),
                );
                self.manage.backup_account = account;
                let busy = self.manage_busy();
                if ui
                    .add_enabled(!busy, egui::Button::new(language.text("Refresh")))
                    .clicked()
                {
                    self.refresh_backup_list(key.to_string(), self.manage.backup_account.clone());
                }
                if ui
                    .add_enabled(
                        !busy && !self.manage.backup_account.trim().is_empty(),
                        egui::Button::new(language.text("Backup before delete")),
                    )
                    .clicked()
                {
                    let account = self.manage.backup_account.clone();
                    self.spawn_manage(
                        OpKind::BackupOp,
                        vec![
                            "backup".to_string(),
                            "create".to_string(),
                            "--provider".to_string(),
                            key.to_string(),
                            "--id".to_string(),
                            account,
                        ],
                    );
                }
            });
            // The catalog belongs to the tab + account filter it was fetched
            // for. A changed filter retires the old inventory and any pending
            // restore/purge confirmation rather than showing another
            // account's backups. The check runs after the account field edit
            // so the same frame never renders a superseded list.
            let filter = self.backup_filter();
            if self.manage.backup_list_filter.as_ref() != Some(&filter) {
                self.manage.backups = None;
                self.manage.backup_list_filter = None;
                self.manage.confirm_restore = None;
                self.manage.confirm_purge = None;
            }
            ui.weak(language.text("Expired backups are purged on the next start."));
            let backups = self.manage.backups.clone().unwrap_or(ManageValue::Null);
            let list: &[ManageValue] = backups
                .get("backups")
                .and_then(|b| b.as_array())
                .map(Vec::as_slice)
                .or_else(|| backups.as_array().map(Vec::as_slice))
                .unwrap_or(&[]);
            for backup in list {
                let backup_id = str_field(backup, "backup_id").to_string();
                if backup_id.is_empty() {
                    continue;
                }
                ui.push_id(format!("backup-{backup_id}"), |ui| {
                    ui.separator();
                    let created = backup
                        .get("created_unix")
                        .and_then(|c| c.as_u64())
                        .map(format_unix_utc)
                        .unwrap_or_else(|| language.text("Unknown reset").to_string());
                    let expires = backup
                        .get("created_unix")
                        .and_then(|c| c.as_u64())
                        .map(|created| format_unix_utc(created + 7 * 24 * 3_600))
                        .unwrap_or_default();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(backup_id.clone()).strong());
                        ui.weak(format!(
                            "{}: {} · {}: {expires}",
                            language.text("Last snapshot"),
                            created,
                            language.text("Expires"),
                        ));
                        if let Some(size) = backup.get("size_bytes").and_then(|s| s.as_u64()) {
                            ui.weak(human_bytes(size));
                        }
                    });
                    ui.horizontal_wrapped(|ui| {
                        let busy = self.manage_busy();
                        if ui
                            .add_enabled(!busy, egui::Button::new(language.text("Verify backup")))
                            .clicked()
                        {
                            self.spawn_manage(
                                OpKind::BackupOp,
                                vec![
                                    "backup".to_string(),
                                    "verify".to_string(),
                                    "--backup".to_string(),
                                    backup_id.clone(),
                                ],
                            );
                        }
                        if self.manage.confirm_restore.as_deref() == Some(&backup_id) {
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(language.text("Restore backup")),
                                )
                                .clicked()
                            {
                                self.manage.confirm_restore = None;
                                self.spawn_manage(
                                    OpKind::BackupOp,
                                    vec![
                                        "backup".to_string(),
                                        "restore".to_string(),
                                        "--backup".to_string(),
                                        backup_id.clone(),
                                        "--confirm".to_string(),
                                        backup_id.clone(),
                                    ],
                                );
                            }
                            if ui.button(language.text("Cancel")).clicked() {
                                self.manage.confirm_restore = None;
                            }
                        } else if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(format!("{}…", language.text("Restore backup"))),
                            )
                            .clicked()
                        {
                            self.manage.confirm_restore = Some(backup_id.clone());
                        }
                        if self.manage.confirm_purge.as_deref() == Some(&backup_id) {
                            if ui
                                .add_enabled(!busy, egui::Button::new(language.text("Purge now")))
                                .clicked()
                            {
                                self.manage.confirm_purge = None;
                                self.spawn_manage(
                                    OpKind::BackupOp,
                                    vec![
                                        "backup".to_string(),
                                        "purge".to_string(),
                                        "--backup".to_string(),
                                        backup_id.clone(),
                                        "--confirm".to_string(),
                                        backup_id.clone(),
                                    ],
                                );
                            }
                            if ui.button(language.text("Cancel")).clicked() {
                                self.manage.confirm_purge = None;
                            }
                        } else if ui
                            .add_enabled(
                                !busy,
                                egui::Button::new(format!("{}…", language.text("Purge now"))),
                            )
                            .clicked()
                        {
                            self.manage.confirm_purge = Some(backup_id.clone());
                        }
                    });
                });
            }
        });
    }

    fn skills_ui(&mut self, ui: &mut egui::Ui, language: LanguageId) {
        ui.collapsing(language.text("Shared skills"), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    crate::ui::components::text_field::singleline(&mut self.manage.skills_path)
                        .desired_width(260.0)
                        .hint_text(language.text("Exact source")),
                );
                if ui.button(language.text("Open...")).clicked() {
                    if let Some(path) = choose_file(
                        self.owner,
                        language.text("Shared skills"),
                        "All files\0*.*\0\0",
                    ) {
                        self.manage.skills_path = path.to_string_lossy().into_owned();
                    }
                }
                ui.checkbox(
                    &mut self.manage.skills_replace,
                    language.text("Replace on conflict"),
                );
                if ui.button(language.text("Import...")).clicked() {
                    let mut argv = vec![
                        "skills".to_string(),
                        "import".to_string(),
                        "--path".to_string(),
                        self.manage.skills_path.clone(),
                    ];
                    if self.manage.skills_replace {
                        argv.push("--replace".to_string());
                    }
                    self.spawn_manage(OpKind::SkillsImport, argv);
                }
            });
            if let Some(preview) = self.manage.skills_preview.clone() {
                render_json_value(ui, &preview, 0);
            }
        });
    }

    fn migration_ui(&mut self, ui: &mut egui::Ui, language: LanguageId) {
        ui.collapsing(language.text("Migration"), |ui| {
            ui.horizontal_wrapped(|ui| {
                let busy = self.manage_busy();
                for (action, label) in [
                    ("preview", language.text("Preview")),
                    ("import", language.text("Import...")),
                ] {
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        self.spawn_manage(
                            OpKind::Migration,
                            vec!["migration".to_string(), action.to_string()],
                        );
                    }
                }
                if ui
                    .add_enabled(!busy, egui::Button::new(language.text("Delete...")))
                    .clicked()
                {
                    // Cutover retires the legacy install; the typed token is
                    // the literal word the CLI requires.
                    self.spawn_manage(
                        OpKind::Migration,
                        vec![
                            "migration".to_string(),
                            "cutover".to_string(),
                            "--confirm".to_string(),
                            "cutover".to_string(),
                        ],
                    );
                }
            });
            if let Some(preview) = self.manage.migration_preview.clone() {
                render_json_value(ui, &preview, 0);
            }
            ui.separator();
            ui.label(language.text("Legacy backups"));
            ui.horizontal_wrapped(|ui| {
                ui.add(
                    crate::ui::components::text_field::singleline(&mut self.manage.legacy_path)
                        .desired_width(220.0)
                        .hint_text(language.text("Exact source")),
                );
                for action in [
                    LegacyAction::Encrypt,
                    LegacyAction::Purge,
                    LegacyAction::Leave,
                ] {
                    let label = match action {
                        LegacyAction::Encrypt => language.text("Encrypt legacy backups"),
                        LegacyAction::Purge => language.text("Purge legacy backups"),
                        LegacyAction::Leave => language.text("Leave legacy backups"),
                    };
                    ui.radio_value(&mut self.manage.legacy_action, action, label);
                }
            });
            match self.manage.legacy_action {
                LegacyAction::Encrypt => {
                    ui.label(language.text("Encrypt creates a verified 7-day backup, then removes the plaintext original."));
                }
                LegacyAction::Purge => {
                    ui.colored_label(
                        egui::Color32::from_rgb(196, 112, 32),
                        language.text("Purge permanently deletes the legacy plaintext without a backup."),
                    );
                }
                LegacyAction::Leave => {}
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(language.text("Type the account id to confirm"));
                ui.add(
                    crate::ui::components::text_field::singleline(&mut self.manage.legacy_confirm)
                        .desired_width(160.0),
                );
                let ready = !self.manage.legacy_path.trim().is_empty()
                    && !self.manage.legacy_confirm.trim().is_empty();
                if ui
                    .add_enabled(ready && !self.manage_busy(), egui::Button::new(language.text("Apply")))
                    .clicked()
                {
                    let path = std::mem::take(&mut self.manage.legacy_path);
                    let confirm = std::mem::take(&mut self.manage.legacy_confirm);
                    let action = self.manage.legacy_action.cli_name().to_string();
                    self.spawn_manage(
                        OpKind::Migration,
                        vec![
                            "migration".to_string(),
                            "legacy-backup".to_string(),
                            "--path".to_string(),
                            path,
                            "--action".to_string(),
                            action,
                            "--confirm".to_string(),
                            confirm,
                        ],
                    );
                }
            });
        });
    }
}

/// Depth-limited generic JSON view for backend payloads whose exact shape is
/// owned by other slices (skills conflicts, migration reports, status).
fn render_json_value(ui: &mut egui::Ui, value: &ManageValue, depth: usize) {
    if depth > 3 {
        ui.weak("…");
        return;
    }
    match value {
        ManageValue::Null => {
            ui.weak("—");
        }
        ManageValue::Bool(flag) => {
            ui.weak(flag.to_string());
        }
        ManageValue::Number(number) => {
            ui.weak(number.to_string());
        }
        ManageValue::String(text) => {
            ui.weak(text);
        }
        ManageValue::Array(items) => {
            for (index, item) in items.iter().enumerate().take(32) {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(format!("#{index}"));
                    render_json_value(ui, item, depth + 1);
                });
            }
            if items.len() > 32 {
                ui.weak(format!("… +{}", items.len() - 32));
            }
        }
        ManageValue::Object(map) => {
            for (key, item) in map.iter().take(32) {
                ui.horizontal_wrapped(|ui| {
                    ui.weak(format!("{key}:"));
                    render_json_value(ui, item, depth + 1);
                });
            }
            if map.len() > 32 {
                ui.weak(format!("… +{}", map.len() - 32));
            }
        }
    }
}

#[cfg(test)]
mod manage_tests {
    use super::*;

    #[test]
    fn additional_limits_keep_scoped_quotas_even_when_values_match() {
        let mut usage = crate::models::UsageData::default();
        usage.weekly.available = true;
        usage.weekly.percentage = 100.0;
        let mut limit = crate::models::UsageLimit {
            kind: "weekly_all".into(),
            is_active: true,
            usage: usage.weekly.clone(),
            ..Default::default()
        };
        assert!(!additional_limit(ProviderId::Claude, &usage, &limit));
        limit.model = Some("model".into());
        assert!(additional_limit(ProviderId::Claude, &usage, &limit));
        limit.model = None;
        limit.scope = Some(serde_json::json!({"model": {"id": "x"}}));
        assert!(additional_limit(ProviderId::Claude, &usage, &limit));
        limit.scope = None;
        usage.weekly.available = false;
        assert!(additional_limit(ProviderId::Claude, &usage, &limit));
    }

    #[test]
    fn envelope_parses_ok_and_error_shapes() {
        let data = parse_manage_output(r#"{"ok":true,"data":{"id":"a"}}"#).unwrap();
        assert_eq!(data.get("id").and_then(|id| id.as_str()), Some("a"));
        let error = parse_manage_output(
            r#"{"ok":false,"error":{"code":"usage_error","message":"unknown command"}}"#,
        )
        .unwrap_err();
        assert!(error.contains("usage_error"));
        assert!(parse_manage_output("not json").is_err());
        assert!(parse_manage_output(r#"{"ok":true}"#).unwrap().is_null());
    }

    #[test]
    fn output_split_prefers_stdout_and_parses_stderr_envelope() {
        let data = manage_output_result(r#"{"ok":true,"data":{"id":"a"}}"#, "", Some(0)).unwrap();
        assert_eq!(data.get("id").and_then(|id| id.as_str()), Some("a"));
        let error = manage_output_result(
            "",
            r#"{"ok":false,"error":{"code":"operation_error","message":"account is busy"}}"#,
            Some(3),
        )
        .unwrap_err();
        assert_eq!(error, "operation_error: account is busy");
        let raw = manage_output_result("", "thread 'main' panicked", Some(101)).unwrap_err();
        assert!(raw.contains("panicked"), "{raw}");
        let empty = manage_output_result("", "", Some(2)).unwrap_err();
        assert!(empty.contains('2'), "{empty}");
    }

    #[test]
    fn absolute_reset_dates_format_in_utc() {
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_unix_utc(1_767_225_600), "2026-01-01 00:00 UTC");
    }

    fn section(resets_at: Option<SystemTime>) -> crate::models::UsageSection {
        crate::models::UsageSection {
            available: true,
            percentage: 12.0,
            resets_at,
        }
    }

    #[test]
    fn reset_display_never_estimates() {
        let now = SystemTime::now();
        assert!(
            reset_display(LanguageId::English, "Session", &section(None), now)
                .unwrap()
                .contains("Unknown reset")
        );
        let past = now.checked_sub(Duration::from_secs(60)).unwrap();
        assert!(
            reset_display(LanguageId::English, "Session", &section(Some(past)), now)
                .unwrap()
                .contains("waiting for next poll")
        );
        let future = now.checked_add(Duration::from_secs(3_700)).unwrap();
        let line =
            reset_display(LanguageId::English, "Session", &section(Some(future)), now).unwrap();
        assert!(line.contains("12%"), "{line}");
        assert!(line.contains("UTC"), "{line}");
        assert!(line.contains("1h"), "{line}");
        let absent = crate::models::UsageSection {
            available: false,
            ..Default::default()
        };
        assert_eq!(
            reset_display(LanguageId::English, "Session", &absent, now),
            None
        );
    }

    #[test]
    fn cleanup_preview_reads_nested_plan_with_null_backup() {
        // Actual CLI shape: data {provider, account_id, plan {targets,
        // warnings, backup_id: null}} — backup_id null means the backend
        // WILL snapshot after stop under the default apply path.
        let preview: ManageValue = serde_json::from_str(
            r#"{"provider":"claude","account_id":"a1","scope":"delete","plan":{"scope":"delete","provider":"claude","account_id":"a1","backup_id":null,"warnings":["External Default data excluded"],"targets":[{"path":"C:/x/cache","kind":"cache","owned":true,"size_bytes":10}]}}"#,
        )
        .unwrap();
        let plan = cleanup_plan(&preview);
        assert_eq!(preview_targets(plan).len(), 1);
        assert_eq!(str_field(plan, "account_id"), "a1");
        assert!(cleanup_confirm_ready("a1", "a1"));
        assert!(cleanup_confirm_ready(" a1 ", "a1"));
        assert!(!cleanup_confirm_ready("a2", "a1"));
        assert!(!cleanup_confirm_ready("a1", ""));
        // Bare plan objects (no envelope) keep working.
        let bare: ManageValue = serde_json::from_str(r#"{"targets":[],"backup_id":"b1"}"#).unwrap();
        assert_eq!(cleanup_plan(&bare), &bare);
    }

    #[test]
    fn cleanup_confirm_allows_metadata_only_delete() {
        // Verified metadata-only registry delete: no asset targets, but the
        // scope summary still needs the typed confirmation.
        let preview: ManageValue = serde_json::from_str(
            r#"{"provider":"claude","account_id":"a1","plan":{"targets":[],"backup_id":null}}"#,
        )
        .unwrap();
        let plan = cleanup_plan(&preview);
        assert!(preview_targets(plan).is_empty());
        assert!(cleanup_confirm_ready("a1", "a1"));
        assert!(!cleanup_confirm_ready("", "a1"));
    }

    #[test]
    fn cleanup_preview_key_tracks_selector_transitions() {
        let base = CleanupPreviewKey {
            provider: "claude".into(),
            account: "a".into(),
            scope: CleanupScopeSel::Cache,
            include_external: false,
        };
        assert_eq!(base.clone(), base);
        // Account switch A -> B invalidates the review.
        let switched = CleanupPreviewKey {
            account: "b".into(),
            ..base.clone()
        };
        assert_ne!(base, switched);
        // Scope switch Cache -> Delete invalidates the review.
        let rescoped = CleanupPreviewKey {
            scope: CleanupScopeSel::LocalData,
            ..base.clone()
        };
        assert_ne!(base, rescoped);
        // Opt-in toggle invalidates the review.
        let opted = CleanupPreviewKey {
            include_external: true,
            ..base.clone()
        };
        assert_ne!(base, opted);
        // Provider tab switch invalidates the review.
        let retabbed = CleanupPreviewKey {
            provider: "codex".into(),
            ..base.clone()
        };
        assert_ne!(base, retabbed);
    }

    #[test]
    fn external_default_full_reset_needs_opt_in() {
        assert!(external_default_reset_blocked(
            CleanupScopeSel::FullReset,
            true,
            false
        ));
        assert!(!external_default_reset_blocked(
            CleanupScopeSel::FullReset,
            true,
            true
        ));
        assert!(!external_default_reset_blocked(
            CleanupScopeSel::FullReset,
            false,
            false
        ));
        assert!(!external_default_reset_blocked(
            CleanupScopeSel::Cache,
            true,
            false
        ));
        assert!(!external_default_reset_blocked(
            CleanupScopeSel::LocalData,
            true,
            false
        ));
    }
}
