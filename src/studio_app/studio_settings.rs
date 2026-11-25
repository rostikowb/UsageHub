use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use super::*;
use crate::poller::{
    self, CredentialSourceState, CredentialSourceStatus, PollError, ProviderTelemetry, TelemetryKey,
};
use crate::providers::ProviderId;

#[derive(Default)]
struct SettingsTelemetryUi {
    in_flight: HashSet<TelemetryKey>,
    probing_credentials: bool,
    last_credential_probe: Option<Instant>,
}

fn telemetry_ui_state() -> &'static Mutex<SettingsTelemetryUi> {
    static STATE: OnceLock<Mutex<SettingsTelemetryUi>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SettingsTelemetryUi::default()))
}

fn ensure_credential_probe(ctx: &egui::Context, accounts: &crate::accounts::AccountSettings) {
    const PROBE_INTERVAL: Duration = Duration::from_secs(15);
    let should_probe = {
        let mut state = telemetry_ui_state()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.probing_credentials {
            false
        } else if state
            .last_credential_probe
            .is_none_or(|last| last.elapsed() >= PROBE_INTERVAL)
        {
            state.probing_credentials = true;
            true
        } else {
            false
        }
    };
    if !should_probe {
        return;
    }
    let ctx = ctx.clone();
    let accounts = accounts.clone();
    std::thread::spawn(move || {
        poller::refresh_credential_statuses(&accounts);
        {
            let mut state = telemetry_ui_state()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.probing_credentials = false;
            state.last_credential_probe = Some(Instant::now());
        }
        ctx.request_repaint();
    });
}

fn trigger_credential_probe(ctx: &egui::Context, accounts: &crate::accounts::AccountSettings) {
    {
        let mut state = telemetry_ui_state()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.probing_credentials {
            state.last_credential_probe = None;
            return;
        }
        state.probing_credentials = true;
    }
    let ctx = ctx.clone();
    let accounts = accounts.clone();
    std::thread::spawn(move || {
        poller::refresh_credential_statuses(&accounts);
        {
            let mut state = telemetry_ui_state()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.probing_credentials = false;
            state.last_credential_probe = Some(Instant::now());
        }
        ctx.request_repaint();
    });
}

fn start_connection_test(
    ctx: &egui::Context,
    provider: ProviderId,
    profile: Option<crate::accounts::AccountProfile>,
) {
    let key = TelemetryKey::new(provider, profile.as_ref().map(|item| item.id.as_str()));
    {
        let mut state = telemetry_ui_state()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.in_flight.insert(key.clone()) {
            return;
        }
    }
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = match &profile {
            Some(profile) => poller::test_account_connection(provider, profile),
            None => poller::test_provider_connection(provider),
        };
        {
            let mut state = telemetry_ui_state()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.in_flight.remove(&key);
        }
        ctx.request_repaint();
    });
}

fn is_testing_connection(provider: ProviderId, account_id: Option<&str>) -> bool {
    let key = TelemetryKey::new(provider, account_id);
    telemetry_ui_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .in_flight
        .contains(&key)
}

impl StudioApp {
    pub(super) fn settings_page(&mut self, ui: &mut egui::Ui) {
        let language = self.language();
        ensure_credential_probe(ui.ctx(), &self.settings.accounts);
        let mut poll_interval_changed = false;
        let mut providers_changed = false;
        let mut display_changed = false;
        let mut accounts_changed = false;
        let mut requested_theme = None;
        let mut open_theme_studio = false;
        let themes = self.available_themes();
        if let Some(error) = &self.settings_error {
            ui.colored_label(egui::Color32::from_rgb(196, 64, 64), error);
            ui.add_space(8.0);
        }
        settings_scroll_area(ui, |ui| {
            // Account manager first: the dashboard opens directly on account
            // navigation. All mutations go through the --manage CLI surface.
            self.manage_section(ui);
            section(ui, language.text("General"), |ui| {
                setting_row(
                    ui,
                    language.text("Update frequency"),
                    language.text("How often provider usage is refreshed"),
                    |ui| {
                        ui.label(language.text("minutes"));
                        let mut minutes = self.settings.poll_interval_ms / POLL_1_MIN;
                        if ui
                            .push_id(
                                ("poll_interval", self.poll_interval_editor_generation),
                                |ui| {
                                    NumberField::new(&mut minutes)
                                        .range(1..=app_settings::MAX_POLL_MINUTES)
                                        .speed(1.0)
                                        .show(ui, 100.0)
                                },
                            )
                            .inner
                            .changed()
                        {
                            self.settings.poll_interval_ms = minutes * POLL_1_MIN;
                            poll_interval_changed = true;
                        }
                        if ui.button(language.text("Refresh now")).clicked() {
                            self.request_refresh();
                        }
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Start with Windows"),
                    language.text("Launch the monitor when you sign in"),
                    |ui| {
                        if Toggle::new(&mut self.startup_enabled)
                            .labels(language.text("Enabled"), language.text("Disabled"))
                            .show(ui)
                            .changed()
                        {
                            crate::window::set_startup_enabled(self.startup_enabled);
                        }
                    },
                );
            });
            section(ui, language.text("Providers"), |ui| {
                for (index, descriptor) in PROVIDER_DESCRIPTORS.iter().enumerate() {
                    if index > 0 {
                        setting_separator(ui);
                    }
                    let enabled = self.settings.provider_enabled(descriptor.id);
                    let default_account = self
                        .settings
                        .accounts
                        .get(descriptor.id)
                        .and_then(|accounts| accounts.selected().cloned());
                    let telemetry_account_id =
                        default_account.as_ref().map(|profile| profile.id.as_str());
                    let testing = is_testing_connection(descriptor.id, telemetry_account_id);
                    let telemetry = poller::provider_telemetry(descriptor.id, telemetry_account_id);
                    setting_row_with_mark(
                        ui,
                        provider_mark_glyph(descriptor.id),
                        language.text(descriptor.display_name),
                        language.text(descriptor.settings_description),
                        |ui| {
                            let mut toggle_enabled = enabled;
                            if Toggle::new(&mut toggle_enabled)
                                .labels(language.text("Enabled"), language.text("Disabled"))
                                .show(ui)
                                .changed()
                            {
                                providers_changed |= self.settings.toggle_provider(descriptor.id);
                            }
                            let test_label = if testing {
                                language.text("Testing...")
                            } else {
                                language.text("Test connection")
                            };
                            if ui
                                .add_enabled(!testing, egui::Button::new(test_label))
                                .clicked()
                            {
                                start_connection_test(
                                    ui.ctx(),
                                    descriptor.id,
                                    default_account.clone(),
                                );
                            }
                        },
                    );
                    let cached_usage = self.usage.as_ref().and_then(|data| data.get(descriptor.id));
                    let cached_error = self.usage.as_ref().and_then(|data| {
                        default_account.as_ref().and_then(|profile| {
                            data.accounts
                                .iter()
                                .find(|account| {
                                    account.provider == descriptor.id
                                        && account.profile.same_source(profile)
                                })
                                .and_then(|account| account.error)
                        })
                    });
                    render_telemetry_summary(
                        ui,
                        &telemetry,
                        enabled,
                        cached_usage,
                        cached_error,
                        language,
                    );
                    if descriptor.id.supports_account_profiles() {
                        let accounts = match descriptor.id {
                            ProviderId::Claude => &mut self.settings.accounts.claude,
                            _ => &mut self.settings.accounts.codex,
                        };
                        ui.push_id(descriptor.key, |ui| {
                            accounts_changed |= account_settings(
                                ui,
                                descriptor.id,
                                accounts,
                                self.usage.as_ref(),
                                enabled,
                                language,
                                self.owner,
                            );
                        });
                    }
                }
            });
            section(ui, language.text("Display"), |ui| {
                setting_row(
                    ui,
                    language.text("Usage direction"),
                    language.text("Count down what is left in supported themes"),
                    |ui| {
                        display_changed |= Toggle::new(&mut self.settings.usage_countdown)
                            .labels(language.text("Remaining"), language.text("Used"))
                            .show(ui)
                            .changed();
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Lock in taskbar"),
                    language.text("Keep the widget docked; never float automatically"),
                    |ui| {
                        display_changed |= Toggle::new(&mut self.settings.lock_taskbar)
                            .labels(language.text("Locked"), language.text("Floating"))
                            .show(ui)
                            .changed();
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Language"),
                    language.text("Language used by the app and widget"),
                    |ui| {
                        let language_code = self
                            .settings
                            .language
                            .get_or_insert_with(|| "system".into());
                        Dropdown::from_id_salt("language")
                            .width(220.0)
                            .selected_text(language_name(language, language_code))
                            .show_ui(ui, |ui| {
                                for (code, name) in languages(language) {
                                    display_changed |= dropdown_selectable_value(
                                        ui,
                                        language_code,
                                        code.into(),
                                        name,
                                    )
                                    .changed();
                                }
                            });
                    },
                );
            });
            section(ui, language.text("Appearance"), |ui| {
                setting_row(
                    ui,
                    language.text("Active theme"),
                    language.text("The widget is managed in Theme Studio"),
                    |ui| {
                        Dropdown::from_id_salt("active_theme")
                            .width(220.0)
                            .selected_text(&self.theme.name)
                            .show_ui(ui, |ui| {
                                for theme in themes.iter() {
                                    let selected =
                                        self.theme_path.as_deref() == Some(theme.path.as_path());
                                    if dropdown_selectable_label(ui, selected, &theme.label)
                                        .clicked()
                                    {
                                        requested_theme = Some(theme.path.clone());
                                    }
                                }
                            });
                    },
                );
                setting_separator(ui);
                setting_row(
                    ui,
                    language.text("Theme editor"),
                    language.text("Create and edit themes"),
                    |ui| {
                        if ui.button(language.text("Open Theme Studio")).clicked() {
                            open_theme_studio = true;
                        }
                    },
                );
            });
        });
        if poll_interval_changed || providers_changed || display_changed || accounts_changed {
            self.settings.accounts.claude.normalize();
            self.settings.accounts.codex.normalize();
            let edited = self.settings.clone();
            let mut merged_result = None;
            let save_result = crate::manager::state::mutate_settings(|persisted| {
                if poll_interval_changed {
                    persisted.poll_interval_ms = edited.poll_interval_ms;
                }
                if providers_changed {
                    persisted.set_enabled_providers(edited.enabled_providers());
                }
                if display_changed {
                    persisted.usage_countdown = edited.usage_countdown;
                    persisted.lock_taskbar = edited.lock_taskbar;
                    persisted.language = edited.language.clone();
                }
                if accounts_changed {
                    merge_provider_accounts(
                        &mut persisted.accounts.claude,
                        &edited.accounts.claude,
                    );
                    merge_provider_accounts(&mut persisted.accounts.codex, &edited.accounts.codex);
                }
                merged_result = Some(persisted.clone());
                Ok(())
            });
            match save_result {
                Ok(()) => {
                    if let Some(updated) = merged_result {
                        self.settings = updated;
                        self.synced_poll_interval_ms = self.settings.poll_interval_ms;
                        self.synced_taskbar_lock = self.settings.lock_taskbar;
                    }
                    if let Some(data) = self.usage.as_mut() {
                        data.select_accounts(&self.settings.accounts);
                    }
                    let new_language = self.language();
                    if new_language != language {
                        configure_style(ui.ctx(), new_language);
                    }
                    self.preview_dirty = true;
                    self.settings_error = None;
                    self.notify_owner();
                    if accounts_changed || providers_changed {
                        trigger_credential_probe(ui.ctx(), &self.settings.accounts);
                    }
                }
                Err(error) => {
                    self.settings_error = Some(format!(
                        "{}: {error}",
                        language.text("Unable to save settings")
                    ));
                }
            }
        }
        if let Some(path) = requested_theme {
            self.request_activate_theme(path);
        }
        if open_theme_studio {
            self.page = Page::Studio;
        }
    }
}

/// Merge Settings-page account changes onto freshly loaded settings under
/// `SettingsOperationGuard` so concurrent `--manage` edits to managed profiles
/// are never clobbered by a stale dashboard snapshot.
fn merge_provider_accounts(
    persisted: &mut crate::accounts::ProviderAccounts,
    edited: &crate::accounts::ProviderAccounts,
) {
    use crate::accounts::AccountOwnership;

    let edited_managed: std::collections::HashMap<&str, bool> = edited
        .profiles
        .iter()
        .filter(|profile| profile.ownership == AccountOwnership::Managed)
        .map(|profile| (profile.id.as_str(), profile.enabled))
        .collect();

    let mut merged_profiles = Vec::new();
    for mut profile in persisted.profiles.iter().cloned() {
        if profile.ownership == AccountOwnership::Managed {
            if let Some(enabled) = edited_managed.get(profile.id.as_str()) {
                profile.enabled = *enabled;
            }
            merged_profiles.push(profile);
        }
    }
    for profile in &edited.profiles {
        if profile.ownership != AccountOwnership::Managed {
            merged_profiles.push(profile.clone());
        }
    }

    persisted.profiles = merged_profiles;
    persisted.used_ids.extend(edited.used_ids.iter().cloned());
    if persisted
        .profiles
        .iter()
        .any(|profile| profile.enabled && profile.id == edited.selected)
    {
        persisted.selected = edited.selected.clone();
    }
    persisted.normalize();
}

fn account_settings(
    ui: &mut egui::Ui,
    provider: ProviderId,
    accounts: &mut crate::accounts::ProviderAccounts,
    data: Option<&AppUsageData>,
    provider_enabled: bool,
    language: LanguageId,
    owner: isize,
) -> bool {
    let mut changed = false;
    let header = egui::CollapsingHeader::new(language.text("Accounts"))
        .default_open(accounts.profiles.len() > 1);
    header.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(language.text("Default account"));
            let selected = accounts.selected().map(|p| p.name.as_str()).unwrap_or("None");
            Dropdown::from_id_salt("widget_account")
                .width(220.0)
                .selected_text(selected)
                .show_ui(ui, |ui| {
                    for profile in accounts.profiles.iter().filter(|p| p.enabled) {
                        changed |= dropdown_selectable_value(ui, &mut accounts.selected,
                            profile.id.clone(), &profile.name).changed();
                    }
                });
        });
        let mut remove = None;
        for (index, profile) in accounts.profiles.iter_mut().enumerate() {
            let managed = profile.ownership == crate::accounts::AccountOwnership::Managed;
            ui.push_id(profile.id.clone(), |ui| {
                ui.separator();
                let profile_snapshot = profile.clone();
                let testing = is_testing_connection(provider, Some(&profile.id));
                let telemetry = poller::provider_telemetry(provider, Some(&profile.id));
                ui.horizontal_wrapped(|ui| {
                    changed |= ui.checkbox(&mut profile.enabled, language.text("Monitor")).changed();
                    if managed {
                        ui.label(egui::RichText::new(&profile.name).strong());
                    } else {
                        let name = ui.add(crate::ui::components::text_field::singleline(&mut profile.name)
                            .desired_width(220.0).hint_text(language.text("Account name")));
                        changed |= name.lost_focus();
                        // Claude removals always go through the account manager's
                        // backup-verified cleanup preview; other providers keep
                        // their direct settings controls.
                        if !managed && provider != ProviderId::Claude && ui.button(language.text("Remove")).clicked() { remove = Some(index); }
                    }
                    let test_label = if testing {
                        language.text("Testing...")
                    } else {
                        language.text("Test connection")
                    };
                    if ui
                        .add_enabled(!testing, egui::Button::new(test_label))
                        .clicked()
                    {
                        start_connection_test(ui.ctx(), provider, Some(profile_snapshot.clone()));
                    }
                });
                if managed {
                    ui.weak(format!("{}: accounts.{}.{}", language.text("Theme binding"), provider.descriptor().key, profile.id));
                } else {
                    ui.label(language.text("Config directory"));
                    let directory = ui.add(crate::ui::components::text_field::singleline(&mut profile.config_dir)
                        .hint_text(if provider == ProviderId::Claude { "~/.claude-work" } else { "~/.codex-work" }));
                    changed |= directory.lost_focus();
                    ui.collapsing(language.text("Custom credentials file"), |ui| {
                        let file = ui.add(crate::ui::components::text_field::singleline(&mut profile.credentials_path)
                            .hint_text(language.text("Optional; overrides the config directory")));
                        changed |= file.lost_focus();
                        if ui.button(language.text("Browse...")).clicked() {
                            if let Some(path) = choose_file(owner, language.text("Select credentials file"), "JSON files\0*.json\0All files\0*.*\0\0") {
                                profile.credentials_path = path.to_string_lossy().into_owned();
                                changed = true;
                            }
                        }
                        ui.weak(format!("{}: accounts.{}.{}", language.text("Theme binding"), provider.descriptor().key, profile.id));
                    });
                }
                ui.collapsing(language.text("Account details and advanced actions"), |ui| {
                    match profile.credential_path(provider) {
                        Err(error) => { ui.colored_label(egui::Color32::from_rgb(196, 64, 64), error); }
                        Ok(None) => { ui.weak(if provider == ProviderId::Claude {
                            language.text("Uses CLAUDE_CONFIG_DIR when set; otherwise detects CLI, Desktop or WSL credentials.")
                        } else {
                            language.text("Uses CODEX_HOME when set; otherwise ~/.codex/auth.json.")
                        }); }
                        Ok(Some(path)) => { ui.weak(path.display().to_string()); }
                    }
                });
                let account_usage = data.and_then(|data| data.accounts.iter().find(|account| {
                    account.provider == provider && account.profile.same_source(profile)
                }));
                render_telemetry_summary(
                    ui,
                    &telemetry,
                    provider_enabled && profile.enabled,
                    account_usage.and_then(|account| account.usage.as_ref()),
                    account_usage.and_then(|account| account.error),
                    language,
                );
                if provider_enabled && profile.enabled {
                    if let Some(account) = account_usage {
                        if let Some(error) = account.error {
                            ui.colored_label(egui::Color32::from_rgb(196, 112, 32), account_error_message(error, language));
                        }
                        if let Some(usage) = &account.usage {
                            if usage.stale { ui.weak(language.text("Last known usage")); }
                            ui.horizontal_wrapped(|ui| {
                                for (label, section) in [("Session", &usage.session), ("Weekly", &usage.weekly)] {
                                    if section.available {
                                        ui.label(format!("{}: {:.0}%", language.text(label), section.percentage));
                                        if let Some(reset) = section.resets_at {
                                            let minutes = reset.duration_since(std::time::SystemTime::now()).unwrap_or_default().as_secs() / 60;
                                            ui.weak(format!("{} {}h {}m", language.text("Resets in"), minutes / 60, minutes % 60));
                                        }
                                    }
                                }
                                if let Some(credits) = &usage.credits {
                                    ui.label(format!("{}: {:.0}%", language.text("Credits"), credits.percentage));
                                }
                            });
                        } else if account.error.is_none() && telemetry.last_poll_at.is_none() {
                            ui.weak(language.text("Waiting for usage"));
                        }
                    } else if telemetry.last_poll_at.is_none() {
                        ui.weak(language.text("Waiting for usage"));
                    }
                }
            });
        }
        if let Some(index) = remove {
            accounts.profiles.remove(index);
            changed = true;
        }
        if ui.button(language.text("Add account")).clicked() {
            accounts.add();
            changed = true;
        }
    });
    changed
}

fn render_telemetry_summary(
    ui: &mut egui::Ui,
    telemetry: &ProviderTelemetry,
    enabled: bool,
    cached_usage: Option<&crate::models::UsageData>,
    cached_error: Option<PollError>,
    language: LanguageId,
) {
    ui.horizontal_wrapped(|ui| {
        if let Some(remaining) = telemetry.retry_after_remaining {
            let cooldown_text = format!(
                "{} · {}: {}",
                language.text("Rate limited"),
                language.text("Cooldown"),
                format_duration_short(remaining)
            );
            ui.colored_label(egui::Color32::from_rgb(196, 112, 32), cooldown_text);
        } else if let Some(error) = telemetry.last_error.or(cached_error) {
            let color = if error.is_auth() {
                egui::Color32::from_rgb(196, 64, 64)
            } else {
                egui::Color32::from_rgb(196, 112, 32)
            };
            ui.colored_label(
                color,
                format!(
                    "{}: {}",
                    language.text("Check failed"),
                    error.message(language)
                ),
            );
        } else if telemetry.last_poll_at.is_some() || cached_usage.is_some_and(|usage| !usage.stale)
        {
            ui.colored_label(
                egui::Color32::from_rgb(76, 175, 80),
                language.text("Connected"),
            );
        } else if cached_usage.is_some_and(|usage| usage.stale) {
            ui.weak(language.text("Last known usage"));
        } else if !enabled {
            ui.weak(language.text("Monitoring disabled"));
        } else {
            ui.weak(language.text("Not polled yet"));
        }

        if let Some(status) = &telemetry.credential_status {
            ui.weak("·");
            ui.weak(format!(
                "{}: {}",
                language.text("Credentials"),
                format_credential_status(status, language)
            ));
        } else if telemetry_ui_state()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .probing_credentials
        {
            ui.weak("·");
            ui.weak(format!(
                "{}: {}",
                language.text("Credentials"),
                language.text("Checking...")
            ));
        }

        if let Some(code) = telemetry.last_http_status {
            ui.weak("·");
            ui.weak(format!("HTTP {code}"));
        }
        if let Some(latency_ms) = telemetry.latency_ms {
            ui.weak("·");
            ui.weak(format!("{latency_ms} ms"));
        }
        if let Some(last_poll_at) = telemetry.last_poll_at {
            ui.weak("·");
            ui.weak(format!(
                "{}: {}",
                language.text("Last checked"),
                format_relative_time(last_poll_at, SystemTime::now(), language)
            ));
        }
    });
}

fn format_credential_status(status: &CredentialSourceStatus, language: LanguageId) -> String {
    let state = match status.state {
        CredentialSourceState::Detected => language.text("Detected"),
        CredentialSourceState::Expired => language.text("Expired"),
        CredentialSourceState::Missing => language.text("Missing"),
        CredentialSourceState::Invalid => language.text("Invalid"),
    };
    let source = match status.source_label {
        "Credentials file" => language.text("Credentials file"),
        "Desktop directory" => language.text("Desktop directory"),
        "Environment variable" => language.text("Environment variable"),
        "Windows Credential Manager" => language.text("Windows Credential Manager"),
        "Local database" => language.text("Local database"),
        "Config file" => language.text("Config file"),
        "WSL credentials" => language.text("WSL credentials"),
        "Invalid path" => language.text("Invalid path"),
        _ => language.text("Not found"),
    };
    if status.state == CredentialSourceState::Missing && status.source_label == "Not found" {
        source.to_string()
    } else {
        format!("{state} ({source})")
    }
}

fn format_duration_short(duration: Duration) -> String {
    let secs = duration.as_secs().max(1);
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn format_relative_time(timestamp: SystemTime, now: SystemTime, language: LanguageId) -> String {
    let elapsed = now.duration_since(timestamp).unwrap_or_default().as_secs();
    if elapsed < 5 {
        language.text("Just now").to_string()
    } else if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3_600 {
        format!("{}m", elapsed / 60)
    } else {
        format!("{}h", elapsed / 3_600)
    }
}

fn account_error_message(error: crate::poller::PollError, language: LanguageId) -> String {
    error.message(language)
}

#[cfg(test)]
mod account_status_tests {
    use super::*;

    #[test]
    fn status_explains_http_failure_and_correct_recovery_action() {
        use crate::poller::PollError;
        assert_eq!(
            account_error_message(PollError::HttpStatus(429), LanguageId::English),
            "HTTP 429: Too Many Requests. Retrying at the next refresh"
        );
        assert_eq!(
            account_error_message(PollError::HttpStatus(401), LanguageId::English),
            "HTTP 401: Unauthorized. Sign in again for this account"
        );
        assert!(
            account_error_message(PollError::HttpStatus(503), LanguageId::English)
                .contains("Service Unavailable")
        );
    }

    #[test]
    fn telemetry_formatting_helpers_are_non_secret_and_human_readable() {
        let cred = CredentialSourceStatus {
            state: CredentialSourceState::Detected,
            source_label: "Credentials file",
        };
        assert_eq!(
            format_credential_status(&cred, LanguageId::English),
            "Detected (Credentials file)"
        );
        assert_eq!(format_duration_short(Duration::from_secs(75)), "1m 15s");
        assert_eq!(format_duration_short(Duration::from_millis(250)), "1s");
        let now = SystemTime::now();
        assert_eq!(
            format_relative_time(now, now, LanguageId::English),
            "Just now"
        );
        assert_eq!(
            format_relative_time(now - Duration::from_secs(42), now, LanguageId::English),
            "42s"
        );
    }

    #[test]
    fn merge_provider_accounts_preserves_concurrent_managed_updates() {
        use crate::accounts::{AccountOwnership, AccountProfile, ProviderAccounts};

        let mut persisted = ProviderAccounts::default();
        persisted.profiles.push(AccountProfile {
            id: "account_1".into(),
            name: "Managed One".into(),
            ownership: AccountOwnership::Managed,
            config_dir: "C:\\managed\\one".into(),
            discovery_key: "omp:user@example.com".into(),
            ..Default::default()
        });
        persisted.profiles.push(AccountProfile {
            id: "account_2".into(),
            name: "Managed Two (new)".into(),
            ownership: AccountOwnership::Managed,
            config_dir: "C:\\managed\\two".into(),
            ..Default::default()
        });
        persisted.used_ids.insert("account_1".into());
        persisted.used_ids.insert("account_2".into());

        // Stale dashboard edit before account_2 existed, renaming Default external
        // profile and disabling account_1.
        let mut edited = ProviderAccounts::default();
        edited.profiles[0].name = "Renamed Default".into();
        edited.profiles.push(AccountProfile {
            id: "account_1".into(),
            name: "Stale Name".into(),
            ownership: AccountOwnership::Managed,
            enabled: false,
            ..Default::default()
        });
        edited.selected = "default".into();

        merge_provider_accounts(&mut persisted, &edited);

        assert!(persisted.find("account_2").is_some());
        let managed_one = persisted.find("account_1").unwrap();
        assert_eq!(managed_one.name, "Managed One");
        assert_eq!(managed_one.discovery_key, "omp:user@example.com");
        assert!(!managed_one.enabled);
        assert_eq!(persisted.find("default").unwrap().name, "Renamed Default");
    }
}
