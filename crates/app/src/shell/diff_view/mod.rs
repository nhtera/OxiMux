//! DiffView — read-only patch renderer driven by GitPanel selection.
//!
//! State machine:
//! ```text
//!   Empty                              ← initial
//!   Loading { path, staged }           ← fetch in flight
//!   Ready   { path, staged, diffs, expanded } ← diffs loaded
//!   Failed  { path, staged, error }    ← fetch failed
//! ```
//!
//! Runtime: `load()` uses `tokio::runtime::Handle::try_current()` and falls
//! back to logging + no-op when no tokio runtime is entered. Step 14 wires
//! the runtime at the shell mount point; until then the view stays in
//! `Loading` indefinitely if invoked without a runtime (matches the
//! `spawn_repo_op` pattern in `git_panel/mod.rs:217`).
//!
//! Rendering: `render.rs` owns the pure data plan + the `IntoElement`
//! builder. This file holds state, actions, async wiring, and the root
//! container.

pub mod render;

use crate::actions::ExpandDiff;
use crate::shell::diff_view::render::{RenderCtx, build_render_plan, render_plan};
use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Task, Window, div, px,
};
use oximux_core::FileDiff;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum DiffViewState {
    Empty,
    Loading {
        path: PathBuf,
        staged: bool,
    },
    Ready {
        path: PathBuf,
        staged: bool,
        diffs: Vec<FileDiff>,
        expanded: bool,
    },
    Failed {
        path: PathBuf,
        staged: bool,
        error: String,
    },
}

pub struct DiffView {
    repo: Repository,
    state: DiffViewState,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// In-flight load task. Dropping aborts; we replace on every `load()`
    /// call so a fast-switching user only sees the latest selection.
    _load_task: Option<Task<()>>,
}

impl DiffView {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            repo,
            state: DiffViewState::Empty,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _load_task: None,
        }
    }

    /// Inspect-only accessor used by tests + by `GitPanel` to avoid
    /// double-loading when the user re-clicks the same row.
    pub fn state(&self) -> &DiffViewState {
        &self.state
    }

    /// Begin loading `path` in the requested stage. Cancels any in-flight
    /// load by dropping the previous task.
    pub fn load(&mut self, path: PathBuf, staged: bool, cx: &mut Context<Self>) {
        self.state = DiffViewState::Loading {
            path: path.clone(),
            staged,
        };
        let repo = self.repo.clone();
        let path_for_fetch = path.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<FileDiff>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo
                        .diff_for_path(&path_for_fetch, staged)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    "no tokio runtime entered; diff load skipped (step 14 wires runtime)"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                view.apply_load_result(path, staged, result);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    /// Toggle a large-diff file from collapsed → expanded. Invoked by the
    /// `ExpandDiff` action and the click on the expand row in `render.rs`.
    pub fn expand(&mut self) {
        if let DiffViewState::Ready { expanded, .. } = &mut self.state {
            *expanded = true;
        }
    }

    fn apply_load_result(
        &mut self,
        path: PathBuf,
        staged: bool,
        result: Result<Vec<FileDiff>, String>,
    ) {
        match result {
            Ok(diffs) => {
                self.state = DiffViewState::Ready {
                    path,
                    staged,
                    diffs,
                    expanded: false,
                };
            }
            Err(error) => {
                self.state = DiffViewState::Failed {
                    path,
                    staged,
                    error,
                };
            }
        }
    }

    fn on_expand_diff(&mut self, _: &ExpandDiff, _window: &mut Window, cx: &mut Context<Self>) {
        self.expand();
        cx.notify();
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rctx = RenderCtx {
            theme: self.theme,
            density: self.density,
            typography: &self.typography,
        };
        let body = match &self.state {
            DiffViewState::Empty => empty_state(&rctx).into_any_element(),
            DiffViewState::Loading { path, .. } => {
                loading_state(&path.display().to_string(), &rctx).into_any_element()
            }
            DiffViewState::Failed { path, error, .. } => {
                failed_state(&path.display().to_string(), error, &rctx).into_any_element()
            }
            DiffViewState::Ready {
                diffs, expanded, ..
            } => {
                let plan = build_render_plan(diffs, *expanded);
                render_plan(&plan, &rctx, cx).into_any_element()
            }
        };
        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_expand_diff))
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_base)
            .border_l_1()
            .border_color(self.theme.border_inactive)
            .child(body)
    }
}

fn empty_state(rctx: &RenderCtx<'_>) -> impl IntoElement {
    centered("Select a file to view diff".to_string(), rctx)
}

fn loading_state(path: &str, rctx: &RenderCtx<'_>) -> impl IntoElement {
    centered(format!("Loading diff for {path}…"), rctx)
}

fn failed_state(path: &str, error: &str, rctx: &RenderCtx<'_>) -> impl IntoElement {
    centered(format!("Failed to load {path}: {error}"), rctx)
}

fn centered(msg: String, rctx: &RenderCtx<'_>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(msg)
}
