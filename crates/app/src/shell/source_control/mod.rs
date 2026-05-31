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
pub mod picker_wiring;
pub mod primary_action;
pub mod scope;
pub mod settings_persistence;
pub mod style;
pub mod toolbar;
pub mod tree;

// Re-export so external callers (notably the integration tests at
// `crates/app/tests/sc_base_ref_persistence.rs` and
// `crates/app/tests/sc_commit_draft_persistence.rs`) keep resolving
// against `oximux_app::shell::source_control::{symbol}` rather than
// reaching into the deeper `settings_persistence` path.
pub use settings_persistence::{load_initial_commit_draft, merge_base_ref_into_settings};

use std::sync::Arc;

use gpui::{
    AnyElement, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, px,
};
use gpui_component::input::{InputEvent, InputState};
use oximux_core::GitState;
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::WorktreeSettingsRepo;
use tokio::sync::watch;

use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::GitPanel;
use crate::shell::source_control::branch_picker::{BranchPicker, OnPick, PickerMode};
use crate::shell::source_control::commit_area::CommitArea;
use crate::shell::source_control::dropdown_items::DropdownInputs;
use crate::shell::source_control::graph::CommitGraph;
use crate::shell::source_control::primary_action::{
    PrimaryAction, PrimaryActionInputs, RemoteOpKind, UpstreamStatus, resolve_primary_action,
};
use crate::shell::source_control::scope::SourceControlScope;
use crate::shell::source_control::settings_persistence::load_initial_base_ref;

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
            CommitArea::new(
                repo.clone(),
                worktree_settings_repo.clone(),
                theme,
                density,
                typography.clone(),
                window,
                cx,
            )
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

}

impl Render for SourceControlPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let action = self.resolve_primary(cx);
        let dropdown_inputs = self.build_dropdown_inputs(cx);
        // Snapshot the conflict flag before we move `dropdown_inputs`
        // into `commit_area_render` below — gates the composer-vs-
        // placeholder swap a few lines down.
        let has_conflict = dropdown_inputs.primary.has_unresolved_conflicts;
        let picker = self.branch_picker.clone();

        // Snapshot per-section render outputs as `AnyElement` so we can drop
        // each borrow of `cx` before composing the final tree.
        let scope_tabs: AnyElement = self.render_scope_tabs(cx).into_any_element();
        let toolbar: AnyElement = self.render_branch_toolbar(cx).into_any_element();
        let filter_row: AnyElement = self.render_filter_row(cx).into_any_element();
        // Suppress the composer entirely under unresolved conflicts.
        // Committing on top of conflict markers would persist them into
        // the tree; force the user to resolve first. The real conflict
        // banner (resolve / abort merge actions) lands in a follow-up
        // phase — for now the placeholder is a one-line muted hint so
        // the user sees WHY the composer is gone instead of an
        // unexplained blank.
        let commit_area_render: AnyElement = if has_conflict {
            render_conflict_placeholder(theme).into_any_element()
        } else {
            self.commit_area.clone().update(cx, |a, cx| {
                a.render(&action, dropdown_inputs, cx).into_any_element()
            })
        };

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

/// One-line muted placeholder shown in the composer slot while
/// `has_unresolved_conflicts` is true. Keeps the layout stable (a
/// disappearing composer would jump the file list around as the user
/// resolves files) and gives the user a hint that the composer is
/// intentionally hidden rather than broken. The full conflict banner
/// — with resolve / abort merge action buttons — lands in a later
/// phase; this is the minimum that closes the safety hole.
fn render_conflict_placeholder(theme: oximux_settings::Theme) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w_full()
        .px(px(style::PAD_H))
        .py(px(style::PAD_V))
        .text_size(px(style::META_TEXT))
        .text_color(theme.fg_subtle)
        .child("Resolve conflicts before committing")
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
