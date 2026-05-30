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

pub mod changed_files;
pub mod discard_confirm;
pub mod row_actions;
pub mod row_renderer;

use crate::actions::{RevertFile, StageFile, UnstageFile};
use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::changed_files::{RenderCtx, partition_files, render_sections};
use crate::shell::git_panel::discard_confirm::{DiscardCopy, DiscardKind};
use crate::shell::source_control::filter::filter_files;
use crate::shell::source_control::style as sc_style;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Task,
    Window, div, px,
};
use gpui_component::scroll::ScrollableElement as _;
use oximux_core::GitState;
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::sync::watch;

use crate::shell::file_tree_view::OnOpenDiff;

pub struct GitPanel {
    repo: Repository,
    /// Last `Ready` payload observed on the watch channel. Cleared back to
    /// `None` on a fresh `Loading` or `Failed` transition so the render path
    /// can distinguish "no data yet" from "no changes".
    git_state: Option<GitState>,
    poll_state: PollState,
    /// Currently-highlighted row. Stores both the path and the section it
    /// came from (`staged` for the Staged section, `false` for Unstaged /
    /// Untracked) so we know which side of the diff to fetch when routing
    /// to `DiffView::load`.
    selected: Option<(PathBuf, bool)>,
    /// Held but no longer driven — kept on the struct so the
    /// constructor signature doesn't churn while the inline sidebar
    /// `DiffView` is dormant. Diffs now open as real editor tabs in
    /// the main pane via `on_open: OnOpenDiff` (see `set_selected`
    /// doc). A follow-up can drop the field + constructor arg + the
    /// `DiffView` mount in `right_sidebar/mod.rs`.
    #[allow(dead_code)]
    diff_view: Option<Entity<DiffView>>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Case-insensitive substring filter applied to `git_state.files` before
    /// partitioning. Empty string disables filtering. Owner: `SourceControlPanel`
    /// updates this via `set_filter` as the user types in the filter input.
    filter_query: String,
    /// Section names (e.g. "STAGED CHANGES") whose body is currently hidden.
    /// Toggled by clicking the section header. Default: all expanded.
    collapsed_sections: HashSet<&'static str>,
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
}

/// Information the shell host needs to render a discard confirm dialog.
///
/// Built by `discard_path` from the row's `FileStatus` via
/// [`crate::shell::git_panel::discard_confirm::copy_for`]. The host
/// passes `copy` + `expected` straight into a `ConfirmPrompt`.
#[derive(Debug, Clone)]
pub struct DiscardRequest {
    pub path: PathBuf,
    pub kind: DiscardKind,
    pub copy: DiscardCopy,
    pub expected: SharedString,
}

/// Event emitted whenever `discard_path` accepts a new request. The
/// shell host subscribes to GitPanel and pulls the live request out of
/// `pending_discard()` to build the confirm dialog. We could ship the
/// `DiscardRequest` on the event itself, but routing through the field
/// keeps the panel a single source of truth — re-subscribers see the
/// same state.
#[derive(Debug, Clone, Copy)]
pub struct DiscardRequested;

impl EventEmitter<DiscardRequested> for GitPanel {}

impl GitPanel {
    /// Build the panel. `state_rx` is `StatusPoller::subscribe()`; the caller
    /// owns the poller so the same receiver can fan out to sidebar / status
    /// bar consumers later.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Repository,
        state_rx: watch::Receiver<PollState>,
        diff_view: Option<Entity<DiffView>>,
        theme: Theme,
        density: Density,
        typography: Typography,
        on_open: Option<OnOpenDiff>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let initial = state_rx.borrow().clone();
        let git_state = match &initial {
            PollState::Ready(s) => Some(s.clone()),
            _ => None,
        };
        let watch_task = Self::start_watch_task(state_rx, cx);
        Self {
            repo,
            git_state,
            poll_state: initial,
            selected: None,
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
        }
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

    /// Update the highlighted row. Previously also routed the patch
    /// fetch into the sibling sidebar `DiffView`; diffs now open as
    /// real editor tabs in the main pane (via `on_open: OnOpenDiff`
    /// dispatched from `changed_files::row`), so the inline view stays
    /// in `Empty` state and we skip the fetch — no point spending git
    /// I/O on a surface nobody mounts. The `diff_view` field is kept
    /// on `Self` to avoid churning the constructor signature; future
    /// cleanup can drop it once the field has no other callers.
    pub(crate) fn set_selected(
        &mut self,
        selection: Option<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) {
        self.selected = selection;
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

    fn on_stage_file(&mut self, _: &StageFile, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, _)) = self.selected.clone() else {
            return;
        };
        self.stage_path(path, cx);
    }

    fn on_unstage_file(&mut self, _: &UnstageFile, _window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, _)) = self.selected.clone() else {
            return;
        };
        self.unstage_path(path, cx);
    }

    fn on_revert_file(&mut self, _: &RevertFile, _window: &mut Window, cx: &mut Context<Self>) {
        // Keyboard / command-palette entrypoint. Goes through the same
        // `discard_path` method that the hover-action button uses so any
        // confirmation modal added later (Phase 01b) covers both paths.
        let Some((path, _)) = self.selected.clone() else {
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

    /// Open a confirm dialog for discarding `path`. Pulls the matching
    /// `FileStatus` out of the current `git_state` so the modal copy
    /// (Delete / Restore / Discard) matches what the user is looking
    /// at. If `path` is no longer in `git_state` (stale UI state), the
    /// fallback copy reads as a generic "Discard changes to ...".
    ///
    /// This method does NOT mutate the working tree — it only sets
    /// `pending_discard` and notifies. The shell host observes the
    /// field, mounts a `ConfirmDialog`, and calls
    /// [`confirmed_discard_path`] when the user confirms.
    ///
    /// Early-returns when another discard for the same path is already
    /// in flight or when any request is currently pending. This
    /// prevents the revert keybind from queueing a second confirm
    /// dialog over an unresolved one (the hover button blocks clicks
    /// via `.disabled(true)` already; the keybind has no equivalent
    /// gate at the action handler).
    ///
    /// [`confirmed_discard_path`]: Self::confirmed_discard_path
    pub fn discard_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.pending_discard.is_some() || self.in_flight_discards.contains(&path) {
            return;
        }
        let request = self.build_discard_request(path);
        self.pending_discard = Some(request);
        cx.emit(DiscardRequested);
        cx.notify();
    }

    fn build_discard_request(&self, path: PathBuf) -> DiscardRequest {
        let file_status = self
            .git_state
            .as_ref()
            .and_then(|s| s.files.iter().find(|f| f.path == path).cloned());

        match file_status {
            Some(f) => {
                let (kind, copy) = discard_confirm::copy_for(&f);
                let expected = discard_confirm::expected_for(&f);
                DiscardRequest {
                    path,
                    kind,
                    copy,
                    expected,
                }
            }
            None => {
                // Stale state — the poller hasn't reported this path
                // (yet). Surface the generic Discard copy with the
                // path basename so the user can decide.
                let display = path
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                DiscardRequest {
                    path,
                    kind: DiscardKind::Discard,
                    copy: DiscardCopy {
                        title: format!("Discard changes to \"{display}\"?").into(),
                        body: "This will revert all changes to this file. This cannot be undone."
                            .into(),
                        confirm_label: "Discard".into(),
                    },
                    expected: display.into(),
                }
            }
        }
    }

    /// Current pending discard request, if any. The shell host observes
    /// this via `cx.observe(&panel)` to mount / dismiss the modal.
    pub fn pending_discard(&self) -> Option<&DiscardRequest> {
        self.pending_discard.as_ref()
    }

    /// Drop the pending request without mutating the working tree.
    /// Called by the shell on Escape / cancel / click-outside.
    pub fn clear_pending_discard(&mut self, cx: &mut Context<Self>) {
        if self.pending_discard.take().is_some() {
            cx.notify();
        }
    }

    /// Paths whose `confirmed_discard_path` op is in flight on the
    /// tokio runtime. The row renderer reads this set to swap the
    /// revert icon for a spinner.
    pub fn in_flight_discards(&self) -> &HashSet<PathBuf> {
        &self.in_flight_discards
    }

    /// Actually run `git restore --` for `path`. Tracks the path in
    /// `in_flight_discards` for spinner rendering and clears it after
    /// the op completes (success or failure). The host calls this from
    /// the ConfirmDialog `on_confirm` callback.
    ///
    /// Pauses editor autosave for the duration of the op so a buffer
    /// open on the same path can't race the `git restore` write and
    /// immediately re-save the user's old content over the checked-out
    /// version (today both calls are no-op stubs; the semantics light
    /// up automatically when the editor side ships an autosave pump).
    pub fn confirmed_discard_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Clear the pending request so the modal can come down.
        self.pending_discard = None;
        self.in_flight_discards.insert(path.clone());
        cx.notify();

        oximux_editor::pause_autosave(&path);

        let repo = self.repo.clone();
        let op_path = path.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo
                        .discard_paths(&[op_path.as_path()])
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::git_panel",
                    "no tokio runtime; confirmed_discard_path skipped (test wiring)"
                );
                self.in_flight_discards.remove(&path);
                oximux_editor::resume_autosave(&path);
                cx.notify();
                return;
            }
        }

        // Detach the result-handling task so concurrent discards (a
        // future Phase 03 bulk-discard flow) don't cancel each other.
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |panel, cx| {
                panel.in_flight_discards.remove(&path);
                oximux_editor::resume_autosave(&path);
                if let Ok(Err(err)) = result {
                    tracing::warn!(
                        target: "oximux_app::git_panel",
                        error = %err,
                        "discard_paths failed"
                    );
                }
                cx.notify();
            });
        })
        .detach();
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
                    selected: self.selected.as_ref().map(|(p, _)| p.as_path()),
                    collapsed: &self.collapsed_sections,
                    branch: state.branch.as_deref(),
                    in_flight_discards: &self.in_flight_discards,
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
        let scroll_body = div()
            .id("git-panel-scroll")
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .child(body)
            .vertical_scrollbar(&self.scroll_handle);

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_stage_file))
            .on_action(cx.listener(Self::on_unstage_file))
            .on_action(cx.listener(Self::on_revert_file))
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
    }
}

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

/// Spawn a repo mutation on the current tokio runtime. Logs and no-ops if no
/// runtime is entered (e.g. the gpui smoke test, or before step 14's shell
/// integration boots the runtime). Caller passes a closure that returns a
/// `Result<()>` future; only the error path is logged.
fn spawn_repo_op<F, Fut>(repo: Repository, op: F, label: &'static str)
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
