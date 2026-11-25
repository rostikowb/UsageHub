//! Legacy install migration and legacy backup disposition UI.

use eframe::egui;

use super::{render_json_value, LegacyAction, OpKind};
use crate::localization::LanguageId;
use crate::studio_app::StudioApp;

impl StudioApp {
    pub(super) fn migration_ui(&mut self, ui: &mut egui::Ui, language: LanguageId) {
        ui.collapsing(language.text("Migration"), |ui| {
            ui.horizontal_wrapped(|ui| {
                let busy = self.manage_busy();
                for (action, label, refresh_usage) in [
                    ("preview", language.text("Preview"), false),
                    ("import", language.text("Import..."), true),
                ] {
                    if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                        self.spawn_manage(
                            OpKind::Migration { refresh_usage },
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
                        OpKind::Migration {
                            refresh_usage: true,
                        },
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
                        OpKind::Migration {
                            refresh_usage: true,
                        },
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
