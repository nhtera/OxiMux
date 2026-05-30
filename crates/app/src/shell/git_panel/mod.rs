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
pub mod range_select;
pub mod row_actions;
pub mod row_renderer;
pub mod selection;

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
                    selected: &self.selected,
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
