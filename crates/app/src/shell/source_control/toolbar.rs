//! Toolbar rendering for the Source Control panel: scope tabs, branch
//! summary chip + view-mode / base-ref / refresh icon cluster, and the
//! filter input row.
//!
//! All three render methods are `pub(super) fn` on `SourceControlPanel`
//! so the panel's `Render` impl in `mod.rs` calls them with the same
//! ergonomic `self.render_*(cx)` shape. The split is purely structural
//! — there is no toolbar state of its own; everything reads from the
//! panel's existing fields (`scope`, `git_state`, `git_panel`,
//! `base_ref`, `filter_input`, etc.).
//!
//! The branch summary string lives here (prefix counts + clickable
//! branch chip) because the chip's click handler dispatches into the
//! picker-wiring module via [`SourceControlPanel::open_switch_picker`]
//! — keeping the two together (chip + handler) would force a circular
//! dependency, so the render call site sits here and the handler sits
//! in `picker_wiring.rs`.

use std::sync::atomic::Ordering;

use gpui::{
    ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::Input,
};

use crate::shell::source_control::SourceControlPanel;
use crate::shell::source_control::scope::SourceControlScope;
use crate::shell::source_control::style as sc_style;

impl SourceControlPanel {
    pub(super) fn render_scope_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typography = &self.typography;
        let _ = self.density;
        let scopes = [SourceControlScope::All, SourceControlScope::Uncommitted];
        // Fixed height + `flex_shrink_0` keeps the row from being compressed
        // when the file list below expands (otherwise the row shrinks under
        // flex pressure and the active underline visually butts against the
        // activity-bar icons above). `items_end` aligns each tab's bottom
        // edge with the row's bottom border so the active 2px underline lands
        // on the row's 1px border (one unified line).
        let mut row = div()
            .flex()
            .flex_row()
            .items_end()
            .flex_shrink_0()
            .h(px(sc_style::TAB_H))
            .border_b_1()
            .border_color(theme.border_inactive)
            .px(px(sc_style::PAD_H));
        // Count of uncommitted changed files for the "Uncommitted" tab
        // badge. Counted once outside the loop so the per-tab render
        // doesn't re-walk `git_state.files` twice. Zero suppresses the
        // pill entirely — an empty `(0)` would just add visual noise.
        let uncommitted_count = self
            .git_state
            .as_ref()
            .map_or(0, |s| s.files.len());
        for scope in scopes {
            let active = scope == self.scope;
            let label = scope.label();
            let fg = if active {
                theme.fg_base
            } else {
                theme.fg_muted
            };
            // Underline color follows the active text color; inactive tabs
            // hold a transparent placeholder so the 2px border doesn't shift
            // the row baseline between active/inactive states.
            let underline = if active {
                theme.fg_base
            } else {
                gpui::transparent_black()
            };
            // Show a small count pill next to the "Uncommitted" tab when
            // there are changed files. Skipped on the "All" tab because
            // the total commit count isn't surfaced through git_state
            // and a fabricated number would mislead.
            let badge_count = if scope == SourceControlScope::Uncommitted {
                uncommitted_count
            } else {
                0
            };
            let label_row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(label)
                .when(badge_count > 0, |row| {
                    row.child(
                        div()
                            .px(px(5.0))
                            .py(px(0.0))
                            .rounded(px(6.0))
                            .bg(theme.bg_panel_alt)
                            .text_size(px(9.0))
                            .text_color(theme.fg_subtle)
                            .child(badge_count.to_string()),
                    )
                });
            let tab = div()
                .flex()
                .items_center()
                .justify_center()
                .px(px(sc_style::PAD_H))
                .pb(px(sc_style::PAD_V))
                .text_size(px(sc_style::TEXT))
                .font_weight(typography.w_medium)
                .text_color(fg)
                .cursor_pointer()
                .border_b_2()
                .border_color(underline)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |panel, _: &MouseDownEvent, _window, cx| {
                        panel.select_scope(scope, cx);
                    }),
                )
                .child(label_row);
            row = row.child(tab);
        }
        row
    }

    pub(super) fn render_branch_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use oximux_core::ViewMode;
        let view_mode = self.git_panel.read(cx).view_mode();
        let (view_mode_icon, view_mode_tooltip) = match view_mode {
            ViewMode::Flat => ("icons/list-tree.svg", "Switch to tree view"),
            ViewMode::Tree => ("icons/list-collapse.svg", "Switch to flat view"),
        };
        let theme = self.theme;
        let _ = (self.density, &self.typography);
        let (ahead, behind, branch) = self
            .git_state
            .as_ref()
            .map(|s| (s.ahead, s.behind, s.branch.clone()))
            .unwrap_or((0, 0, None));
        // Split the summary into prefix (counts) + clickable branch chip
        // suffix. The chip opens the Switch-mode branch picker.
        // Detached HEAD / pre-poll → branch is None; we drop the chip
        // entirely. `filter(!is_empty)` is defensive against a corrupt
        // `GitState` carrying `Some("")` — the live parser never emits
        // empty, but the guard keeps the toolbar from showing a stray
        // trailing " of ".
        let prefix = if behind == 0 && ahead == 0 {
            "0 commits ahead".to_string()
        } else if behind == 0 {
            format!(
                "{ahead} commit{} ahead",
                if ahead == 1 { "" } else { "s" }
            )
        } else if ahead == 0 {
            format!(
                "{behind} commit{} behind",
                if behind == 1 { "" } else { "s" }
            )
        } else {
            format!("{ahead} ahead • {behind} behind")
        };
        let branch_chip: Option<gpui::SharedString> = branch
            .as_deref()
            .filter(|b| !b.is_empty())
            .map(|b| gpui::SharedString::from(b.to_string()));

        // Right-aligned compact icon cluster.
        // settings-2 opens the BaseRef picker; list-tree / list-collapse
        // is the view-mode toggle (icon + tooltip swap built above from
        // the current `view_mode`); refresh is wired to the commit-graph
        // reload.
        let base_ref_tooltip = self
            .base_ref
            .as_deref()
            .map(|b| format!("Base ref: {b} (click to change)"))
            .unwrap_or_else(|| "Change base ref".to_string());
        let actions = div()
            .ml_auto()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .child(
                Button::new("sc-toolbar-base-ref")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/settings-2.svg")
                            .size(px(sc_style::ICON)),
                    )
                    .tooltip(base_ref_tooltip)
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.open_base_ref_picker(window, cx);
                    })),
            )
            .child(
                Button::new("sc-toolbar-view-mode")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path(view_mode_icon)
                            .size(px(sc_style::ICON)),
                    )
                    .tooltip(view_mode_tooltip)
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        panel.git_panel.update(cx, |g, cx| g.cycle_view_mode(cx));
                    })),
            )
            .child(
                Button::new("sc-toolbar-refresh")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/refresh-cw.svg")
                            .size(px(sc_style::ICON)),
                    )
                    .tooltip("Refresh")
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        panel.commit_graph.update(cx, |g, cx| g.refresh(cx));
                    })),
            );

        let mut summary_row = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(sc_style::TOOLBAR_H))
            .px(px(sc_style::PAD_H))
            .border_b_1()
            .border_color(theme.border_inactive)
            .text_size(px(sc_style::TEXT))
            .text_color(theme.fg_muted)
            .child(prefix);
        if let Some(chip) = branch_chip {
            // " of " stays in the muted summary; only the branch name is
            // clickable + emphasized so the affordance is obvious without
            // needing a visible button frame.
            summary_row = summary_row.child(" of ").child(
                div()
                    .id("sc-branch-chip")
                    .ml(px(2.0))
                    .text_color(theme.fg_base)
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|panel, _: &MouseDownEvent, window, cx| {
                            panel.open_switch_picker(window, cx);
                        }),
                    )
                    .child(chip),
            );
        }

        // Push / Pull pills surface the same network ops that live in the
        // dropdown, one click away. Gated by `ahead`/`behind` so they
        // only appear when actionable, and by `in_flight` so a second
        // click during the spawn doesn't double-trigger (the underlying
        // single-flight contract in `commit_ops::run_remote` would catch
        // it, but hiding the affordance keeps the surface honest).
        //
        // Push pill fires `CommitArea::push` directly — a regular
        // (non-force) push. Force-push lives behind the dropdown's
        // confirm-modal path; the pill is the fast happy-path only.
        let in_flight = self
            .commit_area
            .read(cx)
            .in_flight
            .load(Ordering::Relaxed);
        if ahead > 0 && !in_flight {
            let push_label = format!("Push {ahead}");
            let push_tooltip = format!(
                "Push {ahead} commit{} to remote",
                if ahead == 1 { "" } else { "s" }
            );
            summary_row = summary_row.child(
                div().ml(px(8.0)).child(
                    Button::new("sc-push-pill")
                        .ghost()
                        .xsmall()
                        .label(push_label)
                        .tooltip(push_tooltip)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.commit_area.update(cx, |ca, cx| ca.push(cx));
                        })),
                ),
            );
        }
        if behind > 0 && !in_flight {
            let pull_label = format!("Pull {behind}");
            let pull_tooltip = format!(
                "Pull {behind} commit{} from remote",
                if behind == 1 { "" } else { "s" }
            );
            summary_row = summary_row.child(
                div().ml(px(4.0)).child(
                    Button::new("sc-pull-pill")
                        .ghost()
                        .xsmall()
                        .label(pull_label)
                        .tooltip(pull_tooltip)
                        .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                            panel.commit_area.update(cx, |ca, cx| ca.pull(cx));
                        })),
                ),
            );
        }
        summary_row.child(actions)
    }

    pub(super) fn render_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let _ = (self.density, &self.typography);
        let has_query = !self.filter_query.is_empty();
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap(px(6.0))
            .px(px(sc_style::PAD_H))
            .py(px(sc_style::PAD_V_TIGHT))
            .border_b_1()
            .border_color(theme.border_inactive)
            .child(
                Icon::new(IconName::Search)
                    .size(px(sc_style::ICON))
                    .text_color(theme.fg_subtle),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(sc_style::TEXT))
                    .child(Input::new(&self.filter_input).appearance(false)),
            );
        if has_query {
            row = row.child(
                Button::new("sc-filter-clear")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/x.svg").size(px(sc_style::ICON)))
                    .tooltip("Clear filter")
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.clear_filter(window, cx);
                    })),
            );
        }
        row
    }

    pub(super) fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_query.clear();
        let panel = self.git_panel.clone();
        panel.update(cx, |p, cx| p.set_filter(String::new(), cx));
        cx.notify();
    }
}
