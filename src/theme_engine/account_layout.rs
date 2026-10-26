//! Dynamic per-account widget columns.
//!
//! The widget repeats one column (or row slot) per enabled + pinned
//! [`AccountProfile`][crate::accounts::AccountProfile], driven entirely by the
//! settings registry. No static `accounts.count` binding or hand-written JSON
//! is needed: a third pinned account appears as soon as settings reload.
//!
//! Expansion applies to marker-based themes, built-in classic themes, and the
//! imported `left-dock-usage` classic layout. Other custom themes are unchanged.
//!
//! Built-in single-account slots can retain their provider-level bindings.
//! Slots are retained even when usage has not yet been polled.

use std::collections::HashMap;

use super::*;
use crate::accounts::AccountSettings;
use crate::models::AppUsageData;
use crate::providers::{ProviderId, PROVIDER_DESCRIPTORS};

/// Marker container a custom theme can include to opt in to
/// dynamic per-account columns. It must use a `Row` (or `Column`) layout; the
/// expander fills it with one generated card per visible account.
pub const ACCOUNT_ROW_OBJECT_ID: &str = "usagehub-accounts-row";
/// Hidden template card cloned once per visible account inside an opt-in row.
/// Its bindings use the singular `account.` placeholder prefix, rewritten to
/// `accounts.<provider>.<id>.` per clone. Never rendered itself (`render 0`).
pub const ACCOUNT_TEMPLATE_OBJECT_ID: &str = "usagehub-account-template";
/// Generated overflow chip id (single per row). Clicking it opens the native
/// account chooser instead of growing the bar.
pub const ACCOUNT_OVERFLOW_OBJECT_ID: &str = "usagehub-accounts-overflow";
/// Prefix for generated card object ids.
pub const ACCOUNT_CARD_ID_PREFIX: &str = "usagehub-account--";

/// Default card geometry (96-DPI logical pixels) used when the template (or
/// cloned provider group) does not supply its own width.
pub const ACCOUNT_CARD_WIDTH: f64 = 96.0;
pub const ACCOUNT_CARD_GAP: f64 = 3.0;
/// Chrome kept beside the row (session/weekly labels in the classic family).
pub const ACCOUNT_ROW_CHROME: f64 = 42.0;
pub const ACCOUNT_OVERFLOW_WIDTH: f64 = 30.0;
/// Empty-state affordance width: the bar never renders blank when no account
/// is pinned yet.
pub const MANAGE_CHIP_WIDTH: f64 = 118.0;
/// Fallback bar budget when shell geometry is unavailable. The native
/// collision pass still ejects an over-wide bar to floating; this cap only
/// decides how many cards materialize before the overflow chip takes over.
pub const ACCOUNT_MAX_WIDTH_FALLBACK: f64 = 520.0;
/// Maximum cards ever materialized on the bar; the remainder always overflows
/// to the account chooser popup so the widget can never cover Start/icons.
pub const ACCOUNT_MAX_VISIBLE_CARDS: usize = 8;

/// One dynamic widget slot: an enabled + pinned account profile in display
/// order. `selected`/`has_error`/`busy` are overlaid from the latest poller
/// snapshot; the slot itself exists independently of polling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountCard {
    pub provider: ProviderId,
    pub id: String,
    pub name: String,
    pub label: String,
    pub color: String,
    pub selected: bool,
    pub has_error: bool,
    /// A per-account operation (poll job, launch, deletion) currently holds
    /// this account's operation guard. Shown as a generic
    /// «оновлення/операція» badge; the slot is retained.
    pub busy: bool,
}

impl AccountCard {
    /// Theme binding root for this account, matching the `accounts.<provider>.
    /// <id>.*` keys published by [`DataContext`].
    pub fn binding_key(&self) -> String {
        format!("accounts.{}.{}", self.provider.descriptor().key, self.id)
    }

    /// Non-empty display label: explicit label, else the profile name.
    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            &self.label
        }
    }

    /// Account color if customized, else the provider default.
    pub fn display_color(&self) -> &str {
        if self.color.trim().is_empty() {
            default_provider_color(self.provider)
        } else {
            &self.color
        }
    }
}

fn default_provider_color(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::Claude => "#D97757",
        ProviderId::Codex => "#F5F5F5",
        _ => "#9AA0A6",
    }
}

/// Account providers shown as dynamic widget cards. The settings registry only
/// tracks Claude/Codex account lists; every other provider keeps its existing
/// provider-level presentation.
pub const ACCOUNT_CARD_PROVIDERS: [ProviderId; 2] = [ProviderId::Claude, ProviderId::Codex];

/// Collect enabled + pinned profiles in display order (provider descriptor
/// order, then [`ProviderAccounts::ordered`][crate::accounts::ProviderAccounts]).
/// Usage/error/selection overlay comes from `data` when available; cards are
/// emitted regardless so error and not-yet-polled slots stay stable.
pub fn visible_account_cards(
    settings: &AccountSettings,
    data: Option<&AppUsageData>,
) -> Vec<AccountCard> {
    let mut cards = Vec::new();
    for provider in ACCOUNT_CARD_PROVIDERS {
        let Some(configured) = settings.get(provider) else {
            continue;
        };
        for profile in configured.ordered() {
            if !profile.enabled || !profile.pinned {
                continue;
            }
            let (selected, has_error) = data
                .map(|data| {
                    data.accounts
                        .iter()
                        .find(|account| {
                            account.provider == provider && account.profile.id == profile.id
                        })
                        .map(|account| (account.selected, account.error.is_some()))
                        .unwrap_or((false, false))
                })
                .unwrap_or((false, false));
            cards.push(AccountCard {
                provider,
                id: profile.id.clone(),
                name: profile.name.clone(),
                label: profile.label.clone(),
                color: profile.color.clone(),
                selected,
                has_error,
                busy: false,
            });
        }
    }
    cards
}

/// Mark cards whose per-account operation guard is currently held (poll job,
/// launch, or deletion in flight). The slot stays; only the badge changes.
pub fn mark_busy_cards(cards: &mut [AccountCard], is_busy: impl Fn(ProviderId, &str) -> bool) {
    for card in cards {
        card.busy = is_busy(card.provider, &card.id);
    }
}

/// Partition cards into bar-visible and overflow sets against a width budget.
/// At least one card stays visible when any exist; the rest (plus any beyond
/// [`ACCOUNT_MAX_VISIBLE_CARDS`]) go to the overflow account chooser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountLayout {
    pub visible: Vec<AccountCard>,
    pub overflow: Vec<AccountCard>,
}

pub fn layout_accounts(cards: &[AccountCard], max_width: f64) -> AccountLayout {
    let max_width = if max_width.is_finite() && max_width > 0.0 {
        max_width
    } else {
        ACCOUNT_MAX_WIDTH_FALLBACK
    };
    if cards.is_empty() {
        return AccountLayout {
            visible: Vec::new(),
            overflow: Vec::new(),
        };
    }
    // Use account-provider count for the shared, conservative card estimate.
    // The imported classic layout also checks its resolved surface width.
    let provider_count = ACCOUNT_CARD_PROVIDERS
        .iter()
        .filter(|provider| cards.iter().any(|card| card.provider == **provider))
        .count()
        .max(1);
    let classic_width = ((10 + provider_count - 1) / provider_count * 11 - 1 + 4 + 62) as f64;
    let card_step = ACCOUNT_CARD_WIDTH.max(classic_width) + ACCOUNT_CARD_GAP;
    let mut capacity =
        ((max_width - ACCOUNT_ROW_CHROME + ACCOUNT_CARD_GAP) / card_step).floor() as usize;
    capacity = capacity.clamp(1, ACCOUNT_MAX_VISIBLE_CARDS.min(cards.len().max(1)));
    // Reserve the overflow chip when not everything fits.
    if cards.len() > capacity {
        let with_chip = ((max_width - ACCOUNT_ROW_CHROME - ACCOUNT_OVERFLOW_WIDTH
            + ACCOUNT_CARD_GAP)
            / card_step)
            .floor() as usize;
        capacity = with_chip.clamp(1, capacity);
    }
    let capacity = capacity.min(cards.len());
    AccountLayout {
        visible: cards[..capacity].to_vec(),
        overflow: cards[capacity..].to_vec(),
    }
}

/// Whether the theme opts in to (or is eligible for) dynamic account columns:
/// it carries the row marker/template, or it is a built-in theme whose
/// provider groups may be cloned.
pub fn is_account_aware_theme(theme: &ThemeDocument) -> bool {
    if is_builtin_theme_id(theme.id.as_str()) || is_left_dock_classic(theme) {
        return true;
    }
    theme.surfaces.iter().any(|surface| {
        surface.id == ACCOUNT_ROW_OBJECT_ID
            || surface.children.iter().any(|child| {
                child.id == ACCOUNT_ROW_OBJECT_ID || child.id == ACCOUNT_TEMPLATE_OBJECT_ID
            })
    })
}

fn is_builtin_theme_id(id: &str) -> bool {
    matches!(
        id,
        crate::theme_engine::CLASSIC_THEME_ID
            | crate::theme_engine::COMPACT_FLUENT_QUAD_THEME_ID
            | crate::theme_engine::CLASSIC_VERTICAL_THEME_ID
    )
}

// Restrict the legacy flat-slot adaptation to the known imported layout.
pub(crate) fn is_left_dock_classic(theme: &ThemeDocument) -> bool {
    theme.id == "left-dock-usage"
        && theme.surfaces.iter().any(|surface| {
            surface.id == "main"
                && [
                    "provider-row",
                    "claude-provider",
                    "second-provider",
                    "codex-provider",
                ]
                .iter()
                .all(|id| surface.children.iter().any(|child| child.id == *id))
        })
}

/// Expand the active theme for the current layout. Non-aware custom themes
/// are returned unchanged (their surface widths, bindings, and look are
/// preserved verbatim); aware themes materialize exactly the visible cards
/// plus the overflow chip when needed.
pub fn expand_theme_for_accounts(theme: &ThemeDocument, layout: &AccountLayout) -> ThemeDocument {
    if !is_account_aware_theme(theme) {
        return theme.clone();
    }
    if is_left_dock_classic(theme) {
        return expand_left_dock_classic(theme, layout);
    }
    if theme.surfaces.iter().any(|surface| {
        surface.id == ACCOUNT_ROW_OBJECT_ID
            || surface
                .children
                .iter()
                .any(|child| child.id == ACCOUNT_ROW_OBJECT_ID)
    }) {
        return expand_marker_theme(theme, layout);
    }
    expand_builtin_theme(theme, layout)
}

// The imported original has a hand-authored second Claude subtree interleaved
// with the first. Replace those runtime slots with clones of the authored
// provider subtrees, without changing the document on disk.
fn expand_left_dock_classic(theme: &ThemeDocument, layout: &AccountLayout) -> ThemeDocument {
    let mut expanded = theme.clone();
    for surface in &mut expanded.surfaces {
        if surface.id != "main" {
            continue;
        }
        let original = surface.children.clone();
        let mut removed = vec![
            "claude-provider".to_string(),
            "second-provider".to_string(),
            "codex-provider".to_string(),
        ];
        let mut cursor = 0;
        while cursor < removed.len() {
            let parent = removed[cursor].clone();
            for child in &original {
                if child.parent.as_deref() == Some(parent.as_str()) && !removed.contains(&child.id)
                {
                    removed.push(child.id.clone());
                }
            }
            cursor += 1;
        }
        let mut roots = Vec::new();
        let mut descendants = Vec::new();
        for object in &original {
            let provider = match object.id.as_str() {
                "claude-provider" => Some(ProviderId::Claude),
                "codex-provider" => Some(ProviderId::Codex),
                _ => None,
            };
            if let Some(provider) = provider {
                let key = provider.descriptor().key;
                let mut subtree_ids = vec![object.id.clone()];
                let mut index = 0;
                while index < subtree_ids.len() {
                    let parent = subtree_ids[index].clone();
                    for child in &original {
                        if child.parent.as_deref() == Some(parent.as_str())
                            && !subtree_ids.contains(&child.id)
                        {
                            subtree_ids.push(child.id.clone());
                        }
                    }
                    index += 1;
                }
                for card in layout
                    .visible
                    .iter()
                    .filter(|card| card.provider == provider)
                {
                    let root_id = format!("usagehub-classic-{key}-{}", card.id);
                    let remap: HashMap<String, String> = subtree_ids
                        .iter()
                        .enumerate()
                        .map(|(index, id)| {
                            (
                                id.clone(),
                                if index == 0 {
                                    root_id.clone()
                                } else {
                                    format!("{root_id}--{index}")
                                },
                            )
                        })
                        .collect();
                    for source in original
                        .iter()
                        .filter(|child| subtree_ids.contains(&child.id))
                    {
                        let mut clone = source.clone();
                        clone.id = remap[&source.id].clone();
                        if source.id == object.id {
                            let mut events = clone.mouse_events.take().unwrap_or_default();
                            events.click = format!("open_account(\"{key}\", \"{}\")", card.id);
                            clone.mouse_events = Some(events);
                        } else if let Some(parent) = source.parent.as_ref() {
                            clone.parent = remap.get(parent).cloned();
                        }
                        rewrite_subtree_strings(&mut clone, key, &card.binding_key());
                        if source.id == object.id {
                            roots.push(clone);
                        } else {
                            descendants.push(clone);
                        }
                    }
                }
            } else if !removed.contains(&object.id) {
                roots.push(object.clone());
            }
        }
        if !layout.overflow.is_empty() {
            roots.push(overflow_chip(
                "provider-row",
                layout.overflow.len(),
                ACCOUNT_CARD_GAP,
            ));
        }
        roots.extend(descendants);
        surface.children = roots;
        let claude = layout
            .visible
            .iter()
            .filter(|card| card.provider == ProviderId::Claude)
            .count();
        let codex = layout
            .visible
            .iter()
            .filter(|card| card.provider == ProviderId::Codex)
            .count();
        let count = format!("(providers.count - providers.claude.enabled - providers.codex.enabled + {claude} * providers.claude.enabled + {codex} * providers.codex.enabled)");
        let extra = if layout.overflow.is_empty() {
            0.0
        } else {
            ACCOUNT_OVERFLOW_WIDTH + ACCOUNT_CARD_GAP
        };
        surface.width = Expression(format!(
            "42 + {count} * (ceil(10 / max(1, providers.count)) * 11 - 1 + 4 + 62) + max(0, {count} - 1) * 3 + (providers.opencode.enabled + providers.cursor.enabled) * 28 + {extra}"
        ));
    }
    expanded
}

// ---------------------------------------------------------------------------
// Opt-in marker path for custom themes with the row
// ---------------------------------------------------------------------------

/// Materialize one card per visible account inside the marker row, then floor
/// the row and its surface to the generated extent so later cards are not
/// clipped by the authored single-card bounds.
fn expand_marker_theme(theme: &ThemeDocument, layout: &AccountLayout) -> ThemeDocument {
    let mut expanded = theme.clone();
    for surface in &mut expanded.surfaces {
        if !surface
            .children
            .iter()
            .any(|child| child.id == ACCOUNT_ROW_OBJECT_ID)
        {
            continue;
        }
        // Drop previously generated cards/chip; keep the authored template.
        surface.children.retain(|child| {
            !child.id.starts_with(ACCOUNT_CARD_ID_PREFIX) && child.id != ACCOUNT_OVERFLOW_OBJECT_ID
        });
        let Some(row_index) = surface
            .children
            .iter()
            .position(|child| child.id == ACCOUNT_ROW_OBJECT_ID)
        else {
            continue;
        };
        let template_index = surface
            .children
            .iter()
            .position(|child| child.id == ACCOUNT_TEMPLATE_OBJECT_ID);
        let row_id = surface.children[row_index].id.clone();
        // Card width comes from the authored template (or the default); the
        // generated set below sizes the row/surface from it.
        let card_width = template_index
            .and_then(|index| {
                evaluate_width_literal(
                    &surface.children[index].width,
                    &surface.children[index].name,
                )
            })
            .unwrap_or(ACCOUNT_CARD_WIDTH);
        let gap = evaluate_width_literal(&surface.children[row_index].gap, "gap")
            .unwrap_or(ACCOUNT_CARD_GAP);
        let mut generated: Vec<SceneObject> = Vec::new();
        for card in layout.visible.iter() {
            if let Some(template_index) = template_index {
                let template = surface.children[template_index].clone();
                generated.extend(instantiate_template_card(
                    &template,
                    &surface.children,
                    card,
                    &row_id,
                ));
            } else {
                generated.extend(default_template_card(card, &row_id, card_width));
            }
        }
        if !layout.overflow.is_empty() {
            generated.push(overflow_chip(&row_id, layout.overflow.len(), gap));
        }
        // Empty registry state keeps a visible affordance instead of a blank
        // bar: pin accounts from Manage accounts to populate slots.
        if layout.visible.is_empty() && layout.overflow.is_empty() {
            generated.push(manage_accounts_chip(&row_id));
        }
        surface.children.extend(generated);
        // Both the row and its surface clip their children at render time, so
        // a generated set wider than the authored single card must widen both
        // rectangles; otherwise the third and later cards are cropped. The
        // content extent mirrors the managed layout: a row accumulates widths
        // plus gaps, a column only needs its widest child.
        let mut child_widths = vec![card_width; layout.visible.len()];
        if !layout.overflow.is_empty() {
            child_widths.push(ACCOUNT_OVERFLOW_WIDTH);
        }
        if layout.visible.is_empty() && layout.overflow.is_empty() {
            child_widths.push(MANAGE_CHIP_WIDTH);
        }
        let extent = match surface.children[row_index].layout {
            ChildLayout::Row => {
                child_widths.iter().sum::<f64>() + gap * child_widths.len().saturating_sub(1) as f64
            }
            ChildLayout::Column => child_widths.iter().copied().fold(0.0, f64::max),
            ChildLayout::Freeform => 0.0,
        };
        // Authored padding (surface inset around the row) and offsets stay as
        // designed: only the rectangles are floored, never narrowed or moved.
        // Freeform rows place children explicitly, so they keep their bounds.
        if extent > 0.0 {
            let row = &surface.children[row_index];
            let row_offset = evaluate_width_literal(&row.x, "row x").unwrap_or(0.0);
            let authored_row = evaluate_width_literal(&row.width, "row width");
            let row_target = authored_row.map_or(extent, |width| width.max(extent));
            let widened_row = widen_width(&row.width, row_target);
            surface.children[row_index].width = widened_row;
            let authored_surface = evaluate_width_literal(&surface.width, "surface width");
            let right_inset = authored_surface
                .zip(authored_row)
                .map_or(0.0, |(surface_width, row_width)| {
                    (surface_width - row_offset - row_width).max(0.0)
                });
            let widened_surface =
                widen_width(&surface.width, row_offset + row_target + right_inset);
            surface.width = widened_surface;
        }
    }
    expanded
}

fn evaluate_width_literal(expression: &Expression, _label: &str) -> Option<f64> {
    expression
        .0
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// Raise a width to at least `target` without discarding authored geometry:
/// literals become the larger number, dynamic expressions keep their formula
/// and gain a floor.
fn widen_width(expression: &Expression, target: f64) -> Expression {
    match evaluate_width_literal(expression, "width") {
        Some(current) => Expression(format!("{}", current.max(target))),
        None => Expression(format!("max({}, {target})", expression.0)),
    }
}

/// Clone the template subtree for one card, rewriting the singular
/// `account.` placeholder bindings (and `{{provider}}`/`{{id}}` tokens) to the
/// concrete account. The hidden template itself is never shown.
fn instantiate_template_card(
    template: &SceneObject,
    siblings: &[SceneObject],
    card: &AccountCard,
    row_id: &str,
) -> Vec<SceneObject> {
    let key = card.binding_key();
    let tag = format!(
        "{}{}--{}",
        ACCOUNT_CARD_ID_PREFIX,
        key.replace('.', "-"),
        card.id
    );
    // Collect the template's descendant ids (transitive parent chain).
    let mut subtree: Vec<&SceneObject> = Vec::new();
    let mut frontier = vec![template.id.as_str()];
    while let Some(parent) = frontier.pop() {
        for sibling in siblings {
            if sibling.parent.as_deref() == Some(parent) {
                subtree.push(sibling);
                frontier.push(sibling.id.as_str());
            }
        }
    }
    let remap: HashMap<&str, String> = std::iter::once(template.id.as_str())
        .chain(subtree.iter().map(|object| object.id.as_str()))
        .enumerate()
        .map(|(index, old)| {
            (
                old,
                if index == 0 {
                    tag.clone()
                } else {
                    format!("{tag}--{index}")
                },
            )
        })
        .collect();
    let rewrite = |text: &str| {
        rewrite_binding_prefix(text, "account", &key)
            .replace("{{provider}}", card.provider.descriptor().key)
            .replace("{{id}}", &card.id)
    };
    let mut clones = Vec::with_capacity(subtree.len() + 1);
    let remap_object = |object: &SceneObject| {
        let mut clone = object.clone();
        clone.id = remap[object.id.as_str()].clone();
        clone.parent = Some(
            object
                .parent
                .as_deref()
                .and_then(|parent| remap.get(parent).cloned())
                .unwrap_or_else(|| row_id.to_string()),
        );
        if clone.id == tag {
            clone.render = Expression("1".into());
            clone.name = format!(
                "{} ({})",
                card.display_label(),
                card.provider.descriptor().key
            );
            let mut events = clone.mouse_events.unwrap_or_default();
            events.click = format!(
                "open_account(\"{}\", \"{}\")",
                card.provider.descriptor().key,
                card.id
            );
            clone.mouse_events = Some(events);
        }
        rewrite_object_strings(&mut clone, &rewrite);
        clone
    };
    clones.push(remap_object(template));
    for object in subtree {
        clones.push(remap_object(object));
    }
    // Busy («оновлення/операція») is a live controller state, not a poller
    // reading: bake it as a literal so the slot never depends on a binding
    // that plain usage snapshots cannot provide. Session gauges carry the
    // account color so columns stay distinguishable.
    for clone in &mut clones {
        if card.busy && clone.name == "Account busy" {
            clone.render = Expression(format!("1 - {}.has_error", key));
        }
        if clone.name == "Account label" {
            if let SceneContent::Text { color, .. } = &mut clone.content {
                *color = Paint::new(card.display_color());
            }
        }
        if clone.name == "Account session" {
            if let SceneContent::Progress { fill, .. } = &mut clone.content {
                *fill = Paint::new(card.display_color());
            }
        }
    }
    clones
}

/// Fallback card used when a marker row has no authored template: label,
/// session/weekly gauges, and a stable error line.
fn default_template_card(card: &AccountCard, row_id: &str, width: f64) -> Vec<SceneObject> {
    let key = card.binding_key();
    let tag = format!("{}{}", ACCOUNT_CARD_ID_PREFIX, key.replace('.', "-"));
    let mut container = SceneObject::object(&tag, format!("{} account", card.display_label()));
    container.parent = Some(row_id.to_string());
    container.width = Expression(format!("{width}"));
    container.height = Expression("46".into());
    container.mouse_events = Some(MouseEvents {
        click: format!(
            "open_account(\"{}\", \"{}\")",
            card.provider.descriptor().key,
            card.id
        ),
        ..Default::default()
    });
    let mut label = SceneObject::object(
        format!("{tag}--label"),
        format!("{} label", card.display_label()),
    );
    label.parent = Some(tag.clone());
    label.width = Expression(format!("{width}"));
    label.height = Expression("11".into());
    label.content = SceneContent::Text {
        template: format!("{{{}.name}}", key),
        font_family: "Segoe UI".into(),
        font_size: Expression("10".into()),
        weight: FontWeight::Regular,
        rendering: FontRendering::Antialiased,
        contrast: Expression("1.4".into()),
        align: TextAlign::Left,
        color: Paint::new(card.display_color()),
    };
    let mut session = SceneObject::object(format!("{tag}--session"), "session gauge");
    session.parent = Some(tag.clone());
    session.y = Expression("13".into());
    session.width = Expression(format!("{width}"));
    session.height = Expression("13".into());
    session.content = SceneContent::Progress {
        value: Expression(format!("{}.session.display", key)),
        direction: ProgressDirection::LeftToRight,
        fill: Paint::new(card.display_color()),
        track: Paint::new("#FFFFFF24"),
        corner_radius: Expression("2".into()),
        segments: 10,
        segments_expression: None,
        segment_gap: Expression("1".into()),
    };
    let mut error = SceneObject::object(format!("{tag}--error"), "account error");
    error.parent = Some(tag.clone());
    error.y = Expression("28".into());
    error.width = Expression(format!("{width}"));
    error.height = Expression("13".into());
    error.render = Expression(format!("{}.has_error", key));
    error.content = SceneContent::Text {
        template: "⚠".into(),
        font_family: "Segoe UI".into(),
        font_size: Expression("11".into()),
        weight: FontWeight::Bold,
        rendering: FontRendering::Antialiased,
        contrast: Expression("1.4".into()),
        align: TextAlign::Center,
        color: Paint::new("#E5484D"),
    };
    vec![container, label, session, error]
}

fn overflow_chip(row_id: &str, count: usize, _gap: f64) -> SceneObject {
    let mut chip =
        SceneObject::object(ACCOUNT_OVERFLOW_OBJECT_ID, format!("{count} more accounts"));
    chip.parent = Some(row_id.to_string());
    chip.width = Expression(format!("{}", ACCOUNT_OVERFLOW_WIDTH));
    chip.height = Expression("46".into());
    chip.mouse_events = Some(MouseEvents {
        click: "show_account_overflow()".into(),
        ..Default::default()
    });
    chip.content = SceneContent::Text {
        template: format!("+{count}"),
        font_family: "Segoe UI".into(),
        font_size: Expression("10".into()),
        weight: FontWeight::Regular,
        rendering: FontRendering::Antialiased,
        contrast: Expression("1.4".into()),
        align: TextAlign::Center,
        color: Paint::new("#FFFFFFFF"),
    };
    chip
}

/// Empty-state affordance: opens the dashboard so the user can pin accounts.
/// Only materialized when no slot is pinned at all.
fn manage_accounts_chip(row_id: &str) -> SceneObject {
    let mut chip = SceneObject::object("usagehub-accounts-manage", "Manage accounts");
    chip.parent = Some(row_id.to_string());
    chip.width = Expression(format!("{}", MANAGE_CHIP_WIDTH));
    chip.height = Expression("46".into());
    chip.mouse_events = Some(MouseEvents {
        click: "show_dashboard()".into(),
        ..Default::default()
    });
    chip.content = SceneContent::Text {
        template: "Manage accounts".into(),
        font_family: "Segoe UI".into(),
        font_size: Expression("11".into()),
        weight: FontWeight::Regular,
        rendering: FontRendering::Antialiased,
        contrast: Expression("1.4".into()),
        align: TextAlign::Center,
        color: Paint::new("#9AA0A6"),
    };
    chip
}

// ---------------------------------------------------------------------------
// Built-in path: clone per-provider groups for extra pinned accounts
// ---------------------------------------------------------------------------

/// Every provider with pinned cards gets exact per-account metrics: the
/// authored first group is rebound in place (IDs and placement preserved) to
/// the display-first card, with extra columns cloned per remaining card. A
/// single pinned card that is also the selected account keeps the legacy
/// provider binding pixel-identical. Width expressions grow by the cloned
/// column widths so nothing is clipped.
fn expand_builtin_theme(theme: &ThemeDocument, layout: &AccountLayout) -> ThemeDocument {
    let by_provider = grouped_cards(layout);
    let needs_work = !layout.overflow.is_empty()
        || ACCOUNT_CARD_PROVIDERS
            .iter()
            .any(|provider| !by_provider.contains_key(provider))
        || by_provider
            .values()
            .any(|cards| cards.len() >= 2 || (cards.len() == 1 && !cards[0].selected));
    if !needs_work {
        return theme.clone();
    }
    let mut expanded = theme.clone();
    for surface in &mut expanded.surfaces {
        // Row containers whose children gate on `providers.<key>.enabled`.
        let row_indices: Vec<usize> = surface
            .children
            .iter()
            .enumerate()
            .filter(|(_, child)| {
                matches!(child.layout, ChildLayout::Row)
                    && surface.children.iter().any(|grandchild| {
                        grandchild.parent.as_deref() == Some(child.id.as_str())
                            && provider_group_key(grandchild).is_some()
                    })
            })
            .map(|(index, _)| index)
            .collect();
        for row_index in row_indices {
            let row_id = surface.children[row_index].id.clone();
            let row_gap = surface.children[row_index].gap.0.clone();
            // Snapshot group positions before mutating.
            let groups: Vec<(usize, String)> = surface
                .children
                .iter()
                .enumerate()
                .filter(|(_, child)| {
                    child.parent.as_deref() == Some(row_id.as_str())
                        && provider_group_key(child).is_some()
                })
                .map(|(index, child)| (index, provider_group_key(child).unwrap().to_string()))
                .collect();
            let mut additions: Vec<SceneObject> = Vec::new();
            let mut width_terms: Vec<String> = Vec::new();
            for (group_index, key) in groups {
                let Some(provider) = PROVIDER_DESCRIPTORS
                    .iter()
                    .find(|d| d.key == key)
                    .map(|d| d.id)
                else {
                    continue;
                };
                let Some(cards) = by_provider.get(&provider) else {
                    if ACCOUNT_CARD_PROVIDERS.contains(&provider) {
                        surface.children[group_index].render = Expression("0".into());
                    }
                    continue;
                };
                if cards.is_empty() {
                    continue;
                }
                // Single pinned + selected: the legacy provider slot already
                // shows exactly this card, so leave it pixel-identical.
                if cards.len() == 1 && cards[0].selected {
                    continue;
                }
                let template = surface.children[group_index].clone();
                // Descendants of this provider group (transitive).
                let mut subtree: Vec<SceneObject> = Vec::new();
                let mut frontier = vec![template.id.clone()];
                while let Some(parent) = frontier.pop() {
                    for sibling in surface.children.iter().chain(additions.iter()) {
                        if sibling.parent.as_deref() == Some(parent.as_str())
                            && sibling.id != template.id
                        {
                            subtree.push(sibling.clone());
                            frontier.push(sibling.id.clone());
                        }
                    }
                }
                // Rebind the authored first group (IDs/placement preserved) to
                // the display-first card: user-reordered rows must never show
                // the previously-selected account under the first card, and a
                // lone non-selected pin must show its own limits, not the
                // selected account's. The whole first-card subtree (nested
                // children flattened via parent ids) is rebound, not just
                // the group object itself.
                {
                    let account_key = cards[0].binding_key();
                    {
                        let first = &mut surface.children[group_index];
                        first.name = format!("{} ({})", cards[0].display_label(), key);
                        let mut events = first.mouse_events.take().unwrap_or_default();
                        events.click = format!("open_account(\"{key}\", \"{}\")", cards[0].id);
                        first.mouse_events = Some(events);
                        rewrite_subtree_strings(first, &key, &account_key);
                    }
                    let subtree_ids: Vec<String> = subtree.iter().map(|o| o.id.clone()).collect();
                    for descendant in surface.children.iter_mut() {
                        if subtree_ids.iter().any(|id| id == &descendant.id) {
                            rewrite_subtree_strings(descendant, &key, &account_key);
                        }
                    }
                    let rebound = surface.children[group_index].clone();
                    additions.push(account_tag(&rebound.id.clone(), &rebound, &cards[0]));
                }
                for card in &cards[1..] {
                    let account_key = card.binding_key();
                    let tag = format!(
                        "{}-{}-{}",
                        template.id,
                        account_key.replace('.', "-"),
                        card.id
                    );
                    let remap: HashMap<String, String> = std::iter::once(template.id.clone())
                        .chain(subtree.iter().map(|o| o.id.clone()))
                        .enumerate()
                        .map(|(index, old)| {
                            (
                                old,
                                if index == 0 {
                                    tag.clone()
                                } else {
                                    format!("{tag}--{index}")
                                },
                            )
                        })
                        .collect();
                    let mut clone = template.clone();
                    clone.id = tag.clone();
                    clone.name = format!("{} ({})", card.display_label(), key);
                    let mut events = clone.mouse_events.unwrap_or_default();
                    events.click = format!("open_account(\"{key}\", \"{}\")", card.id);
                    clone.mouse_events = Some(events);
                    rewrite_subtree_strings(&mut clone, &key, &account_key);
                    additions.push(clone);
                    for mut descendant in subtree.clone() {
                        let new_parent = descendant
                            .parent
                            .as_deref()
                            .and_then(|p| remap.get(p).cloned());
                        descendant.parent = new_parent;
                        descendant.id = remap
                            .get(&descendant.id)
                            .cloned()
                            .unwrap_or_else(|| descendant.id.clone());
                        rewrite_subtree_strings(&mut descendant, &key, &account_key);
                        additions.push(descendant);
                    }
                    // Per-account color tag; labels stay in menus/tooltip so the
                    // 46px bar geometry is preserved.
                    additions.push(account_tag(&tag, &template, card));
                    width_terms.push(format!("(({}) + ({}))", template.width.0, row_gap));
                }
            }
            // Overflow chip lives in the row when the budget overflows.
            let overflow_here = !layout.overflow.is_empty();
            if overflow_here {
                additions.push(overflow_chip(
                    &row_id,
                    layout.overflow.len(),
                    ACCOUNT_CARD_GAP,
                ));
                width_terms.push(format!("({ACCOUNT_OVERFLOW_WIDTH} + ({}))", row_gap));
            }
            if additions.is_empty() {
                continue;
            }
            surface.children.extend(additions);
            if !width_terms.is_empty() {
                surface.width = Expression(format!(
                    "({}) + {}",
                    surface.width.0,
                    width_terms.join(" + ")
                ));
            }
        }
    }
    expanded
}

fn grouped_cards(layout: &AccountLayout) -> HashMap<ProviderId, Vec<AccountCard>> {
    // Layout order is display order; never re-sort (stable slots).
    let mut map: HashMap<ProviderId, Vec<AccountCard>> = HashMap::new();
    for card in &layout.visible {
        map.entry(card.provider).or_default().push(card.clone());
    }
    map
}

/// Provider key when the object's render gates exactly on that provider, e.g.
/// `providers.claude.enabled` (possibly combined with spacers via `*`).
fn provider_group_key(object: &SceneObject) -> Option<&str> {
    let render = object.render.0.trim();
    PROVIDER_DESCRIPTORS.iter().find_map(|descriptor| {
        let gate = format!("providers.{}.enabled", descriptor.key);
        (render == gate || render.split('*').any(|part| part.trim() == gate))
            .then_some(descriptor.key)
    })
}

fn account_tag(container_id: &str, template: &SceneObject, card: &AccountCard) -> SceneObject {
    let mut tag = SceneObject::object(
        format!("{container_id}--account-tag"),
        format!("{} color tag", card.display_label()),
    );
    tag.parent = Some(container_id.to_string());
    tag.width = template.width.clone();
    tag.height = Expression("2".into());
    tag.y = Expression(format!("({}) - 2", template.height.0));
    tag.background = LayerBackground::Colour {
        colour: Paint::new(card.display_color()),
    };
    tag
}

/// Rewrite standalone `<from>.` binding prefixes to `<to>.` inside every
/// expression/template string of one cloned object. Occurrences preceded by
/// an identifier character or `.` (notably `providers.<from>.`) are left
/// alone.
fn rewrite_subtree_strings(object: &mut SceneObject, from: &str, to: &str) {
    let rewrite = |text: &str| rewrite_binding_prefix(text, from, to);
    for expression in [
        &mut object.render,
        &mut object.visibility,
        &mut object.x,
        &mut object.y,
        &mut object.width,
        &mut object.height,
        &mut object.rotation,
        &mut object.gap,
        &mut object.corner_radius,
    ] {
        expression.0 = rewrite(&expression.0);
    }
    match &mut object.content {
        SceneContent::Text { template, .. } => {
            *template = rewrite(template);
        }
        SceneContent::Progress { value, .. } => {
            value.0 = rewrite(&value.0);
        }
        SceneContent::None => {}
    }
    if let LayerBackground::Gradient { angle, .. } = &mut object.background {
        angle.0 = rewrite(&angle.0);
    }
    if let Some(events) = &mut object.mouse_events {
        for handler in [
            &mut events.click,
            &mut events.double_click,
            &mut events.right_click,
            &mut events.mouse_enter,
            &mut events.mouse_leave,
        ] {
            *handler = rewrite(handler);
        }
    }
}

fn rewrite_object_strings(object: &mut SceneObject, rewrite: &impl Fn(&str) -> String) {
    for expression in [
        &mut object.render,
        &mut object.visibility,
        &mut object.x,
        &mut object.y,
        &mut object.width,
        &mut object.height,
        &mut object.rotation,
        &mut object.gap,
        &mut object.corner_radius,
    ] {
        expression.0 = rewrite(&expression.0);
    }
    match &mut object.content {
        SceneContent::Text { template, .. } => {
            *template = rewrite(template);
        }
        SceneContent::Progress { value, .. } => {
            value.0 = rewrite(&value.0);
        }
        SceneContent::None => {}
    }
    if let LayerBackground::Gradient { angle, .. } = &mut object.background {
        angle.0 = rewrite(&angle.0);
    }
    if let Some(events) = &mut object.mouse_events {
        for handler in [
            &mut events.click,
            &mut events.double_click,
            &mut events.right_click,
            &mut events.mouse_enter,
            &mut events.mouse_leave,
        ] {
            *handler = rewrite(handler);
        }
    }
}

fn rewrite_binding_prefix(text: &str, from: &str, to: &str) -> String {
    if !text.contains(from) {
        return text.to_string();
    }
    // Match on whole substrings so multi-byte characters in localized labels
    // are never split; `find`/`split_at` stay on UTF-8 boundaries.
    let needle = format!("{from}.");
    let mut output = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(offset) = rest.find(&needle) {
        let (head, tail) = rest.split_at(offset);
        let bare = head
            .chars()
            .next_back()
            .is_none_or(|prev| !(prev.is_ascii_alphanumeric() || prev == '.' || prev == '_'));
        output.push_str(head);
        if bare {
            output.push_str(to);
            output.push('.');
        } else {
            output.push_str(&tail[..needle.len()]);
        }
        rest = &tail[needle.len()..];
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{AccountProfile, AccountSettings, ProviderAccounts};

    fn pinned_settings(ids: &[&str]) -> AccountSettings {
        let mut settings = AccountSettings::default();
        settings.claude = ProviderAccounts {
            profiles: ids
                .iter()
                .enumerate()
                .map(|(index, id)| AccountProfile {
                    id: id.to_string(),
                    name: format!("Account {}", index + 1),
                    enabled: true,
                    pinned: true,
                    sort_order: index as i32,
                    ..Default::default()
                })
                .collect(),
            selected: ids.first().map(|id| id.to_string()).unwrap_or_default(),
            used_ids: ids.iter().map(|id| id.to_string()).collect(),
        };
        settings
    }

    fn builtin_like_theme() -> ThemeDocument {
        let mut surface = SceneObject::root(
            "main",
            "Main",
            Expression("200".into()),
            Expression("46".into()),
            Placement::default(),
        );
        let mut row = SceneObject::object("provider-row", "Enabled providers");
        row.parent = Some(surface.id.clone());
        row.width = Expression("canvas.width - 42".into());
        row.height = Expression("46".into());
        row.layout = ChildLayout::Row;
        row.gap = Expression("3".into());
        let mut group = SceneObject::object("claude-provider", "Claude Code provider");
        group.parent = Some(row.id.clone());
        group.render = Expression("providers.claude.enabled".into());
        group.width = Expression("100".into());
        group.height = Expression("46".into());
        let mut segments = SceneObject::object("claude-segments", "Claude segments");
        segments.parent = Some(group.id.clone());
        segments.width = Expression("50".into());
        segments.height = Expression("13".into());
        segments.content = SceneContent::Progress {
            value: Expression("claude.session.display".into()),
            direction: ProgressDirection::LeftToRight,
            fill: Paint::new("#D97757"),
            track: Paint::new("#FFFFFF24"),
            corner_radius: Expression("2".into()),
            segments: 10,
            segments_expression: None,
            segment_gap: Expression("1".into()),
        };
        surface.children = vec![row, group, segments];
        ThemeDocument {
            schema_version: THEME_SCHEMA_VERSION,
            id: crate::theme_engine::CLASSIC_THEME_ID.into(),
            name: "Classic v1".into(),
            canvas: Canvas::default(),
            placement: Placement::default(),
            children: Vec::new(),
            surfaces: vec![surface],
        }
    }

    #[test]
    fn third_pinned_account_appears_without_manual_json() {
        let settings = pinned_settings(&["default", "second", "third"]);
        let cards = visible_account_cards(&settings, None);
        assert_eq!(cards.len(), 3);
        // Stable slots: usage arrives later, the slot exists regardless.
        assert!(cards.iter().all(|card| card.provider == ProviderId::Claude));
        let layout = layout_accounts(&cards, 2000.0);
        assert_eq!(layout.visible.len(), 3);
        assert!(layout.overflow.is_empty());
        let expanded = expand_theme_for_accounts(&builtin_like_theme(), &layout);
        let surface = &expanded.surfaces[0];
        // Original group + one clone per extra account + one color tag each.
        assert!(surface
            .children
            .iter()
            .any(|child| child.id == "claude-provider"));
        // One rebound group clone per extra account (color tags and cloned
        // descendants share the id prefix, so only group objects that still
        // gate on the provider count here).
        let clones: Vec<_> = surface
            .children
            .iter()
            .filter(|child| {
                child.id.starts_with("claude-provider-accounts-claude-")
                    && provider_group_key(child).is_some()
            })
            .collect();
        assert_eq!(clones.len(), 2);
        let rebound: Vec<_> = surface
            .children
            .iter()
            .filter_map(|child| match &child.content {
                SceneContent::Progress { value, .. } => Some(value.0.clone()),
                _ => None,
            })
            .collect();
        assert!(rebound.contains(&"accounts.claude.second.session.display".to_string()));
        assert!(rebound.contains(&"accounts.claude.third.session.display".to_string()));
        // Width grows by the cloned columns instead of clipping them.
        assert!(surface.width.0.len() > "200".len());
    }

    fn data_with_selection(pairs: &[(&str, bool)]) -> AppUsageData {
        use crate::models::AccountUsage;
        let mut data = AppUsageData::default();
        for (id, selected) in pairs {
            data.accounts.push(AccountUsage {
                provider: ProviderId::Claude,
                profile: AccountProfile {
                    id: id.to_string(),
                    name: id.to_string(),
                    enabled: true,
                    ..Default::default()
                },
                source_signature: String::new(),
                source_path: None,
                usage: None,
                error: None,
                selected: *selected,
            });
        }
        data
    }

    fn session_values(surface: &SceneObject) -> Vec<String> {
        surface
            .children
            .iter()
            .filter_map(|child| match &child.content {
                SceneContent::Progress { value, .. } => Some(value.0.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn single_pinned_selected_stays_pixel_identical() {
        let settings = pinned_settings(&["default"]);
        let data = data_with_selection(&[("default", true)]);
        let cards = visible_account_cards(&settings, Some(&data));
        assert!(cards[0].selected);
        let layout = layout_accounts(&cards, 2000.0);
        let expanded = expand_theme_for_accounts(&builtin_like_theme(), &layout);
        assert_eq!(
            expanded.surfaces[0].children.len(),
            builtin_like_theme().surfaces[0].children.len()
        );
        assert_eq!(expanded.surfaces[0].width.0, "200");
    }

    #[test]
    fn single_pinned_non_selected_shows_own_limits() {
        let settings = pinned_settings(&["second"]);
        // Registry selection still points at Default; the lone pin must show
        // Second's limits, never Default's.
        let data = data_with_selection(&[("default", true), ("second", false)]);
        let cards = visible_account_cards(&settings, Some(&data));
        assert_eq!(cards.len(), 1);
        assert!(!cards[0].selected);
        let layout = layout_accounts(&cards, 2000.0);
        let expanded = expand_theme_for_accounts(&builtin_like_theme(), &layout);
        let surface = &expanded.surfaces[0];
        // No clones, surface width untouched — but the authored group is
        // rebound in place (IDs preserved) with a color tag.
        assert!(!surface
            .children
            .iter()
            .any(|child| { child.id.starts_with("claude-provider-accounts-claude-") }));
        assert_eq!(surface.width.0, "200");
        assert_eq!(
            session_values(surface),
            vec!["accounts.claude.second.session.display".to_string()]
        );
        assert!(surface
            .children
            .iter()
            .any(|child| child.id == "claude-provider--account-tag"));
    }

    #[test]
    fn reordered_second_first_shows_each_accounts_exact_limits() {
        let settings = pinned_settings(&["second", "default"]);
        let data = data_with_selection(&[("second", false), ("default", true)]);
        let cards = visible_account_cards(&settings, Some(&data));
        assert_eq!(
            cards
                .iter()
                .map(|card| card.id.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "default"]
        );
        let layout = layout_accounts(&cards, 2000.0);
        let expanded = expand_theme_for_accounts(&builtin_like_theme(), &layout);
        let surface = &expanded.surfaces[0];
        // First group keeps its authored ID but carries Second's metrics.
        let first = surface
            .children
            .iter()
            .find(|child| child.id == "claude-provider")
            .expect("authored group id preserved");
        assert!(
            matches!(&first.mouse_events, Some(events) if events.click == "open_account(\"claude\", \"second\")")
        );
        assert_eq!(
            session_values(surface),
            vec![
                "accounts.claude.second.session.display".to_string(),
                "accounts.claude.default.session.display".to_string()
            ]
        );
    }

    #[test]
    fn custom_theme_without_marker_is_preserved_verbatim() {
        let mut theme = builtin_like_theme();
        theme.id = "unrelated-custom-theme".into();
        let settings = pinned_settings(&["default", "second", "third"]);
        let cards = visible_account_cards(&settings, None);
        let layout = layout_accounts(&cards, 2000.0);
        let expanded = expand_theme_for_accounts(&theme, &layout);
        assert_eq!(
            expanded.surfaces[0].children.len(),
            theme.surfaces[0].children.len()
        );
    }

    #[test]
    fn imported_classic_replaces_legacy_slot_and_preserves_account_order() {
        let mut theme = builtin_like_theme();
        theme.id = "left-dock-usage".into();
        let surface = &mut theme.surfaces[0];
        let mut second = surface.children[1].clone();
        second.id = "second-provider".into();
        let mut legacy_child = surface.children[2].clone();
        legacy_child.id = "second-segments".into();
        legacy_child.parent = Some(second.id.clone());
        let mut codex = surface.children[1].clone();
        codex.id = "codex-provider".into();
        codex.render = Expression("providers.codex.enabled".into());
        surface.children.extend([second, legacy_child, codex]);
        let cards = visible_account_cards(&pinned_settings(&["second", "third"]), None);
        let layout = layout_accounts(&cards, 2000.0);
        let expanded = expand_theme_for_accounts(&theme, &layout);
        let surface = &expanded.surfaces[0];
        let columns: Vec<_> = surface
            .children
            .iter()
            .filter(|child| child.parent.as_deref() == Some("provider-row"))
            .collect();
        assert_eq!(columns.len(), 2);
        assert_eq!(
            columns[0].mouse_events.as_ref().unwrap().click,
            "open_account(\"claude\", \"second\")"
        );
        assert_eq!(
            columns[1].mouse_events.as_ref().unwrap().click,
            "open_account(\"claude\", \"third\")"
        );
        assert!(surface
            .children
            .iter()
            .all(|child| !child.id.starts_with("second-")));
        assert_eq!(
            session_values(surface),
            vec![
                "accounts.claude.second.session.display".to_string(),
                "accounts.claude.third.session.display".to_string(),
            ]
        );
        assert_eq!(columns[0].width.0, theme.surfaces[0].children[1].width.0);
    }

    #[test]
    fn narrow_budget_overflows_to_chooser_instead_of_covering_taskbar() {
        let settings = pinned_settings(&["a1", "a2", "a3", "a4"]);
        let cards = visible_account_cards(&settings, None);
        let layout = layout_accounts(&cards, 200.0);
        assert_eq!(layout.visible.len(), 1);
        assert_eq!(layout.overflow.len(), 3);
    }

    #[test]
    fn rewrite_keeps_provider_gates_and_is_utf8_safe() {
        assert_eq!(
            rewrite_binding_prefix("providers.claude.enabled", "claude", "accounts.claude.a1"),
            "providers.claude.enabled"
        );
        assert_eq!(
            rewrite_binding_prefix(
                "{claude.weekly.display:usage_line}",
                "claude",
                "accounts.claude.second"
            ),
            "{accounts.claude.second.weekly.display:usage_line}"
        );
        let localized = "оновлення… {account.session.display} — готово";
        assert_eq!(
            rewrite_binding_prefix(localized, "account", "accounts.claude.a1"),
            "оновлення… {accounts.claude.a1.session.display} — готово"
        );
    }

    #[test]
    fn singular_placeholders_resolve_but_typos_still_fail() {
        let context = DataContext::default();
        assert!(context.get("account.session.display").is_some());
        assert!(context
            .get("accounts.claude.work.session.display")
            .is_some());
        assert!(context.get_string("account.name").is_some());
        assert!(context.get("account.bogus_field").is_none());
        assert!(context.get("accounts.claude.work.bogus_field").is_none());
    }
}
