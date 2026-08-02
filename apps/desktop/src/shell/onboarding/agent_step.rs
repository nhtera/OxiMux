//! Onboarding step 1: the default-agent picker. Roster assembly (pure,
//! unit-tested), install detection state, model-source resolution, and the
//! card-list render.
//!
//! Model lists NEVER cold-probe a backend here (`probe_catalog` costs up to
//! ~5s for a Node ACP agent, and a fresh install has an empty catalog cache):
//! Claude uses its static rich vocabulary, other agents use a warm
//! `CatalogCache` entry when one exists, and everything else shows a
//! "Default model" placeholder resolved on first chat.

use gpui::{
    App, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px, svg,
};
use gpui_component::Sizable as _;
use gpui_component::searchable_list::SearchableListItem;
use gpui_component::select::Select;
use oximux_agents::thread::{ModelChoice, claude_model_choices};
use oximux_settings::ACP_PRESETS;

use super::OnboardingWizard;

/// One row in the onboarding model `Select`. Mirrors the composer's item shape
/// (name on top, muted capability blurb beneath, both searchable) so the two
/// pickers read identically; the composer's own item type is module-private.
#[derive(Clone)]
pub(super) struct OnboardModelItem {
    pub wire: String,
    pub label: String,
    pub description: Option<String>,
}

impl SearchableListItem for OnboardModelItem {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn render(&self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        use gpui_component::ActiveTheme as _;
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(SharedString::from(self.label.clone()))
            .when_some(self.description.clone(), |this, desc| {
                this.child(
                    div()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(desc)),
                )
            })
    }

    fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.label.to_lowercase().contains(&q)
            || self.description.as_deref().is_some_and(|d| d.to_lowercase().contains(&q))
    }

    fn value(&self) -> &Self::Value {
        &self.wire
    }
}

/// One row in the picker. Assembled once per wizard open; `available` flips
/// from `None` (detecting — rendered optimistically) to the probe result in a
/// single update, so rows grey out in place rather than reshuffling.
#[derive(Debug, Clone)]
pub(super) struct AgentRow {
    pub id: String,
    pub display_name: String,
    pub chat_capable: bool,
    /// Collapsed under the "+N more" expander until revealed.
    pub more: bool,
    /// `None` while detection is in flight; `Some(false)` greys the row.
    pub available: Option<bool>,
    /// Where "Install ↗" sends the user. `None` = no button, sub-label only.
    pub install_url: Option<&'static str>,
}

impl AgentRow {
    pub fn selectable(&self) -> bool {
        self.available.unwrap_or(true)
    }
}

/// The onboarding roster, in render order. Chat-capable agents lead (builtins
/// Claude Code / Codex / Pi, then the OpenCode preset); Cursor / Amp / Aider
/// sit under the expander. `Custom` is excluded (it needs a config flow), and
/// Copilot is import-only today — not a launchable onboarding target.
pub(super) fn assemble_roster(
    builtin_entries: &[(String, String)],
    chat_capable: impl Fn(&str) -> bool,
) -> Vec<AgentRow> {
    const MORE: &[&str] = &["cursor", "amp", "aider"];
    const EXCLUDED: &[&str] = &["custom"];
    const INSTALL_URLS: &[(&str, &str)] = &[
        ("claude-code", "https://claude.com/claude-code"),
        ("codex", "https://developers.openai.com/codex"),
        ("opencode", "https://opencode.ai"),
        ("cursor", "https://cursor.com"),
        ("amp", "https://ampcode.com"),
        ("aider", "https://aider.chat"),
    ];
    let install_url =
        |id: &str| INSTALL_URLS.iter().find(|(k, _)| *k == id).map(|(_, url)| *url);

    let mut rows: Vec<AgentRow> = builtin_entries
        .iter()
        .filter(|(id, _)| !EXCLUDED.contains(&id.as_str()))
        .map(|(id, name)| AgentRow {
            id: id.clone(),
            display_name: name.clone(),
            chat_capable: chat_capable(id),
            more: MORE.contains(&id.as_str()),
            available: None,
            install_url: install_url(id),
        })
        .collect();
    for preset in ACP_PRESETS {
        rows.push(AgentRow {
            id: preset.id.to_string(),
            display_name: preset.title.to_string(),
            chat_capable: chat_capable(preset.id),
            more: MORE.contains(&preset.id),
            available: None,
            install_url: install_url(preset.id),
        });
    }
    // Render order: main group first (roster order within), expander group
    // after in the mockup's Cursor → Amp → Aider order. The sort is stable, so
    // non-more rows (rank 0) keep their insertion order.
    let more_rank =
        |row: &AgentRow| MORE.iter().position(|m| *m == row.id.as_str()).unwrap_or(0);
    rows.sort_by_key(|r| (r.more, more_rank(r)));
    rows
}

/// Where a row's model list comes from. Resolution order: Claude's static rich
/// vocabulary → a warm catalog-cache entry → Codex's static slug list →
/// deferred to first chat. The cache outranks Codex's static list because a
/// live probe is fresher than the pinned slugs.
pub(super) enum ModelSource {
    Rich { choices: Vec<ModelChoice>, default_model: Option<String> },
    Deferred,
}

pub(super) fn resolve_model_source(
    adapter_id: &str,
    cached: Option<(Vec<ModelChoice>, Option<String>)>,
    codex_static: &[&str],
) -> ModelSource {
    if adapter_id == "claude-code" {
        return ModelSource::Rich { choices: claude_model_choices(), default_model: None };
    }
    if let Some((choices, default_model)) = cached
        && !choices.is_empty()
    {
        return ModelSource::Rich { choices, default_model };
    }
    if adapter_id == "codex" {
        return ModelSource::Rich {
            choices: codex_static
                .iter()
                .map(|slug| ModelChoice {
                    wire: (*slug).to_string(),
                    label: (*slug).to_string(),
                    description: None,
                })
                .collect(),
            default_model: None,
        };
    }
    ModelSource::Deferred
}

impl OnboardingWizard {
    /// Step 1 body: heading + agent cards + expander.
    pub(super) fn render_agent_body(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let expanded = self.expanded;
        let more_count = self.rows.iter().filter(|r| r.more).count();

        let mut list = div().flex().flex_col().gap(px(8.0));
        for (ix, row) in self.rows.clone().into_iter().enumerate() {
            if row.more && !expanded {
                continue;
            }
            list = list.child(self.render_agent_card(ix, &row, cx));
        }

        div()
            .flex()
            .flex_col()
            .child(list)
            .when(more_count > 0, |col| {
                col.child(
                    div()
                        .id("onboarding-expander")
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(density.gap_inline))
                        .pt(px(12.0))
                        .text_size(px(typography.t_body_md))
                        .text_color(theme.fg_muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.fg_base))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _window, cx| {
                                this.expanded = !this.expanded;
                                cx.notify();
                            }),
                        )
                        .child(if expanded {
                            "Show less".to_string()
                        } else {
                            format!("+ {more_count} more agents")
                        }),
                )
            })
    }

    fn render_agent_card(
        &mut self,
        ix: usize,
        row: &AgentRow,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let selected = self.selected_agent.as_deref() == Some(row.id.as_str());
        let selectable = row.selectable();
        let id = row.id.clone();

        let icon_tile = div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(30.0))
            .rounded(px(7.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_inactive)
            .child(
                svg()
                    .path(crate::shell::agent_presentation::adapter_icon_path(&row.id))
                    .size(px(17.0))
                    .text_color(if selectable { theme.fg_base } else { theme.fg_subtle }),
            );

        let mut name_row = div()
            .flex()
            .items_center()
            .gap(px(density.gap_inline + 2.0))
            .text_size(px(typography.t_body_md))
            .font_weight(typography.w_semibold)
            .text_color(if selectable { theme.fg_base } else { theme.fg_subtle })
            .child(row.display_name.clone());
        if !row.chat_capable {
            name_row = name_row.child(
                div()
                    .text_size(px(typography.t_label_xs - 1.0))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_subtle)
                    .border_1()
                    .border_color(theme.border_inactive)
                    .rounded(px(density.r_chip))
                    .px(px(5.0))
                    .py(px(1.0))
                    .child("TERMINAL ONLY"),
            );
        }
        let mut identity = div().flex_1().min_w_0().child(name_row);
        if row.available == Some(false) {
            identity = identity.child(
                div()
                    .text_size(px(typography.t_sub_label))
                    .text_color(theme.fg_subtle)
                    .mt(px(3.0))
                    .child("CLI NOT FOUND ON PATH"),
            );
        }

        let mut card = div()
            .id(("onboarding-agent", ix))
            .flex()
            .items_center()
            .gap(px(12.0))
            .min_h(px(54.0))
            .pl(px(14.0))
            .pr(px(10.0))
            .py(px(8.0))
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(if selected { theme.focus_ring } else { theme.border_inactive })
            .rounded(px(density.r_card))
            .child(icon_tile)
            .child(identity);

        if selectable {
            card = card
                .cursor_pointer()
                .hover(|s| s.bg(theme.bg_overlay))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| {
                        this.select_agent(&id, window, cx);
                    }),
                );
            if selected {
                card = card.child(self.render_model_control(cx));
            }
        } else if let Some(url) = row.install_url {
            card = card.child(
                div()
                    .id(("onboarding-install", ix))
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .border_1()
                    .border_color(theme.border_input)
                    .rounded(px(density.r_xs))
                    .px(px(13.0))
                    .py(px(6.0))
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.fg_base).border_color(theme.border_active))
                    .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                        cx.stop_propagation();
                        crate::shell::open_url::open_url(url, cx);
                    })
                    .child("Install ↗"),
            );
        }
        card
    }

    /// The trailing control on the selected row: the searchable model Select
    /// when this agent has a resolvable list, else the "Default model"
    /// placeholder (resolved on first chat — never probed here).
    fn render_model_control(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let typography = self.typography.clone();
        let has_choices = self.selected_model_source_is_rich(cx);
        if has_choices && let Some(select) = self.model_select.clone() {
            return div()
                .flex_none()
                .on_mouse_down(MouseButton::Left, |_, _window, cx| {
                    // A click on the dropdown must not re-dispatch card select.
                    cx.stop_propagation();
                })
                .child(
                    Select::new(&select)
                        .small()
                        .menu_width(px(320.0))
                        .search_placeholder("Search models…"),
                )
                .into_any_element();
        }
        div()
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_subtle)
            .italic()
            .child("Default model")
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_pairs() -> Vec<(String, String)> {
        [("claude-code", "Claude Code"), ("codex", "Codex"), ("pi", "Pi"), ("aider", "Aider"), ("custom", "Custom")]
            .into_iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn roster_orders_main_before_more_and_excludes_custom() {
        let rows = assemble_roster(&builtin_pairs(), |id| {
            matches!(id, "claude-code" | "codex" | "pi" | "opencode" | "cursor" | "amp")
        });
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["claude-code", "codex", "pi", "opencode", "cursor", "amp", "aider"]);
        assert!(!ids.contains(&"custom"));
        let more: Vec<&str> =
            rows.iter().filter(|r| r.more).map(|r| r.id.as_str()).collect();
        assert_eq!(more, ["cursor", "amp", "aider"]);
        // Aider is the terminal-only badge case.
        assert!(!rows.iter().find(|r| r.id == "aider").unwrap().chat_capable);
    }

    #[test]
    fn model_source_resolution_order() {
        // Claude: static rich list regardless of cache.
        assert!(matches!(
            resolve_model_source("claude-code", None, &[]),
            ModelSource::Rich { .. }
        ));
        // Warm cache wins for any other agent.
        let cached = Some((
            vec![ModelChoice { wire: "m".into(), label: "M".into(), description: None }],
            Some("m".to_string()),
        ));
        let ModelSource::Rich { default_model, .. } =
            resolve_model_source("opencode", cached, &[])
        else {
            panic!("expected rich source from cache");
        };
        assert_eq!(default_model.as_deref(), Some("m"));
        // Codex falls back to its static slugs when the cache is cold.
        let ModelSource::Rich { choices, .. } =
            resolve_model_source("codex", None, &["gpt-5-codex"])
        else {
            panic!("expected codex static fallback");
        };
        assert_eq!(choices[0].wire, "gpt-5-codex");
        // Everything else defers.
        assert!(matches!(resolve_model_source("pi", None, &[]), ModelSource::Deferred));
        // An empty cached list must not produce an empty picker.
        assert!(matches!(
            resolve_model_source("cursor", Some((vec![], None)), &[]),
            ModelSource::Deferred
        ));
    }
}
