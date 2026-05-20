//! Source Control tab body — replaces the bare `GitPanel + DiffView` mount
//! inside `RightSidebar`. Composes:
//!
//! ```text
//! ┌── scope tabs (All | Uncommitted) ────┐
//! ├── branch chip (branch • ↑A ↓B • N) ──┤
//! ├── filter input ──────────────────────┤
//! ├── commit area (prefix subj body btn) ┤
//! ├── files list (GitPanel)              ┤
//! ├── diff view (DiffView)               ┤
//! ├── commit graph (Phase 05, scope=All) ┤
//! └──────────────────────────────────────┘
//! ```
//!
//! All children stay always-mounted (Phase 04 plan: avoid IPC storms on tab
//! switch). Filter / scope / commit state lives on `SourceControlPanel`.

pub mod commit_area;
pub mod filter;
pub mod graph;
pub mod primary_action;
pub mod scope;
pub mod style;

use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Subscription, Window, div, px,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use oximux_core::GitState;
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::watch;

use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::GitPanel;
use crate::shell::source_control::commit_area::CommitArea;
use crate::shell::source_control::graph::CommitGraph;
use crate::shell::source_control::primary_action::{
    PrimaryAction, PrimaryActionInputs, RemoteOpKind, UpstreamStatus, resolve_primary_action,
};
use crate::shell::source_control::scope::SourceControlScope;
use crate::shell::source_control::style as sc_style;

/// Bundle of repo + design tokens passed through `SourceControlPanel::new`.
pub struct PanelConfig {
    pub repo: Repository,
    pub theme: Theme,
    pub density: Density,
    pub typography: Typography,
}

pub struct SourceControlPanel {
    /// Snapshot of the last `PollState` from the StatusPoller. Held for
    /// future use (in-flight indicators, error toasts); read by render via
    /// `git_state` for now.
    #[allow(dead_code)]
    poll_state: PollState,
    git_state: Option<GitState>,

    scope: SourceControlScope,
    filter_query: String,
    filter_input: Entity<InputState>,

    // Track which remote op (if any) the user kicked off so the primary
    // button can mirror the in-flight kind. `Arc<watch>` so the spawn task
    // can flip it back to `None` regardless of which panel update wins.
    in_flight_remote: Arc<std::sync::Mutex<Option<RemoteOpKind>>>,

    // Composed entities — always mounted.
    pub git_panel: Entity<GitPanel>,
    pub diff_view: Entity<DiffView>,
    pub commit_area: Entity<CommitArea>,
    pub commit_graph: Entity<CommitGraph>,

    theme: Theme,
    density: Density,
    typography: Typography,

    _state_observer: gpui::Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SourceControlPanel {
    pub fn new(
        cfg: PanelConfig,
        state_rx: watch::Receiver<PollState>,
        diff_view: Entity<DiffView>,
        git_panel: Entity<GitPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let PanelConfig {
            repo,
            theme,
            density,
            typography,
        } = cfg;
        let initial = state_rx.borrow().clone();
        let git_state = match &initial {
            PollState::Ready(s) => Some(s.clone()),
            _ => None,
        };
        let observer = Self::start_state_observer(state_rx, cx);

        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
        // Push every keystroke into both this panel's `filter_query` (used by
        // the future "no matches" empty state) and into the embedded
        // `GitPanel`, which actually filters the rendered file list.
        let panel_for_filter = git_panel.clone();
        let filter_sub = cx.subscribe_in(
            &filter_input,
            window,
            move |me, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    me.filter_query = value.clone();
                    panel_for_filter.update(cx, |panel, cx| panel.set_filter(value, cx));
                    cx.notify();
                }
            },
        );

        let commit_area = cx.new(|cx| {
            CommitArea::new(repo.clone(), theme, density, typography.clone(), window, cx)
        });
        let commit_graph =
            cx.new(|cx| CommitGraph::new(repo.clone(), theme, density, typography.clone(), cx));

        // `repo` was passed into the children (CommitArea, CommitGraph) and
        // GitPanel; the panel itself doesn't need to retain it for v1. If a
        // future feature (e.g. branch switcher) lands here, restore the field.
        let _ = repo;
        Self {
            poll_state: initial,
            git_state,
            scope: SourceControlScope::All,
            filter_query: String::new(),
            filter_input,
            in_flight_remote: Arc::new(std::sync::Mutex::new(None)),
            git_panel,
            diff_view,
            commit_area,
            commit_graph,
            theme,
            density,
            typography,
            _state_observer: observer,
            _subscriptions: vec![filter_sub],
        }
    }

    /// Focus the commit subject. Called from `RightSidebar` when Cmd+K fires.
    pub fn focus_commit_subject(&self, window: &mut Window, cx: &mut Context<Self>) {
        // Re-fetch through the entity update so the inner InputState gets
        // the window context it needs.
        let area = self.commit_area.clone();
        area.update(cx, |a, cx| a.focus_subject(window, cx));
    }

    pub fn select_scope(&mut self, scope: SourceControlScope, cx: &mut Context<Self>) {
        self.scope = scope;
        cx.notify();
    }

    fn start_state_observer(
        mut rx: watch::Receiver<PollState>,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let state = rx.borrow_and_update().clone();
                if this
                    .update(cx, |panel, cx| {
                        if let PollState::Ready(ref s) = state {
                            panel.git_state = Some(s.clone());
                        }
                        panel.poll_state = state;
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    fn resolve_primary(&self, cx: &Context<Self>) -> PrimaryAction {
        let (staged_count, has_unstaged, has_partial, has_conflict, upstream) = self
            .git_state
            .as_ref()
            .map(|s| {
                use oximux_core::IndexStatus;
                use oximux_core::WorktreeStatus;
                let mut staged = 0usize;
                let mut unstaged = false;
                let mut partial = false;
                let mut conflict = false;
                for f in &s.files {
                    if matches!(f.index, IndexStatus::Unmerged)
                        || matches!(f.worktree, WorktreeStatus::Unmerged)
                    {
                        conflict = true;
                    }
                    let is_s = f.is_staged();
                    let is_u = f.is_unstaged();
                    if is_s {
                        staged += 1;
                    }
                    if is_u {
                        unstaged = true;
                    }
                    if is_s && is_u {
                        partial = true;
                    }
                }
                let upstream = if s.upstream.is_some() {
                    Some(UpstreamStatus {
                        has_upstream: true,
                        ahead: s.ahead,
                        behind: s.behind,
                    })
                } else if s.branch.is_some() {
                    Some(UpstreamStatus::default())
                } else {
                    None
                };
                (staged, unstaged, partial, conflict, upstream)
            })
            .unwrap_or((0, false, false, false, None));

        let in_flight = self.in_flight_remote.lock().ok().and_then(|g| *g);
        let inputs = PrimaryActionInputs {
            staged_count,
            has_unstaged_changes: has_unstaged,
            has_partially_staged_changes: has_partial,
            has_message: self.commit_area.read(cx).has_message(cx),
            has_unresolved_conflicts: has_conflict,
            is_committing: matches!(
                self.commit_area.read(cx).status,
                commit_area::CommitStatus::Committing
            ),
            is_remote_operation_active: in_flight.is_some(),
            upstream_status: upstream,
            in_flight_remote_op_kind: in_flight,
        };
        resolve_primary_action(&inputs)
    }

    fn render_scope_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                .child(label);
            row = row.child(tab);
        }
        row
    }

    fn render_branch_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let _ = (self.density, &self.typography);
        let (ahead, behind) = self
            .git_state
            .as_ref()
            .map(|s| (s.ahead, s.behind))
            .unwrap_or((0, 0));
        let summary = if behind == 0 && ahead == 0 {
            "0 commits ahead".to_string()
        } else if behind == 0 {
            format!("{ahead} commit{} ahead", if ahead == 1 { "" } else { "s" })
        } else if ahead == 0 {
            format!(
                "{behind} commit{} behind",
                if behind == 1 { "" } else { "s" }
            )
        } else {
            format!("{ahead} ahead • {behind} behind")
        };

        // Right-aligned compact icon cluster.
        // settings-2 / list-tree are placeholders (backend lands later); refresh
        // is wired to the commit-graph reload.
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
                    .tooltip("Change base ref (coming soon)")
                    .disabled(true),
            )
            .child(
                Button::new("sc-toolbar-view-mode")
                    .ghost()
                    .xsmall()
                    .icon(
                        Icon::default()
                            .path("icons/list-tree.svg")
                            .size(px(sc_style::ICON)),
                    )
                    .tooltip("Toggle tree view (coming soon)")
                    .disabled(true),
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

        div()
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
            .child(summary)
            .child(actions)
    }

    fn render_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .icon(
                        Icon::default()
                            .path("icons/x.svg")
                            .size(px(sc_style::ICON)),
                    )
                    .tooltip("Clear filter")
                    .on_click(cx.listener(|panel, _: &ClickEvent, window, cx| {
                        panel.clear_filter(window, cx);
                    })),
            );
        }
        row
    }

    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.filter_query.clear();
        let panel = self.git_panel.clone();
        panel.update(cx, |p, cx| p.set_filter(String::new(), cx));
        cx.notify();
    }
}

impl Render for SourceControlPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let action = self.resolve_primary(cx);

        // Snapshot per-section render outputs as `AnyElement` so we can drop
        // each borrow of `cx` before composing the final tree.
        let scope_tabs: AnyElement = self.render_scope_tabs(cx).into_any_element();
        let toolbar: AnyElement = self.render_branch_toolbar(cx).into_any_element();
        let filter_row: AnyElement = self.render_filter_row(cx).into_any_element();
        let commit_area_render: AnyElement = self
            .commit_area
            .clone()
            .update(cx, |a, cx| a.render(&action, cx).into_any_element());

        // Filter wiring: helper `filter_files` is unit-tested in
        // `crates/app/tests/sc_filter.rs`; the changed-files list itself does
        // not consume the query yet — that wires through `GitPanel` in a
        // follow-up. Holding the input value here keeps the field interactive
        // and ready to plug into the panel filter slot when it lands.
        let _ = &self.filter_query;

        // Files area takes the middle slack with `flex_1` so the graph
        // section that follows can be anchored to the bottom of the panel.
        // `min_h_0` lets the file list shrink below its intrinsic content
        // height (required for the inner scroll region to actually scroll
        // rather than pushing the graph off-screen).
        let files_block = div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .flex_col()
            .overflow_hidden()
            .child(self.git_panel.clone())
            .child(self.diff_view.clone());

        let mut body = div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(theme.bg_panel)
            .child(scope_tabs)
            .child(toolbar)
            .child(filter_row)
            .child(commit_area_render)
            .child(files_block);
        if self.scope.shows_graph() {
            // Graph sits at its natural height, pinned to the bottom of the
            // panel by the `flex_1` files_block above. Top border separates
            // the graph from the file list visually.
            body = body.child(
                div()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.border_inactive)
                    .child(self.commit_graph.clone()),
            );
        }
        body
    }
}
