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
pub mod conflict_banner;
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
use crate::shell::stash_panel::StashPanel;

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
    /// Host callback to open a file in the main pane. Drives the
    /// "Open all in editor" button on the ConflictSummaryCard. `None`
    /// in test wiring → the button stays disabled with a "wiring
    /// unavailable" tooltip rather than silently no-op'ing.
    pub on_open_file: Option<crate::shell::file_tree_view::OnOpenFile>,
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

    /// Cached result of `Repository::current_operation()` — drives the
    /// amber OperationBanner above the file list. Refreshed by the
    /// state observer once per poll tick (cheap: 5 fs::metadata reads
    /// on the worktree's `.git/`), NOT once per render. Per-render
    /// recomputation would burn the fs cost on every keystroke /
    /// scope-tab click / unrelated cx.notify, violating phase-08's
    /// non-functional req that detection runs once per poll tick.
    current_op: Option<oximux_core::GitOperation>,

    /// Per-worktree persistence layer; cloned for upserts after the user
    /// picks a base ref. `None` when the panel runs without a settings
    /// repo (test wiring); in that case the base ref still works in
    /// memory but doesn't survive restart.
    worktree_settings_repo: Option<WorktreeSettingsRepo>,

    /// Host callback to open a file in the main pane. Captured for
    /// the ConflictSummaryCard's "Open all in editor" button; the
    /// async fetch of `list_conflicting_paths` fires it once per
    /// path returned.
    on_open_file: Option<crate::shell::file_tree_view::OnOpenFile>,

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
    /// Stash list section docked between the commit area and the graph.
    /// Default collapsed (power-user surface; see `StashPanel::new`).
    /// `pub(crate)` so the host workspace can `cx.subscribe` to
    /// `PushStashRequested` and mount the push-stash dialog.
    pub(crate) stash_panel: Entity<StashPanel>,

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
            on_open_file,
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
        // Detect any in-progress git op at mount time so the banner
        // shows immediately if the user opens OxiMux mid-rebase
        // rather than waiting for the first poll tick.
        let initial_op = repo.current_operation();
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
        let stash_panel =
            cx.new(|cx| StashPanel::new(repo.clone(), theme, density, typography.clone(), cx));

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
            current_op: initial_op,
            on_open_file,
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
            stash_panel,
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

    /// Fetch the current set of conflicting paths and dispatch the
    /// host's `OnOpenFile` callback for each — opens every
    /// conflicting file in its own editor tab. Async because
    /// `list_conflicting_paths` shells out to `git diff
    /// --diff-filter=U`; the per-path open dispatches inside
    /// `update_in` on the panel's foreground executor so each click
    /// lands as a real Window event.
    ///
    /// Paths from git are workdir-relative; the join with
    /// `repo.workdir()` produces the absolute path `OnOpenFile`'s
    /// contract requires. No-op (warn-logged) when the panel has no
    /// host callback wired (test-only case).
    /// Fetch the current set of conflicting paths and dispatch the
    /// host's `OnOpenFile` callback for each — opens every
    /// conflicting file in its own editor tab.
    ///
    /// Two-stage async to thread the tokio work + gpui dispatch
    /// correctly (same pattern as `commit_ops::run_commit`): the
    /// `git diff --diff-filter=U` shellout runs on the live tokio
    /// runtime via `Handle::try_current().spawn(...)`, sends the
    /// Vec<PathBuf> back through a `oneshot`, then a
    /// `cx.spawn_in(window, ...)` task on the gpui executor awaits
    /// the oneshot and fires `on_open` per path inside an
    /// `update_in` block. Mixing the two runtimes directly (e.g.
    /// awaiting a tokio future inside `cx.spawn_in`) silently no-ops
    /// in headless test contexts; the channel hand-off works in both
    /// production and tests.
    ///
    /// Paths from git are workdir-relative; the join with
    /// `repo.workdir()` produces the absolute path `OnOpenFile`'s
    /// contract requires. No-op (warn-logged) when the panel has no
    /// host callback wired (test-only case) or when no tokio runtime
    /// is entered.
    ///
    /// `pub` (not `pub(super)`) so integration tests at
    /// `crates/app/tests/sc_open_all_conflicts.rs` can drive the
    /// method directly without going through a Button click event.
    pub fn open_all_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(on_open) = self.on_open_file.clone() else {
            tracing::warn!(
                target: "oximux_app::source_control",
                "open_all_conflicts: no on_open_file callback wired; click ignored",
            );
            return;
        };
        let repo = self.repo.clone();
        let workdir = repo.workdir().to_path_buf();
        let (tx, rx) = tokio::sync::oneshot::channel::<oximux_git::Result<Vec<std::path::PathBuf>>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = tx.send(repo.list_conflicting_paths().await);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::source_control",
                    "open_all_conflicts: no tokio runtime entered; skipping",
                );
                return;
            }
        }
        cx.spawn_in(window, async move |_panel_weak, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let paths = match result {
                Ok(p) => p,
                Err(err) => {
                    tracing::warn!(
                        target: "oximux_app::source_control",
                        error = %err,
                        "list_conflicting_paths failed; open-all-conflicts skipped",
                    );
                    return;
                }
            };
            let _ = cx.update(|window, app| {
                for rel in paths {
                    let absolute = workdir.join(&rel);
                    on_open(absolute, window, app);
                }
            });
        })
        .detach();
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
                // Refresh the cached in-progress git op before the
                // panel update so render sees the new value on the
                // same tick. Stat-only — microsecond cost on APFS,
                // tolerable on a poll tick (vs per-render, which
                // would burn it on every keystroke).
                let op = repo.current_operation();
                if this
                    .update(cx, |panel, cx| {
                        if let PollState::Ready(ref s) = state {
                            panel.git_state = Some(s.clone());
                        }
                        panel.poll_state = state;
                        panel.current_op = op;
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
        // into `commit_area_render` below — gates the composer
        // suppression a few lines down.
        let has_conflict = dropdown_inputs.primary.has_unresolved_conflicts;
        // Count files whose status is Unmerged for the
        // ConflictSummaryCard. Mirrors `build_primary_inputs`'s
        // detection but tallies rather than collapsing to bool.
        let conflict_count = self
            .git_state
            .as_ref()
            .map(|s| {
                use oximux_core::{IndexStatus, WorktreeStatus};
                s.files
                    .iter()
                    .filter(|f| {
                        matches!(f.index, IndexStatus::Unmerged)
                            || matches!(f.worktree, WorktreeStatus::Unmerged)
                    })
                    .count()
            })
            .unwrap_or(0);
        // Read the cached in-progress git op (`self.current_op`).
        // `Repository::current_operation` is refreshed by
        // `start_state_observer` once per poll tick — the panel
        // re-renders many times per tick (filter keystrokes,
        // unrelated cx.notify), so caching keeps the per-render
        // path free of the 5 fs::metadata calls.
        let current_op = self.current_op;
        let picker = self.branch_picker.clone();

        // Snapshot per-section render outputs as `AnyElement` so we can drop
        // each borrow of `cx` before composing the final tree.
        let scope_tabs: AnyElement = self.render_scope_tabs(cx).into_any_element();
        let toolbar: AnyElement = self.render_branch_toolbar(cx).into_any_element();
        let filter_row: AnyElement = self.render_filter_row(cx).into_any_element();
        // Conflict cards: the summary card sits ABOVE the operation
        // banner when both apply — the more granular "files in
        // conflict" surface dominates the broader "operation
        // pending" surface so the user's eye lands on what they
        // can act on first.
        // Capture the panel weak ref + on_open availability for the
        // banner's click handler. Disabled (with explanatory
        // tooltip) when no host callback is wired — test wiring
        // path, production always supplies one.
        let panel_weak = cx.weak_entity();
        let open_all_enabled = self.on_open_file.is_some();
        let conflict_card: Option<AnyElement> = conflict_banner::render_conflict_summary_card(
            conflict_count,
            theme,
            open_all_enabled,
            move |window, app| {
                let _ = panel_weak.update(app, |panel, cx| {
                    panel.open_all_conflicts(window, cx);
                });
            },
        )
        .map(IntoElement::into_any_element);
        let operation_banner: Option<AnyElement> =
            conflict_banner::render_operation_banner(current_op, theme)
                .map(IntoElement::into_any_element);
        // Suppress the composer entirely under unresolved conflicts.
        // Committing on top of conflict markers would persist them
        // into the tree; the ConflictSummaryCard above explains why
        // the slot is empty, so the composer slot just collapses
        // rather than rendering a muted placeholder.
        let commit_area_render: Option<AnyElement> = if has_conflict {
            None
        } else {
            Some(self.commit_area.clone().update(cx, |a, cx| {
                a.render(&action, dropdown_inputs, cx).into_any_element()
            }))
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
            .children(conflict_card)
            .children(operation_banner)
            .child(files_block)
            .children(commit_area_render)
            // Stash list docked above the graph (or at the very bottom
            // when the scope hides the graph). Always-mounted entity;
            // collapsed by default — see `StashPanel::is_collapsed`.
            .child(self.stash_panel.clone());
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
