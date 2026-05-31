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
//! ├── commit graph (scope=All)           ┤
//! └──────────────────────────────────────┘
//! ```
//!
//! All children stay always-mounted — avoids IPC storms when the user
//! flips between scope tabs or quickly switches workspaces. Filter /
//! scope / commit state lives on `SourceControlPanel`.

pub mod branch_picker;
pub mod commit_area;
pub mod commit_ops;
pub mod dropdown_items;
pub mod filter;
pub mod graph;
pub mod primary_action;
pub mod scope;
pub mod style;
pub mod tree;

use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, Styled, Subscription, Window, div, px,
};
use gpui_component::{
    Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use oximux_core::GitState;
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::WorktreeSettingsRepo;
use tokio::sync::watch;

use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::GitPanel;
use crate::shell::source_control::branch_picker::{BranchPicker, OnPick, PickerMode, PickerOutcome};
use crate::shell::source_control::commit_area::{CommitArea, CommitStatus};
use crate::shell::source_control::dropdown_items::DropdownInputs;
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
    /// Per-worktree V006 settings repo. `None` in test wiring; the
    /// production path always supplies it. When absent, the panel
    /// renders without persistence (BaseRef picker still works but
    /// changes don't survive a restart).
    pub worktree_settings_repo: Option<WorktreeSettingsRepo>,
}

pub struct SourceControlPanel {
    /// Snapshot of the last `PollState` from the StatusPoller. Held for
    /// future use (in-flight indicators, error toasts); read by render via
    /// `git_state` for now.
    #[allow(dead_code)]
    poll_state: PollState,
    git_state: Option<GitState>,

    /// Cached result of `Repository::lease_status` — drives the Force
    /// Push label swap on the dropdown. Refreshed each time the poller
    /// reports a state where the lease semantics could plausibly apply
    /// (branch is diverged from its upstream). `false` while the first
    /// check is in flight or when the state isn't a lease candidate.
    force_push_with_lease: bool,

    /// Per-worktree base ref override. `None` = use the repo's default
    /// branch as the diff base. Flows into the dropdown's "Rebase from
    /// {base}" label and (downstream) the commit-graph diff base.
    base_ref: Option<String>,

    /// Per-worktree persistence layer; cloned for upserts after the user
    /// picks a base ref. `None` when the panel runs without a settings
    /// repo (test wiring); in that case the base ref still works in
    /// memory but doesn't survive restart.
    worktree_settings_repo: Option<WorktreeSettingsRepo>,

    /// Held for the async picker-fetch + branch-switch tasks that need a
    /// live `Repository` handle on the panel itself (the observer task
    /// already has its own clone).
    repo: Repository,

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
    pub branch_picker: Entity<BranchPicker>,

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
            worktree_settings_repo,
        } = cfg;
        // Read the persisted base ref synchronously — the repo handle
        // is local SQLite, ~microsecond cost. If the row is missing /
        // the read fails / no settings repo at all, fall through to
        // `None` (use the repo default). Errors are logged but never
        // raised; persistence is a nice-to-have, not a panel invariant.
        let initial_base_ref = load_initial_base_ref(worktree_settings_repo.as_ref(), &repo);
        let initial = state_rx.borrow().clone();
        let git_state = match &initial {
            PollState::Ready(s) => Some(s.clone()),
            _ => None,
        };
        let observer = Self::start_state_observer(state_rx, repo.clone(), cx);

        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
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

        // Picker entity is built once and reused across opens — the
        // owner-side callback (built below from a weak self-ref) reads
        // the picker's current mode at fire time and routes the user's
        // choice through `apply_picker_outcome`. The callback only fires
        // while the panel is alive because the picker holds it as a
        // `Box<dyn Fn>` and the weak capture short-circuits when `self`
        // is dropped.
        let panel_weak = cx.weak_entity();
        let on_pick: OnPick = Box::new(move |outcome, window, cx| {
            let _ = panel_weak.update(cx, |panel, cx| {
                // Read the live picker mode — the same picker entity
                // services both Switch and BaseRef surfaces and the
                // outcome's meaning depends on which one is active.
                let mode = panel.branch_picker.read(cx).mode();
                panel.apply_picker_outcome(outcome, mode, window, cx);
            });
        });
        let branch_picker = cx.new(|cx| {
            BranchPicker::new(
                PickerMode::Switch,
                on_pick,
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
        });

        Self {
            poll_state: initial,
            git_state,
            force_push_with_lease: false,
            base_ref: initial_base_ref,
            worktree_settings_repo,
            repo,
            scope: SourceControlScope::All,
            filter_query: String::new(),
            filter_input,
            in_flight_remote: Arc::new(std::sync::Mutex::new(None)),
            git_panel,
            diff_view,
            commit_area,
            commit_graph,
            branch_picker,
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
        repo: Repository,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                if rx.changed().await.is_err() {
                    return;
                }
                let state = rx.borrow_and_update().clone();
                // Decide whether to refresh the upstream-rewrite check
                // BEFORE the panel-update borrow. The check only matters
                // when local and upstream have actually diverged — pure
                // ahead-only or behind-only states are never lease
                // candidates, so we skip the (cached-but-still-locking)
                // backend call.
                let should_check_lease = matches!(
                    &state,
                    PollState::Ready(s)
                        if s.upstream.is_some() && s.ahead > 0 && s.behind > 0
                );
                if this
                    .update(cx, |panel, cx| {
                        if let PollState::Ready(ref s) = state {
                            panel.git_state = Some(s.clone());
                        }
                        panel.poll_state = state;
                        if !should_check_lease {
                            // Reset stale lease state immediately when we
                            // leave the diverged window — otherwise the
                            // dropdown would keep showing Force Push on a
                            // freshly-pulled branch.
                            panel.force_push_with_lease = false;
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                if should_check_lease {
                    refresh_force_push_with_lease(&repo, &this, cx).await;
                }
            }
        })
    }

    /// Build the inputs snapshot consumed by both the primary-action
    /// resolver (single-verb split button) and the dropdown resolver
    /// (full menu). Walks `git_state.files` once so both surfaces see the
    /// same staged/unstaged/conflict counts.
    fn build_primary_inputs(&self, cx: &Context<Self>) -> PrimaryActionInputs {
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
                    // Skip Ignored entries — they show up in `git status
                    // --ignored` but the user never thinks of them as
                    // "unstaged changes". Mirrors the filter in
                    // `changed_files::partition_files` so the primary
                    // button state stays consistent with the file-list
                    // sections rendered above it (otherwise a clean repo
                    // with one ignored `dist/` directory would render
                    // "No changes on this branch" alongside a "Stage All"
                    // button).
                    if matches!(f.index, IndexStatus::Ignored)
                        || matches!(f.worktree, WorktreeStatus::Ignored)
                    {
                        continue;
                    }
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

        // Derive the in-flight remote op directly from the commit area's
        // status (single source of truth; see `resolve_primary` for the
        // historical mutex note).
        let commit_status = self.commit_area.read(cx).status.clone();
        let in_flight_remote_kind = match &commit_status {
            commit_area::CommitStatus::Pushing => Some(RemoteOpKind::Push),
            commit_area::CommitStatus::Pulling => Some(RemoteOpKind::Pull),
            commit_area::CommitStatus::Syncing => Some(RemoteOpKind::Sync),
            commit_area::CommitStatus::Fetching => Some(RemoteOpKind::Fetch),
            _ => None,
        };
        // Sync the legacy mutex so any future caller sees a consistent
        // value — harmless if nothing else reads it.
        if let Ok(mut g) = self.in_flight_remote.lock() {
            *g = in_flight_remote_kind;
        }

        PrimaryActionInputs {
            staged_count,
            has_unstaged_changes: has_unstaged,
            has_partially_staged_changes: has_partial,
            has_message: self.commit_area.read(cx).has_message(cx),
            has_unresolved_conflicts: has_conflict,
            is_committing: matches!(commit_status, commit_area::CommitStatus::Committing),
            is_remote_operation_active: in_flight_remote_kind.is_some(),
            upstream_status: upstream,
            in_flight_remote_op_kind: in_flight_remote_kind,
        }
    }

    /// Build the dropdown-only inputs wrapper. `force_push_with_lease`
    /// is the cached result of `Repository::lease_status`, refreshed by
    /// the state observer whenever the branch is diverged from its
    /// upstream. `base_ref` is None until a configurable base ref lands;
    /// the PR-operation flag stays false until a hosted-review backend
    /// exists.
    fn build_dropdown_inputs(&self, cx: &Context<Self>) -> DropdownInputs {
        DropdownInputs {
            primary: self.build_primary_inputs(cx),
            force_push_with_lease: self.force_push_with_lease,
            base_ref: self.base_ref.clone(),
            is_pr_operation_active: false,
        }
    }

    fn resolve_primary(&self, cx: &Context<Self>) -> PrimaryAction {
        resolve_primary_action(&self.build_primary_inputs(cx))
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
        summary_row.child(actions)
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
                    .icon(Icon::default().path("icons/x.svg").size(px(sc_style::ICON)))
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

    /// Open the branch picker in Switch mode anchored under the toolbar
    /// branch chip. Branch list is fetched async; the popover opens
    /// immediately (empty list with the placeholder text), then populates
    /// when `list_branches` returns. `list_branches` is local and
    /// typically resolves in tens of milliseconds.
    fn open_switch_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let current = self.git_state.as_ref().and_then(|s| s.branch.clone());
        let picker = self.branch_picker.clone();
        cx.spawn_in(window, async move |_panel_weak, cx| {
            let candidates: Vec<String> = match repo.list_branches().await {
                Ok(bs) => {
                    let names: Vec<String> = bs.into_iter().map(|b| b.name).collect();
                    promote_current_branch(names, current.as_deref())
                }
                Err(err) => {
                    tracing::warn!(
                        target: "oximux_app::source_control",
                        error = %err,
                        "list_branches failed; opening picker with empty list",
                    );
                    Vec::new()
                }
            };
            let _ = picker.update_in(cx, |p, window, cx| {
                p.set_mode(PickerMode::Switch);
                p.open(
                    candidates,
                    current.clone(),
                    sc_style::PAD_H,
                    sc_style::TAB_H + sc_style::TOOLBAR_H,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Branch-picker callback router. Lives on the panel so the spawned
    /// closure stays tiny (just `panel.apply_picker_outcome(...)`) and
    /// the actual dispatch is testable without GPUI plumbing. `mode`
    /// is the picker's live mode at fire time — the same `Branch`
    /// outcome means "switch to it" in Switch mode but "use it as the
    /// diff base" in BaseRef mode.
    fn apply_picker_outcome(
        &mut self,
        outcome: PickerOutcome,
        mode: PickerMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (mode, outcome) {
            (PickerMode::Switch, PickerOutcome::Branch(name)) => self.switch_to_branch(name, cx),
            (PickerMode::Switch, PickerOutcome::CreateFromHead(name)) => {
                self.create_branch_and_switch(name, cx)
            }
            (PickerMode::BaseRef, PickerOutcome::Branch(name)) => {
                self.set_base_ref(Some(name), cx)
            }
            (PickerMode::BaseRef, PickerOutcome::UseRepoDefault) => self.set_base_ref(None, cx),
            // Impossible combinations per `build_rows` invariants:
            // Switch mode never emits `UseRepoDefault`; BaseRef mode
            // never emits `CreateFromHead`. Log defensively so a future
            // refactor that loosens those invariants is loud.
            (mode, outcome) => {
                tracing::warn!(
                    target: "oximux_app::source_control",
                    ?mode,
                    ?outcome,
                    "branch picker emitted an outcome that's invalid for the active mode; ignoring",
                );
            }
        }
    }

    /// Persist a new base ref selection — both in memory (so the
    /// dropdown updates on the next render) and on disk (so it survives
    /// app restart). Persistence errors are logged but never raised:
    /// the in-memory change still wins so the picker UX isn't held
    /// hostage to SQLite hiccups.
    fn set_base_ref(&mut self, value: Option<String>, cx: &mut Context<Self>) {
        self.base_ref = value.clone();
        cx.notify();
        let Some(ref settings_repo) = self.worktree_settings_repo else {
            return;
        };
        let workspace_id = self.repo.workdir().to_string_lossy().to_string();
        if let Err(err) = merge_base_ref_into_settings(settings_repo, &workspace_id, value) {
            tracing::warn!(
                target: "oximux_app::source_control",
                error = %err,
                workspace_id = %workspace_id,
                "worktree_settings.upsert failed; base ref change won't survive restart",
            );
        }
    }

    /// Open the branch picker in BaseRef mode anchored under the
    /// toolbar. Populates from `list_remote_branches` (5 s cache
    /// absorbs spam clicks). The picker's "(repo default)" row maps
    /// to `set_base_ref(None)`; any other row maps to
    /// `set_base_ref(Some(name))`.
    ///
    /// Anchor uses the same left offset as the Switch picker rather
    /// than the settings button's right-edge position — the picker
    /// card is wide enough (`CARD_WIDTH = 280`) to cover the button
    /// regardless, but the precise anchor wants the source button's
    /// measured bounds, which GPUI doesn't expose synchronously from
    /// the click handler. Worth revisiting once `Bounds` capture from
    /// the toolbar's interactive elements becomes ergonomic.
    fn open_base_ref_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let picker = self.branch_picker.clone();
        let current_base = self.base_ref.clone();
        cx.spawn_in(window, async move |_panel_weak, cx| {
            let candidates: Vec<String> = match repo.list_remote_branches(false).await {
                Ok(bs) => bs.into_iter().map(|b| b.name).collect(),
                Err(err) => {
                    tracing::warn!(
                        target: "oximux_app::source_control",
                        error = %err,
                        "list_remote_branches failed; opening BaseRef picker with empty list",
                    );
                    Vec::new()
                }
            };
            let _ = picker.update_in(cx, |p, window, cx| {
                p.set_mode(PickerMode::BaseRef);
                p.open(
                    candidates,
                    current_base,
                    sc_style::PAD_H,
                    sc_style::TAB_H + sc_style::TOOLBAR_H,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    /// Dispatch `repo.switch_branch(name)`. Success leaves status
    /// unchanged (the StatusPoller's next tick reflects the new branch).
    /// Failure surfaces via `CommitStatus::Failed("switch", err)` so the
    /// existing status row in the commit area shows the error without
    /// needing a separate toast surface.
    fn switch_to_branch(&mut self, name: String, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let commit_area = self.commit_area.clone();
        cx.spawn(async move |_panel_weak, cx| {
            let result = repo.switch_branch(&name).await;
            commit_area.update(cx, |area, cx| {
                write_branch_op_status(area, "switch", &name, result, cx);
            });
        })
        .detach();
    }

    /// Chain `create_branch(name, Some("HEAD"))` followed by
    /// `switch_branch(name)`. `create_branch` does NOT check out by
    /// itself; the second call is mandatory for the user-visible "Create
    /// branch from HEAD" semantics. If creation fails (name collision,
    /// invalid characters), we bail before the switch so we don't leave
    /// the user on an unintended branch.
    fn create_branch_and_switch(&mut self, name: String, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let commit_area = self.commit_area.clone();
        cx.spawn(async move |_panel_weak, cx| {
            let create_result = repo.create_branch(&name, Some("HEAD")).await;
            if create_result.is_err() {
                commit_area.update(cx, |area, cx| {
                    write_branch_op_status(area, "create branch", &name, create_result, cx);
                });
                return;
            }
            let switch_result = repo.switch_branch(&name).await;
            commit_area.update(cx, |area, cx| {
                write_branch_op_status(area, "switch", &name, switch_result, cx);
            });
        })
        .detach();
    }
}

impl Render for SourceControlPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let action = self.resolve_primary(cx);
        let dropdown_inputs = self.build_dropdown_inputs(cx);
        let picker = self.branch_picker.clone();

        // Snapshot per-section render outputs as `AnyElement` so we can drop
        // each borrow of `cx` before composing the final tree.
        let scope_tabs: AnyElement = self.render_scope_tabs(cx).into_any_element();
        let toolbar: AnyElement = self.render_branch_toolbar(cx).into_any_element();
        let filter_row: AnyElement = self.render_filter_row(cx).into_any_element();
        let commit_area_render: AnyElement = self.commit_area.clone().update(cx, |a, cx| {
            a.render(&action, dropdown_inputs, cx).into_any_element()
        });

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
        //
        // The inline sidebar `DiffView` is no longer mounted — diffs open
        // as real editor tabs in the main pane via `OnOpenDiff` (see
        // `WorkspaceRoot::build_on_open_diff_callback`). The `diff_view`
        // entity is kept on `Self` to avoid churning the constructor
        // signature; it stays in `Empty` state and never renders.
        let files_block = div()
            .flex()
            .flex_1()
            .min_h(px(0.0))
            .flex_col()
            .overflow_hidden()
            .child(self.git_panel.clone());

        // Layout order: scope tabs → toolbar → filter →
        // **files (flex_1)** → **commit area docked at bottom** → graph.
        // The previous order rendered the commit textarea mid-panel,
        // floating above the file list when the list was short. Anchoring
        // the composer to the bottom keeps the cockpit's "type-and-commit"
        // surface in a stable position regardless of file-list height.
        let mut body = div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(theme.bg_panel)
            .child(scope_tabs)
            .child(toolbar)
            .child(filter_row)
            .child(files_block)
            .child(commit_area_render);
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
        // `.relative()` makes the body the positioning ancestor for the
        // branch picker's full-overlay (`absolute().inset_0()` inside its
        // own render). That confines click-outside dismiss to the panel
        // surface — clicks elsewhere in the cockpit don't accidentally
        // trigger it.
        div()
            .relative()
            .w_full()
            .h_full()
            .child(body)
            .child(picker)
    }
}

/// Write the per-workspace base ref into the V006 `worktree_settings`
/// row, preserving sibling fields (`commit_draft`, `view_mode_override`)
/// that other surfaces own. Delegates to
/// [`WorktreeSettingsRepo::modify`], which fuses the read+write under
/// one connection lock so concurrent writers (view-mode toggle,
/// commit-draft debounce) cannot interleave a stale read.
pub fn merge_base_ref_into_settings(
    settings_repo: &WorktreeSettingsRepo,
    workspace_id: &str,
    value: Option<String>,
) -> Result<(), oximux_storage::StorageError> {
    settings_repo.modify(workspace_id, |s| s.base_ref = value)
}

/// Read the persisted base ref for this workspace from the V006
/// `worktree_settings` table. Falls through to `None` (= use the repo
/// default) whenever the read fails, the row is absent, or no settings
/// repo was provided (test wiring). Errors are logged at warn but
/// never raised — a missing initial value is not a startup-blocker.
fn load_initial_base_ref(
    settings_repo: Option<&WorktreeSettingsRepo>,
    repo: &Repository,
) -> Option<String> {
    let sr = settings_repo?;
    let workspace_id = repo.workdir().to_string_lossy().to_string();
    match sr.get(&workspace_id) {
        Ok(Some(settings)) => settings.base_ref,
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                target: "oximux_app::source_control",
                error = %err,
                workspace_id = %workspace_id,
                "worktree_settings.get failed; base ref defaults to repo default",
            );
            None
        }
    }
}

/// Write the result of a branch-level operation (switch / create-branch)
/// to the commit area's status row WITHOUT clobbering an in-flight
/// commit / push / pull / sync / fetch op's own status.
///
/// The commit area's `in_flight` AtomicBool is owned by `commit_ops`
/// for the lifetime of those ops. Reading it lets the branch-level path
/// detect "someone else's status row" and step around — writing
/// `CommitStatus::Idle` on a successful switch while a push is racing
/// would silently swallow whatever the push reports next. The error
/// case is also guarded so a slow switch failing AFTER a commit failed
/// doesn't overwrite the commit error.
///
/// When the lane is busy we still log the result at warn level so the
/// outcome isn't completely invisible to operators inspecting traces.
fn write_branch_op_status(
    area: &mut CommitArea,
    verb: &'static str,
    branch: &str,
    result: oximux_git::Result<()>,
    cx: &mut Context<CommitArea>,
) {
    let busy = area
        .in_flight
        .load(std::sync::atomic::Ordering::Relaxed);
    if busy {
        if let Err(ref err) = result {
            tracing::warn!(
                target: "oximux_app::source_control",
                verb = %verb,
                branch = %branch,
                error = %err,
                "branch op failed while commit-area was in flight — status row owned elsewhere",
            );
        }
        return;
    }
    match result {
        Ok(()) => area.status = CommitStatus::Idle,
        Err(err) => area.status = CommitStatus::Failed(verb.to_string(), err.to_string()),
    }
    cx.notify();
}

/// Move `current` (if present) to position 0 of `names`. Used to pin
/// the currently-checked-out branch to the top of the Switch-picker
/// list so the user can immediately see where they already are.
/// Pure for unit-test coverage; the side-effect-free shape also makes
/// it cheap to reuse from future call sites (e.g. a "recent branches"
/// pre-filter pass).
pub fn promote_current_branch(mut names: Vec<String>, current: Option<&str>) -> Vec<String> {
    if let Some(c) = current
        && let Some(pos) = names.iter().position(|n| n == c)
        && pos > 0
    {
        let cur = names.remove(pos);
        names.insert(0, cur);
    }
    names
}

/// Refresh the cached `force_push_with_lease` flag on the panel.
///
/// Calls `Repository::lease_status(false)` (the 30 s cache absorbs
/// duplicate calls within that window) and writes the boolean back into
/// the panel state via `update`. Errors from the backend are logged at
/// `warn` and treated as `false` — better to surface a regular Push row
/// than to silently encourage a force-push the backend couldn't confirm
/// safe.
async fn refresh_force_push_with_lease(
    repo: &Repository,
    this: &gpui::WeakEntity<SourceControlPanel>,
    cx: &mut gpui::AsyncApp,
) {
    let lease_ok = match repo.lease_status(false).await {
        Ok(status) => status.behind_is_patch_equivalent,
        Err(err) => {
            tracing::warn!(
                target: "oximux_app::source_control",
                error = %err,
                "lease_status query failed; falling back to non-force-push label"
            );
            false
        }
    };
    let _ = this.update(cx, |panel, cx| {
        if panel.force_push_with_lease != lease_ok {
            panel.force_push_with_lease = lease_ok;
            cx.notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::promote_current_branch;

    #[test]
    fn promote_current_to_front_when_present() {
        let names = vec!["main".into(), "feat-a".into(), "feat-b".into()];
        let out = promote_current_branch(names, Some("feat-a"));
        assert_eq!(out, vec!["feat-a", "main", "feat-b"]);
    }

    #[test]
    fn promote_no_op_when_current_already_at_front() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, Some("main"));
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_no_op_when_current_not_in_list() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, Some("missing"));
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_no_op_when_current_is_none() {
        let names = vec!["main".into(), "feat-a".into()];
        let out = promote_current_branch(names, None);
        assert_eq!(out, vec!["main", "feat-a"]);
    }

    #[test]
    fn promote_empty_input_returns_empty() {
        let out = promote_current_branch(Vec::<String>::new(), Some("main"));
        assert!(out.is_empty());
    }
}
