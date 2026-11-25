//! Account cleanup preview and confirmation modal UI.

use eframe::egui;

use super::{
    human_bytes, str_field, CleanupPreviewKey, CleanupScopeSel, ManageState, ManageValue, OpKind,
};
use crate::localization::LanguageId;
use crate::providers::ProviderId;
use crate::studio_app::StudioApp;

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

/// Backend preview nests the actionable plan under `plan`
/// (`{provider, account_id, plan: {targets, warnings, backup_id, ...}}`);
/// tolerate a bare plan object for forward compatibility.
pub(super) fn cleanup_plan<'a>(preview: &'a ManageValue) -> &'a ManageValue {
    preview.get("plan").unwrap_or(preview)
}

/// UI-selected preview identity: provider tab, account combo, scope radios
/// and the external-Default opt-in. Any change invalidates the review.
pub(super) fn current_cleanup_key(manage: &ManageState, provider: &str) -> CleanupPreviewKey {
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
pub(super) fn external_default_reset_blocked(
    scope: CleanupScopeSel,
    is_external_default: bool,
    opt_in: bool,
) -> bool {
    scope == CleanupScopeSel::FullReset && is_external_default && !opt_in
}

pub(super) fn preview_targets(plan: &ManageValue) -> &[ManageValue] {
    plan.get("targets")
        .and_then(|targets| targets.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

impl StudioApp {
    pub(super) fn cleanup_ui(
        &mut self,
        ui: &mut egui::Ui,
        _provider: ProviderId,
        language: LanguageId,
    ) {
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
}
