//! Account manager: native cards, cleanup, backups, skills and migration.
//!
//! Mutations shell out to this same executable's `--manage` JSON surface on
//! a worker thread (egui never blocks), while safe local read-only queries
//! (`status`, read-only `discover`, `backup list`, `migration preview`) run
//! directly in-process on the worker thread. On completion the fresh settings
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
use crate::providers::ProviderId;

#[path = "manage/backups_ui.rs"]
mod backups_ui;
#[path = "manage/cards.rs"]
mod cards;
#[path = "manage/cleanup_ui.rs"]
mod cleanup_ui;
#[path = "manage/migration_ui.rs"]
mod migration_ui;
#[path = "manage/skills_ui.rs"]
mod skills_ui;
#[path = "manage/worker.rs"]
mod worker;

use cleanup_ui::current_cleanup_key;
use worker::{ManageJob, OmpProfileStatus};

/// Latest `--manage` JSON payload by operation.
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
    pub(super) fn cli_name(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Logout => "logout",
            Self::LocalData => "delete",
            Self::FullReset => "reset",
        }
    }

    pub(super) fn label(self, language: LanguageId) -> &'static str {
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
    pub(super) fn cli_name(self) -> &'static str {
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
    pub(super) provider: String,
    pub(super) account: String,
    pub(super) scope: CleanupScopeSel,
    pub(super) include_external: bool,
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
    Migration {
        refresh_usage: bool,
    },
}

impl OpKind {
    fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::Status { .. }
                | Self::CleanupPreview { .. }
                | Self::BackupList { .. }
                | Self::Migration {
                    refresh_usage: false
                }
        )
    }
}

pub(super) struct ManageState {
    pub provider_tab: ProviderId,
    job: Option<ManageJob>,
    pub(super) error: Option<String>,
    pub(super) notice: Option<String>,
    pub(super) new_name: String,
    pub(super) rename: HashMap<String, String>,
    pub(super) import_path: String,
    pub(super) import_name: String,
    pub(super) color_drafts: HashMap<String, String>,
    pub(super) status: HashMap<String, String>,
    omp_statuses: HashMap<String, OmpProfileStatus>,
    omp_cache_loaded: bool,
    omp_refresh_rx: Option<mpsc::Receiver<HashMap<String, OmpProfileStatus>>>,
    pub(super) cleanup_account: String,
    pub(super) cleanup_provider: ProviderId,
    pub(super) cleanup_name: String,
    pub(super) cleanup_scope: CleanupScopeSel,
    pub(super) navigate_cleanup: bool,
    pub(super) cleanup_preview: Option<ManageValue>,
    pub(super) cleanup_preview_key: Option<CleanupPreviewKey>,
    pub(super) cleanup_confirm: String,
    pub(super) include_external_default: bool,
    pub(super) no_backup_ack: bool,
    pub(super) error_from_preview: bool,
    /// Cleanup dialog step: 0 = choose action, 1 = review/apply.
    pub(super) cleanup_step: u8,
    pub(super) skip_backup: bool,
    pub(super) cleanup_result: Option<ManageValue>,
    pub(super) confirm_force_stop: Option<String>,
    pub(super) confirm_purge: Option<String>,
    pub(super) confirm_restore: Option<String>,
    pub(super) backups: Option<ManageValue>,
    /// Provider tab + account filter that produced `backups`; the inventory
    /// is hidden and retired whenever the visible filter differs.
    pub(super) backup_list_filter: Option<(String, String)>,
    pub(super) backup_account: String,
    pub(super) skills_path: String,
    pub(super) skills_replace: bool,
    pub(super) skills_preview: Option<ManageValue>,
    pub(super) migration_preview: Option<ManageValue>,
    pub(super) legacy_path: String,
    pub(super) legacy_action: LegacyAction,
    pub(super) legacy_confirm: String,
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
            omp_statuses: HashMap::new(),
            omp_cache_loaded: false,
            omp_refresh_rx: None,
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

pub(super) fn str_field<'a>(value: &'a ManageValue, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .unwrap_or("")
}

pub(super) fn omp_import_status_message(data: &ManageValue, language: LanguageId) -> String {
    let base = language.text("Operation completed");
    match data
        .get("email")
        .and_then(ManageValue::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
    {
        Some(email) => format!("{base}: {email}"),
        None => base.to_string(),
    }
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

pub(super) fn human_bytes(bytes: u64) -> String {
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
    pub(super) fn provider_key(provider: ProviderId) -> &'static str {
        match provider {
            ProviderId::Claude => "claude",
            ProviderId::Codex => "codex",
            _ => "claude",
        }
    }

    pub(super) fn status_key(provider: ProviderId, id: &str) -> String {
        format!("{}:{}", Self::provider_key(provider), id)
    }

    /// Drain finished background jobs: omp metadata refresh and `--manage`
    /// worker completion (reloading registry/usage and notifying the monitor).
    fn poll_manage(&mut self) {
        if let Some(rx) = self.manage.omp_refresh_rx.as_ref() {
            if let Ok(statuses) = rx.try_recv() {
                self.manage.omp_statuses = statuses;
                self.manage.omp_cache_loaded = true;
                self.manage.omp_refresh_rx = None;
            }
        }
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
        let refresh_usage = !kind.is_read_only();
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
        // can still have committed registry changes. Read-only queries skip
        // triggering a live HTTP usage poll across all accounts.
        if refresh_usage {
            self.reload_after_manage();
        }
    }

    fn apply_manage_result(&mut self, kind: OpKind, data: ManageValue) {
        match kind {
            OpKind::Simple => {
                let discovery = data.get("applied").is_some() && data.get("added").is_some();
                let imported_from_omp = data
                    .get("imported_from_omp")
                    .and_then(ManageValue::as_bool)
                    .unwrap_or(false);
                if discovery {
                    let added = data
                        .get("added")
                        .and_then(|value| value.as_array())
                        .map_or(0, Vec::len);
                    let removed = data
                        .get("removed")
                        .and_then(|value| value.as_array())
                        .map_or(0, Vec::len);
                    self.manage.notice = Some(format!(
                        "{}: {added} · {}: {removed}",
                        self.language().text("New accounts"),
                        self.language().text("Removed")
                    ));
                } else if imported_from_omp {
                    let msg = omp_import_status_message(&data, self.language());
                    if let (Some(provider), Some(id)) = (
                        data.get("provider").and_then(ManageValue::as_str),
                        data.get("id").and_then(ManageValue::as_str),
                    ) {
                        self.manage
                            .status
                            .insert(format!("{provider}:{id}"), msg.clone());
                    }
                    self.manage.notice = Some(msg);
                } else {
                    self.manage.notice = Some(summarize_data(&data));
                }
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
            OpKind::Migration { .. } => {
                self.manage.migration_preview = Some(data);
            }
        }
    }

    /// Adopt the authoritative managed registry after any CLI transaction and
    /// refresh the usage view; the monitor owns the cache file itself.
    fn reload_after_manage(&mut self) {
        let fresh = app_settings::load_settings();
        self.settings.accounts = fresh.accounts;
        self.manage.omp_cache_loaded = false;
        // Committed rename/color drafts now match the fresh registry; drop
        // them so cards re-seed from authoritative values. Uncommitted edits
        // (draft differs) survive reloads.
        for (provider, accounts) in [
            (ProviderId::Claude, &self.settings.accounts.claude),
            (ProviderId::Codex, &self.settings.accounts.codex),
        ] {
            for profile in &accounts.profiles {
                let draft_key = Self::status_key(provider, &profile.id);
                if self
                    .manage
                    .rename
                    .get(&draft_key)
                    .is_some_and(|draft| *draft == profile.name)
                {
                    self.manage.rename.remove(&draft_key);
                }
                if self
                    .manage
                    .color_drafts
                    .get(&draft_key)
                    .is_some_and(|draft| *draft == profile.color)
                {
                    self.manage.color_drafts.remove(&draft_key);
                }
            }
        }
        if let Some(cache) = app_settings::load_usage_cache() {
            self.manage.last_snapshot_unix = Some(cache.updated_unix);
            self.update_usage_cache(cache);
        }
        self.notify_owner();
        let _ = self.request_live_usage_refresh();
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
}

/// Depth-limited generic JSON view for backend payloads whose exact shape is
/// owned by other slices (skills conflicts, migration reports, status).
pub(super) fn render_json_value(ui: &mut egui::Ui, value: &ManageValue, depth: usize) {
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
    use super::cards::{
        additional_limit, classify_omp_expiry, resolve_omp_email_hint, OmpExpiryState,
        OMP_EXPIRING_SOON_SECS,
    };
    use super::cleanup_ui::{
        cleanup_confirm_ready, cleanup_plan, external_default_reset_blocked, preview_targets,
    };
    use super::worker::{
        load_omp_statuses_from_profiles, manage_output_result, parse_manage_output,
        try_run_read_only_in_process,
    };
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
        let bare: ManageValue = serde_json::from_str(r#"{"targets":[],"backup_id":"b1"}"#).unwrap();
        assert_eq!(cleanup_plan(&bare), &bare);
    }

    #[test]
    fn cleanup_confirm_allows_metadata_only_delete() {
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
        let switched = CleanupPreviewKey {
            account: "b".into(),
            ..base.clone()
        };
        assert_ne!(base, switched);
        let rescoped = CleanupPreviewKey {
            scope: CleanupScopeSel::LocalData,
            ..base.clone()
        };
        assert_ne!(base, rescoped);
        let opted = CleanupPreviewKey {
            include_external: true,
            ..base.clone()
        };
        assert_ne!(base, opted);
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

    #[test]
    fn omp_import_status_message_formats_with_and_without_email() {
        let with_email =
            serde_json::json!({"imported_from_omp": true, "email": " user@example.com "});
        assert_eq!(
            omp_import_status_message(&with_email, LanguageId::English),
            "Operation completed: user@example.com"
        );
        let blank_email = serde_json::json!({"imported_from_omp": true, "email": "   "});
        assert_eq!(
            omp_import_status_message(&blank_email, LanguageId::English),
            "Operation completed"
        );
        let no_email = serde_json::json!({"imported_from_omp": true});
        assert_eq!(
            omp_import_status_message(&no_email, LanguageId::English),
            "Operation completed"
        );
    }

    #[test]
    fn omp_expiry_classification_covers_expired_soon_and_valid() {
        assert_eq!(classify_omp_expiry(100, 100), OmpExpiryState::Expired);
        assert_eq!(classify_omp_expiry(90, 100), OmpExpiryState::Expired);
        assert_eq!(
            classify_omp_expiry(100 + OMP_EXPIRING_SOON_SECS, 100),
            OmpExpiryState::ExpiringSoon(Duration::from_secs(OMP_EXPIRING_SOON_SECS))
        );
        assert_eq!(
            classify_omp_expiry(100 + OMP_EXPIRING_SOON_SECS + 1, 100),
            OmpExpiryState::Valid(Duration::from_secs(OMP_EXPIRING_SOON_SECS + 1))
        );
    }

    #[test]
    fn omp_email_hint_uses_cached_metadata_without_blocking_ui() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.json"),
            r#"{"usagehub_omp_email":"cached@example.com","usagehub_omp_expires_unix":1800000000}"#,
        )
        .unwrap();
        let profile = crate::accounts::AccountProfile {
            id: "account_1".into(),
            name: "Codex".into(),
            config_dir: dir.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let cache = load_omp_statuses_from_profiles(std::slice::from_ref(&profile));
        let cached = cache.get("account_1").unwrap();
        assert!(cached.is_omp_imported);
        assert_eq!(cached.expires_unix, Some(1_800_000_000));
        assert_eq!(
            resolve_omp_email_hint(&profile, Some(cached)).as_deref(),
            Some("cached@example.com")
        );

        let mut with_key = profile.clone();
        with_key.discovery_key = "omp:primary@example.com".into();
        assert_eq!(
            resolve_omp_email_hint(&with_key, Some(cached)).as_deref(),
            Some("primary@example.com")
        );
    }

    #[test]
    fn in_process_dispatch_only_handles_safe_read_only_commands() {
        assert!(try_run_read_only_in_process(&["discover".into(), "--apply".into()]).is_none());
        assert!(try_run_read_only_in_process(&[
            "backup".into(),
            "create".into(),
            "--provider".into(),
            "codex".into(),
            "--id".into(),
            "a1".into()
        ])
        .is_none());
        assert!(try_run_read_only_in_process(&["cleanup".into(), "apply".into()]).is_none());
        assert!(try_run_read_only_in_process(&[
            "signin".into(),
            "--provider".into(),
            "codex".into(),
            "--id".into(),
            "a1".into(),
            "--from-omp".into()
        ])
        .is_none());
        assert!(try_run_read_only_in_process(&[
            "status".into(),
            "--provider".into(),
            "claude".into(),
            "--id".into(),
            "default".into()
        ])
        .is_some());
        assert!(try_run_read_only_in_process(&[
            "backup".into(),
            "list".into(),
            "--provider".into(),
            "claude".into()
        ])
        .is_some());
    }
}
