//! Git changed-files panel — first UI surface consuming the `oximux-git`
//! wrappers shipped in Phase 2 steps 1–7. Subscribes to a `StatusPoller`
//! receiver, partitions `GitState::files` into three sections, dispatches
//! file-level stage / unstage / revert actions.
//!
//! Diff view (`OpenDiff`), hunk-level ops, commit dialog, stash UI, and the
//! confirm-dialog for revert all land in later Phase 2 steps. Step 8 is the
//! data-flow skeleton + section rendering only.
//!
//! Runtime: the stage / unstage handlers use `tokio::runtime::Handle::try_current`
//! to spawn into whichever tokio runtime the parent shell entered. If no
//! runtime is entered (e.g. the GPUI smoke test), the handler logs and no-ops
//! instead of panicking. Step 14 wires runtime setup at the shell mount point.

pub mod bulk_action_bar;
pub mod changed_files;
pub mod discard_confirm;
pub mod discard_ops;
pub mod range_select;
pub mod row_actions;
pub mod row_renderer;
pub mod selection;
pub mod tree_render;

pub use discard_ops::{DiscardRequest, DiscardRequested, DiscardScope};

use crate::actions::{RevertFile, StageFile, UnstageFile};
use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::changed_files::{RenderCtx, partition_files, render_sections};
use crate::shell::source_control::filter::filter_files;
use crate::shell::source_control::style as sc_style;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollHandle, StatefulInteractiveElement, Styled, Task, Window, div, px,
};
use gpui_component::scroll::ScrollableElement as _;
use oximux_core::{GitState, ViewMode};
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::WorktreeSettingsRepo;
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::sync::watch;

use crate::shell::file_tree_view::OnOpenDiff;

pub struct GitPanel {
    /// `pub(super)` so [`selection`] can clone the handle to spawn
    /// vectorised stage / unstage ops in `bulk_stage_selected` /
    /// `bulk_unstage_selected`. No other sibling submodule mutates the
    /// field; the inner `RwLock` cache stays internal to `oximux-git`.
    pub(super) repo: Repository,
    /// Last `Ready` payload observed on the watch channel. Cleared back to
    /// `None` on a fresh `Loading` or `Failed` transition so the render path
    /// can distinguish "no data yet" from "no changes".
    ///
    /// `pub(super)` so [`selection::flat_visible_paths`] can recompute
    /// the partition without taking a per-render snapshot.
    pub(super) git_state: Option<GitState>,
    poll_state: PollState,
    /// Multi-select path set. Bare click replaces with one entry,
    /// Cmd-click toggles, Shift+Click replaces with the range from
    /// [`last_clicked`] to the click target in flat render order.
    /// Selection survives poll ticks because it's keyed by `PathBuf` —
    /// see [`selection`] for the routing methods.
    ///
    /// [`last_clicked`]: Self::last_clicked
    pub(super) selected: HashSet<PathBuf>,
    /// Anchor for Shift+Click range expansion. Bare / Cmd-click updates
    /// it; Shift+Click leaves it alone (so a second Shift+Click
    /// re-anchors from the same starting row, matching Finder list
    /// semantics). `None` only before the first row interaction or
    /// after [`selection::GitPanel::clear_selection`].
    pub(super) last_clicked: Option<PathBuf>,
    /// Held but no longer driven — kept on the struct so the
    /// constructor signature doesn't churn while the inline sidebar
    /// `DiffView` is dormant. Diffs now open as real editor tabs in
    /// the main pane via `on_open: OnOpenDiff`. A follow-up can drop
    /// the field + constructor arg + the `DiffView` mount in
    /// `right_sidebar/mod.rs`.
    #[allow(dead_code)]
    diff_view: Option<Entity<DiffView>>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Case-insensitive substring filter applied to `git_state.files` before
    /// partitioning. Empty string disables filtering. Owner: `SourceControlPanel`
    /// updates this via `set_filter` as the user types in the filter input.
    ///
    /// `pub(super)` so [`selection::flat_visible_paths`] can apply the
    /// same filter when computing the Shift+Click range.
    pub(super) filter_query: String,
    /// Section names (e.g. "STAGED CHANGES") whose body is currently hidden.
    /// Toggled by clicking the section header. Default: all expanded.
    ///
    /// `pub(super)` so [`selection::flat_visible_paths`] can exclude
    /// collapsed-section paths from Shift+Click range computation —
    /// otherwise a range spanning a collapsed section silently selects
    /// invisible rows.
    pub(super) collapsed_sections: HashSet<&'static str>,
    /// Scroll position for the static sections list. Wired through `track_scroll`
    /// on the inner overflow region and consumed by `vertical_scrollbar` so the
    /// thumb actually moves as the user scrolls.
    scroll_handle: ScrollHandle,
    /// Drop cancels the receiver-watching task (mirrors `_poll_task` /
    /// `_blink_task` lifetime semantics in `TerminalView`).
    _watch_task: Task<()>,
    /// Host callback: open a read-only diff tab in the active project's
    /// pane group for the clicked file (with the staged-vs-unstaged
    /// discriminator). `None` in test wiring silently no-ops the click;
    /// the existing inline sidebar `DiffView` keeps working as a glanceable
    /// summary. Routing through `OnOpenDiff` (not `OnOpenFile`) means SCM
    /// clicks land in a tab that is explicitly a diff — no risk of
    /// confusing the diff with an editable text buffer.
    pub(crate) on_open: Option<OnOpenDiff>,
    /// In-flight discard request awaiting user confirmation. `Some` from
    /// the moment the user clicks the revert icon (or hits the revert
    /// chord) until `confirmed_discard_path` runs or
    /// `clear_pending_discard` is called. The shell host observes this
    /// field and mounts a `ConfirmDialog`.
    pending_discard: Option<DiscardRequest>,
    /// Paths whose `confirmed_discard_path` op is in flight on the
    /// tokio runtime. The row renderer reads this set to swap the
    /// revert icon for a spinner.
    in_flight_discards: HashSet<PathBuf>,
    /// `true` while a vectorised `bulk_stage_selected` /
    /// `bulk_unstage_selected` op is on the tokio runtime. The
    /// [`bulk_action_bar`] reads this to swap the count badge for a
    /// spinner and disable both action buttons until the op resolves.
    ///
    /// `pub(super)` so [`selection`] can flip the flag from its
    /// completion handler and `bulk_action_bar` can read it during
    /// render.
    ///
    /// [`bulk_action_bar`]: crate::shell::git_panel::bulk_action_bar
    pub(super) bulk_op_in_flight: bool,
    /// Section-rendering mode. `Flat` (default) lists every file in
    /// per-status sections; `Tree` groups files under their directory
    /// ancestors. Toggled by the toolbar button via [`cycle_view_mode`];
    /// per-worktree persistence rides through [`worktree_settings_repo`]
    /// keyed by the repo's workdir path.
    ///
    /// `pub(super)` so `changed_files` can branch on it without exposing
    /// a getter for every read.
    ///
    /// [`cycle_view_mode`]: Self::cycle_view_mode
    /// [`worktree_settings_repo`]: Self::worktree_settings_repo
    pub(super) view_mode: ViewMode,
    /// Folder paths (relative to the worktree root) whose subtree is
    /// hidden in Tree mode. Toggled by clicking a folder row; ignored
    /// in Flat mode. Defaults to all-expanded for a fresh panel — folder
    /// state is intentionally session-scoped (an app restart re-expands
    /// everything; only `view_mode` itself survives).
    ///
    /// `pub(super)` so the tree-render path can read it without copying.
    pub(super) collapsed_dirs: HashSet<PathBuf>,
    /// Handle to the per-worktree settings repo. `Some` in production
    /// wiring; `None` in test scaffolding that doesn't spin up a DB.
    /// All reads/writes use the worktree's absolute path as the key so
    /// the same panel re-opened in the same worktree gets the same
    /// `view_mode` back.
    worktree_settings_repo: Option<WorktreeSettingsRepo>,
}

// `DiscardRequest`, `DiscardScope`, `DiscardRequested`, and the
// `discard_*` / `confirmed_discard_*` methods live in
// [`crate::shell::git_panel::discard_ops`] so this module stays under
// the 800-LOC fail cap as Phase 03 Slice C piles on the area-discard
// branches. The types are re-exported at module scope above.

impl GitPanel {
    /// Build the panel. `state_rx` is `StatusPoller::subscribe()`; the caller
    /// owns the poller so the same receiver can fan out to sidebar / status
    /// bar consumers later.
    ///
    /// `worktree_settings_repo` is the per-worktree V006 settings repo;
    /// `None` in test wiring keeps `view_mode` at the in-memory default
    /// (`Flat`) and silently no-ops the persistence write on toggle.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Repository,
        state_rx: watch::Receiver<PollState>,
        diff_view: Option<Entity<DiffView>>,
        theme: Theme,
        density: Density,
        typography: Typography,
        on_open: Option<OnOpenDiff>,
        worktree_settings_repo: Option<WorktreeSettingsRepo>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let initial = state_rx.borrow().clone();
        let git_state = match &initial {
            PollState::Ready(s) => Some(s.clone()),
            _ => None,
        };
        let view_mode = initial_view_mode(&repo, worktree_settings_repo.as_ref());
        let watch_task = Self::start_watch_task(state_rx, cx);
        Self {
            repo,
            git_state,
            poll_state: initial,
            selected: HashSet::new(),
            last_clicked: None,
            diff_view,
            focus_handle,
            theme,
            density,
            typography,
            filter_query: String::new(),
            collapsed_sections: HashSet::new(),
            scroll_handle: ScrollHandle::new(),
            _watch_task: watch_task,
            on_open,
            pending_discard: None,
            in_flight_discards: HashSet::new(),
            bulk_op_in_flight: false,
            view_mode,
            collapsed_dirs: HashSet::new(),
            worktree_settings_repo,
        }
    }

    /// Current section-rendering mode. Read by the SCM toolbar to flip
    /// the toggle button's icon + tooltip between Tree-affordance and
    /// Flat-affordance copies.
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Flip Flat ↔ Tree and persist the new mode to the per-worktree
    /// settings repo. The persist path runs through
    /// `WorktreeSettingsRepo::modify`, which fuses the read+write under
    /// one connection lock so sibling fields (`base_ref`,
    /// `commit_draft`) survive untouched even when other writers race.
    /// Storage errors are logged at `warn!` — a missed write only loses
    /// the preference for the next launch, never blocks the toggle.
    pub fn cycle_view_mode(&mut self, cx: &mut Context<Self>) {
        self.view_mode = self.view_mode.toggled();
        if let Some(settings_repo) = self.worktree_settings_repo.clone() {
            let key = self.repo.workdir().to_string_lossy().into_owned();
            let next = self.view_mode.as_str().to_string();
            if let Err(e) = settings_repo.modify(&key, |s| {
                s.view_mode_override = Some(next);
            }) {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    error = %e,
                    key = %key,
                    "view_mode upsert failed; preference will reset on next launch"
                );
            }
        }
        cx.notify();
    }

    /// Update the changed-files filter. Pass the raw input value; empty /
    /// whitespace-only disables filtering. Caller `cx.notify`-ing is the
    /// usual path because GitPanel itself isn't tracked by the input.
    pub fn set_filter(&mut self, query: String, cx: &mut Context<Self>) {
        if self.filter_query != query {
            self.filter_query = query;
            cx.notify();
        }
    }

    /// Toggle a section's collapsed state. `name` matches the section title
    /// passed to `render_sections` (e.g. "STAGED CHANGES").
    pub(crate) fn toggle_section(&mut self, name: &'static str, cx: &mut Context<Self>) {
        if !self.collapsed_sections.remove(name) {
            self.collapsed_sections.insert(name);
        }
        cx.notify();
    }

    /// Toggle a Tree-mode folder's collapsed state. Called by
    /// [`tree_render::folder_row`]'s click handler. Folder state is
    /// session-scoped — never persisted, the next app launch always
    /// starts with all folders expanded.
    ///
    /// [`tree_render::folder_row`]: crate::shell::git_panel::tree_render
    pub(crate) fn toggle_collapsed_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&path) {
            self.collapsed_dirs.insert(path);
        }
        cx.notify();
    }

    fn start_watch_task(mut rx: watch::Receiver<PollState>, cx: &mut Context<Self>) -> Task<()> {
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
                        } else {
                            panel.git_state = None;
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

    /// `S` chord — stage every path in [`selected`]. Routes through the
    /// same vectorised `repo.stage_paths` backend as the hover-action
    /// single-path stage, so a 1-row selection is indistinguishable from
    /// a hover stage. No-op when nothing is selected.
    ///
    /// [`selected`]: Self::selected
    fn on_stage_file(&mut self, _: &StageFile, _window: &mut Window, cx: &mut Context<Self>) {
        self.bulk_stage_selected(cx);
    }

    /// `U` chord — symmetric unstage over the entire selection set.
    fn on_unstage_file(&mut self, _: &UnstageFile, _window: &mut Window, cx: &mut Context<Self>) {
        self.bulk_unstage_selected(cx);
    }

    /// `R` chord — keyboard / command-palette discard. Slice A keeps
    /// the modal-driven single-row flow: acts only when exactly one
    /// path is selected. Multi-row discard waits for Slice C's
    /// `DiscardAllArea` routing, so we don't queue N modals on a chord.
    fn on_revert_file(&mut self, _: &RevertFile, _window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.len() != 1 {
            // Trace so a user wondering "why didn't R do anything?"
            // can see the deferral in the logs. Surfaced as `debug` —
            // not a warning; the chord wiring is intentional.
            tracing::debug!(
                target: "oximux_app::git_panel",
                count = self.selected.len(),
                "R chord skipped: multi-row discard lands in Slice C (DiscardAllArea)"
            );
            return;
        }
        let Some(path) = self.selected.iter().next().cloned() else {
            return;
        };
        self.discard_path(path, cx);
    }

    /// Stage a specific path — hover-action entrypoint. Doesn't read
    /// `self.selected` so it works on any row, not just the highlighted
    /// one. Spawns the git op on the current tokio runtime; failures land
    /// in tracing.
    pub fn stage_path(&mut self, path: PathBuf, _cx: &mut Context<Self>) {
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move { repo.stage_paths(&[path.as_path()]).await },
            "stage_paths",
        );
    }

    /// Unstage a specific path. Symmetric counterpart to `stage_path`.
    pub fn unstage_path(&mut self, path: PathBuf, _cx: &mut Context<Self>) {
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move { repo.unstage_paths(&[path.as_path()]).await },
            "unstage_paths",
        );
    }

    /// Stage every path in `paths` — section-header "Stage all"
    /// entrypoint. Fire-and-forget: no in-flight tracking, no
    /// selection mutation, just one `git add` invocation. Failures
    /// land in `tracing::warn!` via [`spawn_repo_op`]. No-op when
    /// `paths` is empty.
    ///
    /// Distinct from [`selection::bulk_stage_selected`] which acts
    /// on `self.selected` and flips `bulk_op_in_flight` for the
    /// BulkActionBar spinner.
    pub fn stage_paths_bulk(&mut self, paths: Vec<PathBuf>, _cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move {
                let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
                repo.stage_paths(&refs).await
            },
            "stage_paths_bulk",
        );
    }

    /// Unstage every path in `paths` — section-header "Unstage all"
    /// entrypoint. Same shape as [`stage_paths_bulk`].
    pub fn unstage_paths_bulk(&mut self, paths: Vec<PathBuf>, _cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move {
                let refs: Vec<&std::path::Path> = paths.iter().map(|p| p.as_path()).collect();
                repo.unstage_paths(&refs).await
            },
            "unstage_paths_bulk",
        );
    }
}


impl Focusable for GitPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match (&self.poll_state, &self.git_state) {
            (PollState::Failed(e), _) => placeholder_state(
                &format!("git status failed: {e}"),
                self.theme,
                self.density,
                &self.typography,
            )
            .into_any_element(),
            (PollState::Loading, None) => {
                placeholder_state("Loading…", self.theme, self.density, &self.typography)
                    .into_any_element()
            }
            (_, Some(state)) => {
                // Filter first, then partition. `filter_files` returns
                // borrowed slices; we clone to an owned Vec so partition's
                // `&[FileStatus]` signature stays unchanged. Cost is trivial
                // at the row counts a working tree produces.
                let filtered: Vec<oximux_core::FileStatus> =
                    filter_files(&state.files, &self.filter_query)
                        .into_iter()
                        .cloned()
                        .collect();
                let sections = partition_files(&filtered);
                let rctx = RenderCtx {
                    theme: self.theme,
                    density: self.density,
                    typography: &self.typography,
                    selected: &self.selected,
                    collapsed: &self.collapsed_sections,
                    branch: state.branch.as_deref(),
                    in_flight_discards: &self.in_flight_discards,
                    filter_active: !self.filter_query.trim().is_empty(),
                    discard_pending: self.pending_discard.is_some(),
                    view_mode: self.view_mode,
                    collapsed_dirs: &self.collapsed_dirs,
                };
                render_sections(&sections, &rctx, cx).into_any_element()
            }
            (_, None) => placeholder_state("Loading…", self.theme, self.density, &self.typography)
                .into_any_element(),
        };

        // Inner scroll region: stateful (id required for `overflow_y_scroll`)
        // with `flex_1 + min_h(0)` so the sections column can shrink below its
        // intrinsic height. Without these, an expanded STAGED CHANGES section
        // (dozens of rows) overflows the flex chain and pushes the rest of the
        // Source Control panel — and the chrome above it — off-screen.
        //
        // `relative()` anchors the gpui-component vertical scrollbar overlay;
        // `track_scroll` + `vertical_scrollbar` share `scroll_handle` so the
        // thumb mirrors the user's wheel/drag position.
        //
        // `BULK_BAR_PAD_PX` reserves space at the bottom of the scroll
        // region when the BulkActionBar is mounted so the last row
        // stays visible behind it. Zero when no selection — no waste
        // on the common path.
        let bar = bulk_action_bar::render_bulk_action_bar(
            self,
            self.theme,
            self.density,
            &self.typography,
            cx,
        );
        let pad_bottom = if bar.is_some() { BULK_BAR_PAD_PX } else { 0.0 };
        let scroll_body = div()
            .id("git-panel-scroll")
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .pb(px(pad_bottom))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(body)
            .vertical_scrollbar(&self.scroll_handle);

        // Outer container is `relative` so the floating `BulkActionBar`
        // (positioned absolute, bottom-anchored) lays out against it.
        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_stage_file))
            .on_action(cx.listener(Self::on_unstage_file))
            .on_action(cx.listener(Self::on_revert_file))
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .min_h(px(0.0))
            .overflow_hidden()
            .bg(self.theme.bg_panel)
            .border_l_1()
            .border_color(self.theme.border_inactive)
            .child(scroll_body)
            .children(bar)
    }
}

/// Bottom padding reserved inside the changed-files scroll body when
/// the floating [`bulk_action_bar`] is mounted, so the last row keeps
/// 1–2 line-heights of clearance behind the bar. Picked by eye to
/// cover the bar's own `py(4.0)` + ~16px text + 1px border with a
/// small margin.
const BULK_BAR_PAD_PX: f32 = 32.0;

/// Centered single-line text used for both the loading placeholder and the
/// `PollState::Failed` surface. Same layout, different copy.
fn placeholder_state(
    msg: &str,
    theme: Theme,
    density: Density,
    _typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .p(px(density.pad_panel))
        .text_size(px(sc_style::TEXT))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}

/// Resolve the initial `ViewMode` for a fresh panel: per-worktree
/// override wins, then fall back to the in-memory default (Flat). Storage
/// errors and unknown strings both decode to `Flat` so a corrupted row
/// can never panic the panel.
fn initial_view_mode(repo: &Repository, settings_repo: Option<&WorktreeSettingsRepo>) -> ViewMode {
    let Some(settings_repo) = settings_repo else {
        return ViewMode::default();
    };
    let key = repo.workdir().to_string_lossy().into_owned();
    settings_repo
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| s.view_mode_override)
        .as_deref()
        .map(ViewMode::from_str)
        .unwrap_or_default()
}

/// Spawn a repo mutation on the current tokio runtime. Logs and no-ops if no
/// runtime is entered (e.g. the gpui smoke test, or before step 14's shell
/// integration boots the runtime). Caller passes a closure that returns a
/// `Result<()>` future; only the error path is logged.
///
/// `pub(super)` so the [`selection`] submodule can spawn vectorised stage
/// / unstage ops in `bulk_stage_selected` / `bulk_unstage_selected`.
pub(super) fn spawn_repo_op<F, Fut>(repo: Repository, op: F, label: &'static str)
where
    F: FnOnce(Repository) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = oximux_git::Result<()>> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                if let Err(e) = op(repo).await {
                    tracing::warn!(target: "oximux_app::git_panel", error = %e, op = label, "git op failed");
                }
            });
        }
        Err(_) => {
            tracing::warn!(target: "oximux_app::git_panel", op = label, "no tokio runtime entered; op skipped (step 14 wires runtime)");
        }
    }
}
