//! Encrypted backup inventory, creation, verification, restore, and purge UI.

use eframe::egui;

use super::{format_unix_utc, human_bytes, str_field, ManageValue, OpKind};
use crate::localization::LanguageId;
use crate::providers::ProviderId;
use crate::studio_app::StudioApp;

impl StudioApp {
    /// Visible inventory filter: the provider tab plus the account id field.
    /// The empty account intentionally lists every account, including a
    /// deleted one whose backup is the restore/undo path; the filter is never
    /// resolved against the current profiles.
    pub(super) fn backup_filter(&self) -> (String, String) {
        (
            Self::provider_key(self.manage.provider_tab).to_string(),
            self.manage.backup_account.clone(),
        )
    }

    pub(super) fn refresh_backup_list(&mut self, provider: String, account: String) {
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

    pub(super) fn backups_ui(
        &mut self,
        ui: &mut egui::Ui,
        provider: ProviderId,
        language: LanguageId,
    ) {
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
}
