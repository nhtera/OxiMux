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

use crate::actions::{RevertFile, StageFile, UnstageFile};
use crate::shell::diff_view::DiffView;
use crate::shell::git_panel::changed_files::{RenderCtx, partition_files, render_sections};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Task, Window, div, px,
};
use oximux_core::GitState;
use oximux_git::{PollState, Repository};
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;
use tokio::sync::watch;

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
    /// Optional sibling diff view. `None` keeps the panel buildable before
    /// step 14 wires the shell. When `Some`, row clicks call
    /// `diff_view.load(path, staged)`.
    diff_view: Option<Entity<DiffView>>,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Drop cancels the receiver-watching task (mirrors `_poll_task` /
    /// `_blink_task` lifetime semantics in `TerminalView`).
    _watch_task: Task<()>,
}

impl GitPanel {
    /// Build the panel. `state_rx` is `StatusPoller::subscribe()`; the caller
    /// owns the poller so the same receiver can fan out to sidebar / status
    /// bar consumers later.
    pub fn new(
        repo: Repository,
        state_rx: watch::Receiver<PollState>,
        diff_view: Option<Entity<DiffView>>,
        theme: Theme,
        density: Density,
        typography: Typography,
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
            _watch_task: watch_task,
        }
    }

    /// Update the highlighted row and route to the sibling `DiffView` when
    /// present. Called by `changed_files::row` click handlers; pub-crate so
    /// the sibling module reaches it without exposing the field directly.
    pub(crate) fn set_selected(
        &mut self,
        selection: Option<(PathBuf, bool)>,
        cx: &mut Context<Self>,
    ) {
        self.selected = selection.clone();
        if let (Some((path, staged)), Some(view)) = (selection, self.diff_view.as_ref()) {
            view.update(cx, |v, cx| v.load(path, staged, cx));
        }
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

    fn on_stage_file(&mut self, _: &StageFile, _window: &mut Window, _cx: &mut Context<Self>) {
        let Some((path, _)) = self.selected.clone() else {
            return;
        };
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move { repo.stage_paths(&[path.as_path()]).await },
            "stage_paths",
        );
    }

    fn on_unstage_file(&mut self, _: &UnstageFile, _window: &mut Window, _cx: &mut Context<Self>) {
        let Some((path, _)) = self.selected.clone() else {
            return;
        };
        spawn_repo_op(
            self.repo.clone(),
            move |repo| async move { repo.unstage_paths(&[path.as_path()]).await },
            "unstage_paths",
        );
    }

    fn on_revert_file(&mut self, _: &RevertFile, _window: &mut Window, _cx: &mut Context<Self>) {
        // Step 8 stub. Step 11 wires the type-to-confirm modal before any
        // worktree mutation. Logging here so the wiring is visible end-to-end.
        if let Some((path, _)) = self.selected.as_ref() {
            tracing::info!(
                ?path,
                "RevertFile dispatched — confirm modal lands in step 11"
            );
        }
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
                let sections = partition_files(&state.files);
                let rctx = RenderCtx {
                    theme: self.theme,
                    density: self.density,
                    typography: &self.typography,
                    selected: self.selected.as_ref().map(|(p, _)| p.as_path()),
                };
                render_sections(&sections, &rctx, cx).into_any_element()
            }
            (_, None) => placeholder_state("Loading…", self.theme, self.density, &self.typography)
                .into_any_element(),
        };

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_stage_file))
            .on_action(cx.listener(Self::on_unstage_file))
            .on_action(cx.listener(Self::on_revert_file))
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_panel)
            .border_l_1()
            .border_color(self.theme.border_inactive)
            .child(body)
    }
}

/// Centered single-line text used for both the loading placeholder and the
/// `PollState::Failed` surface. Same layout, different copy.
fn placeholder_state(
    msg: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .p(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
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
