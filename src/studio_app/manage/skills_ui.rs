//! Shared skills library import UI.

use eframe::egui;

use super::{render_json_value, OpKind};
use crate::localization::LanguageId;
use crate::studio_app::{choose_file, StudioApp};

impl StudioApp {
    pub(super) fn skills_ui(&mut self, ui: &mut egui::Ui, language: LanguageId) {
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
}
