//! Account cards, live usage display, and Codex/omp sign-in controls.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui;

use super::worker::{ensure_omp_cache_refresh, OmpProfileStatus};
use super::{format_countdown, format_unix_utc, reset_display, OpKind};
use crate::accounts::{AccountOwnership, AccountProfile};
use crate::localization::LanguageId;
use crate::providers::ProviderId;
use crate::studio_app::StudioApp;
use crate::ui::theme::muted;

/// Warn proactively when an imported omp access token expires within this window.
pub(super) const OMP_EXPIRING_SOON_SECS: u64 = 3_600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OmpExpiryState {
    Expired,
    ExpiringSoon(Duration),
    Valid(Duration),
}

pub(super) fn classify_omp_expiry(expires_unix: u64, now_unix: u64) -> OmpExpiryState {
    if expires_unix <= now_unix {
        OmpExpiryState::Expired
    } else {
        let remaining = Duration::from_secs(expires_unix - now_unix);
        if remaining.as_secs() <= OMP_EXPIRING_SOON_SECS {
            OmpExpiryState::ExpiringSoon(remaining)
        } else {
            OmpExpiryState::Valid(remaining)
        }
    }
}

pub(super) fn resolve_omp_email_hint(
    profile: &AccountProfile,
    cached: Option<&OmpProfileStatus>,
) -> Option<String> {
    profile
        .discovery_key
        .strip_prefix("omp:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| cached.and_then(|info| info.email.clone()))
        .or_else(|| {
            let trimmed = profile.name.trim();
            if !trimmed.is_empty() && trimmed.contains('@') {
                Some(trimmed.to_string())
            } else {
                None
            }
        })
}

pub(super) fn additional_limit(
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

impl StudioApp {
    fn trigger_omp_import(&mut self, provider_key: &str, profile: &AccountProfile) {
        let mut argv = vec![
            "signin".to_string(),
            "--provider".to_string(),
            provider_key.to_string(),
            "--id".to_string(),
            profile.id.clone(),
            "--from-omp".to_string(),
        ];
        let cached = self.manage.omp_statuses.get(&profile.id);
        if let Some(email) = resolve_omp_email_hint(profile, cached) {
            argv.push("--email".to_string());
            argv.push(email);
        }
        self.spawn_manage(OpKind::Simple, argv);
    }

    pub(super) fn account_cards(
        &mut self,
        ui: &mut egui::Ui,
        provider: ProviderId,
        language: LanguageId,
    ) {
        let key = Self::provider_key(provider);
        let profiles: Vec<AccountProfile> = self
            .settings
            .accounts
            .get(provider)
            .map(|accounts| accounts.ordered().into_iter().cloned().collect())
            .unwrap_or_default();
        if provider == ProviderId::Codex {
            ensure_omp_cache_refresh(&mut self.manage, &profiles);
        }
        let now = SystemTime::now();
        let now_unix = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
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
                if provider == ProviderId::Codex && profile.ownership == AccountOwnership::Managed {
                    self.render_omp_expiry_indicator(ui, key, profile, language, now_unix);
                }
                let desktop_available = provider != ProviderId::Codex
                    || (id.eq_ignore_ascii_case("default")
                        && profile.ownership != AccountOwnership::Managed);
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
                        && profile.ownership == AccountOwnership::Managed
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
                            self.trigger_omp_import(key, profile);
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
                            if profile.ownership == AccountOwnership::Managed {
                                super::CleanupScopeSel::LocalData
                            } else {
                                super::CleanupScopeSel::FullReset
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
                if let Some(line) = self
                    .manage
                    .status
                    .get(&Self::status_key(provider, &id))
                    .cloned()
                {
                    ui.weak(line);
                }
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
                        // Rename + pin/order/color: ordinary display edits, no confirmation.
                        let draft_key = Self::status_key(provider, &id);
                        ui.horizontal_wrapped(|ui| {
                            let buffer = self
                                .manage
                                .rename
                                .entry(draft_key.clone())
                                .or_insert_with(|| profile.name.clone());
                            ui.add(
                                crate::ui::components::text_field::singleline(buffer)
                                    .desired_width(180.0),
                            );
                            if ui.button(language.text("Rename account")).clicked() {
                                let name = self
                                    .manage
                                    .rename
                                    .get(&draft_key)
                                    .cloned()
                                    .unwrap_or_default();
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
                                    .entry(draft_key.clone())
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
                                    .get(&draft_key)
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

    fn render_omp_expiry_indicator(
        &mut self,
        ui: &mut egui::Ui,
        provider_key: &str,
        profile: &AccountProfile,
        language: LanguageId,
        now_unix: u64,
    ) {
        let Some(cached) = self.manage.omp_statuses.get(&profile.id) else {
            return;
        };
        if !cached.is_omp_imported && !profile.discovery_key.starts_with("omp:") {
            return;
        }
        let Some(expires_unix) = cached.expires_unix else {
            return;
        };
        match classify_omp_expiry(expires_unix, now_unix) {
            OmpExpiryState::Expired => {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(196, 64, 64),
                        language.text("omp token expired"),
                    );
                    if ui
                        .add_enabled(
                            !self.manage_busy(),
                            egui::Button::new(language.text("Re-import from omp")),
                        )
                        .on_hover_text(language.text(
                            "Reuses an existing omp ChatGPT sign-in. UsageHub never refreshes omp's token.",
                        ))
                        .clicked()
                    {
                        self.trigger_omp_import(provider_key, profile);
                    }
                });
            }
            OmpExpiryState::ExpiringSoon(remaining) => {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(196, 112, 32),
                        format!(
                            "{}: {}",
                            language.text("omp token expires in"),
                            format_countdown(language, remaining)
                        ),
                    );
                    if ui
                        .add_enabled(
                            !self.manage_busy(),
                            egui::Button::new(language.text("Re-import from omp")),
                        )
                        .on_hover_text(language.text(
                            "Reuses an existing omp ChatGPT sign-in. UsageHub never refreshes omp's token.",
                        ))
                        .clicked()
                    {
                        self.trigger_omp_import(provider_key, profile);
                    }
                });
            }
            OmpExpiryState::Valid(remaining) => {
                ui.weak(format!(
                    "{}: {}",
                    language.text("omp token expires in"),
                    format_countdown(language, remaining)
                ));
            }
        }
    }

    fn account_usage_lines(
        &self,
        ui: &mut egui::Ui,
        provider: ProviderId,
        profile: &AccountProfile,
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
        profile: &AccountProfile,
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
}
