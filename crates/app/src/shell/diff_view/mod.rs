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

pub mod hunk_actions;
pub mod paint;
pub mod render;
pub mod syntax;
pub mod word_diff;

use crate::actions::{ExpandDiff, RetryDiff};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::diff_view::paint::{PreparedRow, prepare, render_rows};
use crate::shell::diff_view::render::{RenderCtx, build_render_plan};
use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement as _, Styled, Subscription, Task,
    UniformListScrollHandle, Window, div, px,
};
use oximux_core::FileDiff;
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum DiffViewState {
    Empty,
    Loading {
        path: PathBuf,
        staged: bool,
        /// Carried so retry-on-failure can re-route through the untracked
        /// codepath (`diff_for_untracked`) when the original load did.
        untracked: bool,
    },
    Ready {
        path: PathBuf,
        staged: bool,
        /// Source-of-truth for post-hunk-op reloads. Persisting it in the
        /// Ready state means `stage_hunk` / `unstage_hunk` /
        /// `confirmed_discard_hunk` can re-run `load()` with the same
        /// routing the initial fetch used, instead of falling back to the
        /// tracked-path branch for files git doesn't know about.
        untracked: bool,
        diffs: Vec<FileDiff>,
        expanded: bool,
    },
    Failed {
        path: PathBuf,
        staged: bool,
        untracked: bool,
        error: String,
    },
    /// Commit-detail mode: showing every file a single commit touches.
    /// Distinct from `Loading` because the routing key is a SHA (no
    /// `staged`/`untracked`/`path` semantics) and the post-load state
    /// disables hunk action chips — Stage/Unstage/Discard make no
    /// sense against a historical commit.
    CommitLoading {
        sha: String,
        short_oid: String,
        subject: String,
    },
    CommitReady {
        sha: String,
        short_oid: String,
        subject: String,
        diffs: Vec<FileDiff>,
        expanded: bool,
    },
    CommitFailed {
        sha: String,
        short_oid: String,
        subject: String,
        error: String,
    },
}

/// Public visibility-decision payload returned by `DiffView::current_side`.
/// Drives the `hunk_actions` overlay's button gating without exposing
/// the full `DiffViewState` enum to the renderer.
#[derive(Debug, Clone, Copy)]
pub struct HunkActionSide {
    pub staged: bool,
    pub untracked: bool,
}

/// Snapshot of the file under cursor + the routing fields needed for a
/// post-op reload. Kept private — `stage_hunk` / `unstage_hunk` /
/// `confirmed_discard_hunk` build one before spawning their tokio task.
struct HunkTarget {
    path: PathBuf,
    staged: bool,
    untracked: bool,
    file: FileDiff,
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
    /// In-flight hunk op (stage / unstage / discard). Single shared slot
    /// mirrors `StashPanel::_op_task` — rapid back-to-back ops cancel
    /// the prior op's gpui-side refresh, but the tokio side-effect still
    /// completes; the next op fires its own reload.
    _op_task: Option<Task<()>>,
    /// Active hunk-discard confirm modal (per-request; `None` when idle).
    /// Mounted INSIDE the DiffView's render tree rather than
    /// workspace_root so multiple open diff tabs each carry their own
    /// confirm slot, and so the dialog backdrop scopes to the diff
    /// surface the user is reading (no full-window modal for a per-tab
    /// destructive op).
    confirm_dialog: Option<Entity<ConfirmDialog>>,
    /// Per-mount observer on the active `ConfirmDialog`. Reset each time
    /// a new dialog is mounted; the previous observer drops along with
    /// its dialog. Same lifecycle pattern as
    /// `WorkspaceRoot::_discard_dialog_observer`.
    _confirm_dialog_observer: Option<Subscription>,
    /// Vertical scroll state for the virtualized diff body. Owned here so
    /// the `uniform_list` reports an exact content height (`rows × h_row`)
    /// and scrolling reaches the true end of the diff.
    scroll_handle: UniformListScrollHandle,
    /// Cached flattened render rows. Built once per (diff, expanded) change
    /// — NOT per frame — so syntax highlighting and word-diff pairing stay
    /// off the scroll path. `None` means "stale, rebuild on next render";
    /// every state transition that changes the body resets it.
    prepared: Option<Rc<Vec<PreparedRow>>>,
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
            _op_task: None,
            confirm_dialog: None,
            _confirm_dialog_observer: None,
            scroll_handle: UniformListScrollHandle::new(),
            prepared: None,
        }
    }

    /// Invalidate the cached render rows so the next `render` rebuilds them
    /// from the current state. Called on every transition that changes the
    /// diff body (load, commit load, expand, seed).
    fn invalidate_prepared(&mut self) {
        self.prepared = None;
    }

    /// Inspect-only accessor used by tests + by `GitPanel` to avoid
    /// double-loading when the user re-clicks the same row.
    pub fn state(&self) -> &DiffViewState {
        &self.state
    }

    /// Begin loading `path` in the requested stage. Cancels any in-flight
    /// load by dropping the previous task.
    ///
    /// Routing: tracked files go through `diff_for_path` (normal git diff
    /// against index or HEAD). When `untracked = true`, the path bypasses
    /// git entirely and `diff_for_untracked` reads the file off disk to
    /// synthesize an "all-additions" diff — `git diff` returns nothing for
    /// untracked paths, which would leave the user staring at "No diff"
    /// when they clicked a new file row.
    pub fn load(&mut self, path: PathBuf, staged: bool, untracked: bool, cx: &mut Context<Self>) {
        // Drop any pending post-op reload from a hunk dispatch against
        // the prior file. Without this, a user who stages a hunk and
        // immediately clicks a different file would see the new file
        // load, then briefly flash back to the prior file's diff when
        // the stale `_op_task` finishes its reload chain.
        self._op_task = None;
        self.invalidate_prepared();
        self.state = DiffViewState::Loading {
            path: path.clone(),
            staged,
            untracked,
        };
        let repo = self.repo.clone();
        let path_for_fetch = path.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<FileDiff>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = if untracked {
                        repo.diff_for_untracked(&path_for_fetch)
                            .await
                            .map_err(|e| e.to_string())
                    } else {
                        repo.diff_for_path(&path_for_fetch, staged)
                            .await
                            .map_err(|e| e.to_string())
                    };
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
                view.apply_load_result(path, staged, untracked, result);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    /// Re-run the most recent load. No-op unless the current state is
    /// `Failed` or `CommitFailed` (so retry-while-Ready doesn't spam
    /// refresh). Caller is the `RetryDiff` action handler.
    pub fn retry(&mut self, cx: &mut Context<Self>) {
        match &self.state {
            DiffViewState::Failed {
                path,
                staged,
                untracked,
                ..
            } => {
                let (path, staged, untracked) = (path.clone(), *staged, *untracked);
                self.load(path, staged, untracked, cx);
            }
            DiffViewState::CommitFailed {
                sha,
                short_oid,
                subject,
                ..
            } => {
                let (sha, short_oid, subject) =
                    (sha.clone(), short_oid.clone(), subject.clone());
                self.load_commit(sha, short_oid, subject, cx);
            }
            _ => {}
        }
    }

    /// Begin loading the per-file diff for a commit. Bypasses the
    /// file/staged routing — uses `repo.commit_files(sha)` to fetch
    /// every file the commit touches, then mounts them in the same
    /// `Vec<FileDiff>` shape the unstaged/staged path uses. Hunk
    /// action chips do NOT render on this side (`current_side`
    /// returns `None` whenever the state isn't the file-mode `Ready`).
    pub fn load_commit(
        &mut self,
        sha: String,
        short_oid: String,
        subject: String,
        cx: &mut Context<Self>,
    ) {
        // Same drop-on-entry rule as `load()`: a stale post-op reload
        // from a prior file selection must not flash over the new
        // commit-detail view.
        self._op_task = None;
        self.invalidate_prepared();
        self.state = DiffViewState::CommitLoading {
            sha: sha.clone(),
            short_oid: short_oid.clone(),
            subject: subject.clone(),
        };
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<FileDiff>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let sha_for_fetch = sha.clone();
                handle.spawn(async move {
                    let r = repo
                        .commit_files(&sha_for_fetch)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    "no tokio runtime entered; commit load skipped"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                view.apply_commit_load_result(sha, short_oid, subject, result);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    fn apply_commit_load_result(
        &mut self,
        sha: String,
        short_oid: String,
        subject: String,
        result: Result<Vec<FileDiff>, String>,
    ) {
        match result {
            Ok(diffs) => {
                // Preserve `expanded` across reloads of the same commit
                // (e.g. via retry). Different SHA always starts
                // collapsed — that's the user-intent signal.
                let expanded = match &self.state {
                    DiffViewState::CommitReady {
                        sha: prev_sha,
                        expanded: prev_expanded,
                        ..
                    } if *prev_sha == sha => *prev_expanded,
                    _ => false,
                };
                self.state = DiffViewState::CommitReady {
                    sha,
                    short_oid,
                    subject,
                    diffs,
                    expanded,
                };
            }
            Err(error) => {
                self.state = DiffViewState::CommitFailed {
                    sha,
                    short_oid,
                    subject,
                    error,
                };
            }
        }
    }

    /// Toggle a large-diff file from collapsed → expanded. Invoked by the
    /// `ExpandDiff` action and the click on the expand row in `render.rs`.
    /// Applies to both file-mode Ready and commit-detail CommitReady —
    /// the large-diff threshold can trip either path (e.g. opening a
    /// squash commit that touches a 50-file refactor).
    pub fn expand(&mut self) {
        match &mut self.state {
            DiffViewState::Ready { expanded, .. } => *expanded = true,
            DiffViewState::CommitReady { expanded, .. } => *expanded = true,
            _ => {}
        }
        // Expanding a collapsed large diff changes which rows render — drop
        // the cached row list so the next render rebuilds the full body.
        self.invalidate_prepared();
    }

    fn apply_load_result(
        &mut self,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        result: Result<Vec<FileDiff>, String>,
    ) {
        match result {
            Ok(diffs) => {
                // Preserve `expanded` across reloads of the same
                // (path, staged, untracked) tuple. Hunk dispatch
                // (stage / unstage / discard) chains a reload after
                // every op; without this carry-over the user has to
                // re-expand a large-diff file after every action.
                // A fresh navigation (different path or staged-side
                // flip) starts collapsed — that's the user-intent
                // signal that the prior expansion was specific to
                // the prior context.
                let expanded = match &self.state {
                    DiffViewState::Ready {
                        path: prev_path,
                        staged: prev_staged,
                        untracked: prev_untracked,
                        expanded: prev_expanded,
                        ..
                    } if *prev_path == path
                        && *prev_staged == staged
                        && *prev_untracked == untracked =>
                    {
                        *prev_expanded
                    }
                    _ => false,
                };
                self.state = DiffViewState::Ready {
                    path,
                    staged,
                    untracked,
                    diffs,
                    expanded,
                };
            }
            Err(error) => {
                self.state = DiffViewState::Failed {
                    path,
                    staged,
                    untracked,
                    error,
                };
            }
        }
    }

    /// Test-only: stamp the view into `Ready` with pre-fetched diffs.
    /// Integration tests use this to skip the load chain (which would
    /// require pumping the gpui executor across a tokio crossing, and
    /// trip `test_scheduler.rs::detect_non_determinism`). Production
    /// code goes through `load()` exclusively.
    #[doc(hidden)]
    pub fn seed_ready_for_test(
        &mut self,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        diffs: Vec<FileDiff>,
    ) {
        self.state = DiffViewState::Ready {
            path,
            staged,
            untracked,
            diffs,
            expanded: false,
        };
        self.invalidate_prepared();
    }

    /// Inspector for the host: which side (staged vs unstaged) is on
    /// screen, and is the file untracked. Drives the `hunk_actions`
    /// overlay's button visibility — Stage shows when `!staged`, Unstage
    /// when `staged`, Discard when `!staged && !untracked`. Returns
    /// `None` when the view isn't in `Ready`.
    pub fn current_side(&self) -> Option<HunkActionSide> {
        let DiffViewState::Ready {
            staged, untracked, ..
        } = &self.state
        else {
            return None;
        };
        Some(HunkActionSide {
            staged: *staged,
            untracked: *untracked,
        })
    }

    /// Build a STAGING snapshot for `file_idx`: a `FileDiff` whose `hunks`
    /// are the file's stageable change regions (from `change_regions`), not
    /// its full-file display hunks. Returns `None` when the view isn't
    /// Ready or `file_idx` is out of range.
    ///
    /// Diffs are fetched with full-file context so the user can scroll the
    /// whole document, which collapses every edit into one giant hunk. The
    /// per-hunk Stage/Unstage/Discard chips index REGIONS, so the op path
    /// rebuilds a `FileDiff` carrying one `git apply`-ready hunk per region
    /// — then `repo.stage_hunks(&file, &[region_idx])` works unchanged with
    /// per-region (git add -p) granularity instead of whole-file ops.
    fn hunk_target(&self, file_idx: usize) -> Option<HunkTarget> {
        let DiffViewState::Ready {
            path,
            staged,
            untracked,
            diffs,
            ..
        } = &self.state
        else {
            return None;
        };
        let orig = diffs.get(file_idx)?;
        let regions = oximux_core::change_regions(orig);
        let file = FileDiff {
            path: orig.path.clone(),
            status: orig.status.clone(),
            hunks: regions.into_iter().map(|r| r.stage_hunk).collect(),
            large: false,
        };
        Some(HunkTarget {
            path: path.clone(),
            staged: *staged,
            untracked: *untracked,
            file,
        })
    }

    /// Stage the hunk at `hunk_idx` within `file_idx`. No-op if the view
    /// isn't Ready, the file is untracked (untracked → whole-file stage
    /// only), the view is already showing the staged side, or the index
    /// is out of range. Reloads the diff on completion.
    pub fn stage_hunk(&mut self, file_idx: usize, hunk_idx: usize, cx: &mut Context<Self>) {
        let Some(target) = self.hunk_target(file_idx) else {
            return;
        };
        if target.staged || target.untracked {
            return;
        }
        if hunk_idx >= target.file.hunks.len() {
            return;
        }
        let repo = self.repo.clone();
        let file = target.file;
        self.spawn_hunk_op(target.path, target.staged, target.untracked, cx, async move {
            repo.stage_hunks(&file, &[hunk_idx])
                .await
                .map_err(|e| e.to_string())
        });
    }

    /// Unstage the hunk at `hunk_idx` within `file_idx`. No-op if the
    /// view isn't Ready, the file is untracked, the view is showing the
    /// unstaged side, or the index is out of range. Reloads on
    /// completion.
    pub fn unstage_hunk(&mut self, file_idx: usize, hunk_idx: usize, cx: &mut Context<Self>) {
        let Some(target) = self.hunk_target(file_idx) else {
            return;
        };
        if !target.staged || target.untracked {
            return;
        }
        if hunk_idx >= target.file.hunks.len() {
            return;
        }
        let repo = self.repo.clone();
        let file = target.file;
        self.spawn_hunk_op(target.path, target.staged, target.untracked, cx, async move {
            repo.unstage_hunks(&file, &[hunk_idx])
                .await
                .map_err(|e| e.to_string())
        });
    }

    /// Open the type-to-confirm modal for "Discard this hunk?". On
    /// confirm, runs `discard_hunks` and reloads. No-op when the view
    /// isn't Ready, the file is untracked, the staged side is on
    /// screen (discard is worktree-only — user must unstage first), or
    /// the index is out of range. First-open-wins: a re-fire while a
    /// dialog is already mounted is ignored so a rapid double-click
    /// doesn't replace a half-typed confirm string.
    pub fn request_discard_hunk(
        &mut self,
        file_idx: usize,
        hunk_idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirm_dialog.is_some() {
            return;
        }
        let Some(target) = self.hunk_target(file_idx) else {
            return;
        };
        if target.staged || target.untracked {
            return;
        }
        if hunk_idx >= target.file.hunks.len() {
            return;
        }

        let weak = cx.entity().downgrade();
        let on_confirm: ConfirmCallback = Rc::new(move |_window, cx| {
            let _ = weak.update(cx, |view, cx| {
                view.confirmed_discard_hunk(file_idx, hunk_idx, cx);
            });
        });
        // Cancel path is purely cosmetic — the observer below drops the
        // slot when `is_cancelled()` flips. No host-side state to clear.
        let on_cancel: ConfirmCallback = Rc::new(|_window, _cx| {});

        let prompt = ConfirmPrompt {
            title: "Discard this hunk?".into(),
            body: "This will revert this hunk in the worktree. The index is \
                   untouched. This cannot be undone."
                .into(),
            expected: "Discard".into(),
            on_confirm,
            confirm_label: Some("Discard".into()),
            on_cancel: Some(on_cancel),
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let dialog =
            cx.new(|cx| ConfirmDialog::new(prompt, theme, density, typography, window, cx));

        self._confirm_dialog_observer = Some(cx.observe_in(
            &dialog,
            window,
            |view, dialog, _window, cx| {
                let d = dialog.read(cx);
                if d.is_confirmed() || d.is_cancelled() {
                    view.confirm_dialog = None;
                    view._confirm_dialog_observer = None;
                    cx.notify();
                }
            },
        ));
        self.confirm_dialog = Some(dialog);
        cx.notify();
    }

    /// Wired by the `ConfirmDialog` on-confirm callback. The dialog has
    /// already validated typed input; this method runs the destructive
    /// op + reload chain. Public for tests + the callback closure (must
    /// be reachable from `&mut Self`).
    pub fn confirmed_discard_hunk(
        &mut self,
        file_idx: usize,
        hunk_idx: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.hunk_target(file_idx) else {
            return;
        };
        if target.staged || target.untracked {
            return;
        }
        if hunk_idx >= target.file.hunks.len() {
            return;
        }
        let repo = self.repo.clone();
        let file = target.file;
        self.spawn_hunk_op(target.path, target.staged, target.untracked, cx, async move {
            repo.discard_hunks(&file, &[hunk_idx])
                .await
                .map_err(|e| e.to_string())
        });
    }

    /// Shared tokio→oneshot→gpui machinery for stage / unstage / discard.
    /// Spawns the future on tokio, awaits on the gpui side, and reloads
    /// the diff via `load()` with the same routing the initial fetch
    /// used. Errors from the underlying git op are logged; the reload
    /// still runs so the user sees the actual git state.
    fn spawn_hunk_op<F>(
        &mut self,
        path: PathBuf,
        staged: bool,
        untracked: bool,
        cx: &mut Context<Self>,
        op: F,
    ) where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<Result<(), String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = op.await;
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    "no tokio runtime entered; hunk op skipped"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            if let Err(err) = result {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    %err,
                    "hunk op failed; reloading to surface live state"
                );
            }
            let _ = this.update(cx, |view, cx| {
                view.load(path, staged, untracked, cx);
            });
        });
        self._op_task = Some(task);
    }

    fn on_expand_diff(&mut self, _: &ExpandDiff, _window: &mut Window, cx: &mut Context<Self>) {
        self.expand();
        cx.notify();
    }

    fn on_retry_diff(&mut self, _: &RetryDiff, _window: &mut Window, cx: &mut Context<Self>) {
        self.retry(cx);
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
        // When no file is selected, collapse to a zero-height placeholder so
        // the source-control panel flows directly from the staged-files list
        // into the commit graph. Earlier builds rendered a full-width
        // "Select a file to view diff" stripe that wasted vertical space.
        if matches!(self.state, DiffViewState::Empty) {
            return div()
                .track_focus(&self.focus_handle)
                .on_action(cx.listener(Self::on_expand_diff))
                .on_action(cx.listener(Self::on_retry_diff))
                .into_any_element();
        }

        // Rebuild the cached row list only when stale. This keeps the
        // expensive work — walking every hunk, syntect highlighting each
        // line, word-diff pairing — OFF the per-frame render path; it runs
        // once per (diff, expanded) change, not on every scroll tick.
        // Done before borrowing `&self.state` for the body so the heavy
        // plan build doesn't tangle with the element tree below.
        if self.prepared.is_none() {
            let rows = match &self.state {
                DiffViewState::Ready {
                    diffs, expanded, ..
                }
                | DiffViewState::CommitReady {
                    diffs, expanded, ..
                } => {
                    let plan = build_render_plan(diffs, *expanded);
                    // Stageable change regions per file — full-file context
                    // makes one giant hunk, so the renderer docks a chip bar
                    // per region (git add -p granularity) using these.
                    let regions: Vec<Vec<oximux_core::ChangeRegion>> =
                        diffs.iter().map(oximux_core::change_regions).collect();
                    let rctx = RenderCtx {
                        theme: self.theme,
                        density: self.density,
                        typography: &self.typography,
                    };
                    Some(Rc::new(prepare(&plan, &regions, &rctx)))
                }
                _ => None,
            };
            self.prepared = rows;
        }

        let rctx = RenderCtx {
            theme: self.theme,
            density: self.density,
            typography: &self.typography,
        };
        // Resolve action-chip visibility BEFORE the match — inside the
        // arm, `&self.state` is borrowed and `current_side` (which reads
        // `&self.state`) would conflict.
        let side = self.current_side();
        // Weak handle routes chip / expand / copy clicks back into the
        // view from the virtualized list's App-scope render closure (which
        // doesn't carry a `Context<DiffView>`).
        let weak = cx.entity().downgrade();
        let body = match &self.state {
            DiffViewState::Empty => unreachable!("handled above"),
            DiffViewState::Loading { path, .. } => {
                loading_state(&path.display().to_string(), &rctx).into_any_element()
            }
            DiffViewState::Failed { path, error, .. } => {
                failed_state(&path.display().to_string(), error, &rctx, cx).into_any_element()
            }
            DiffViewState::Ready { .. } => {
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(rows, side, &self.scroll_handle, &rctx, weak).into_any_element()
            }
            DiffViewState::CommitLoading {
                short_oid, subject, ..
            } => loading_state(
                &format!("Loading commit {short_oid}: {subject}…"),
                &rctx,
            )
            .into_any_element(),
            DiffViewState::CommitFailed {
                short_oid, error, ..
            } => failed_state(
                &format!("commit {short_oid}"),
                error,
                &rctx,
                cx,
            )
            .into_any_element(),
            DiffViewState::CommitReady { .. } => {
                // `side = None` so hunk action chips do NOT render on
                // commit-detail rows — Stage/Unstage/Discard against a
                // historical commit model nothing git supports.
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(rows, None, &self.scroll_handle, &rctx, weak).into_any_element()
            }
        };
        // The body owns its own scrolling now: `Ready`/`CommitReady` return
        // a `uniform_list` (flex_1 + min_h(0)) that virtualizes rows and
        // reports an exact content height, so scrolling reaches the true
        // end of the diff. Non-Ready states return a centered placeholder.
        let scroll_body = body;
        // When the discard-hunk confirm modal is mounted, stack it as a
        // centered overlay over the diff body. Mirrors `confirm_dialog`
        // and `rename_tab_dialog` overlay shape in `workspace_root` but
        // scoped to THIS diff tab (other open diff tabs each carry their
        // own slot) so a destructive action in one tab doesn't backdrop
        // the rest of the workspace.
        let overlay = self.confirm_dialog.clone().map(|dialog| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.0))
                .child(dialog)
        });
        let mut root = div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_expand_diff))
            .on_action(cx.listener(Self::on_retry_diff))
            .relative()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(self.theme.bg_base)
            .border_l_1()
            .border_color(self.theme.border_inactive)
            .child(scroll_body);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        root.into_any_element()
    }
}

fn loading_state(path: &str, rctx: &RenderCtx<'_>) -> impl IntoElement {
    centered(format!("Loading diff for {path}…"), rctx)
}

fn failed_state(
    path: &str,
    error: &str,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<DiffView>,
) -> impl IntoElement {
    let pad = px(rctx.density.pad_panel);
    let text_size = px(rctx.typography.t_body_sm);
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(8.0))
        .h_full()
        .w_full()
        .p(pad)
        .text_size(text_size)
        .text_color(rctx.theme.fg_subtle)
        .child(
            div()
                .text_color(rctx.theme.status_error)
                .child(format!("Failed to load {path}")),
        )
        .child(div().child(error.to_string()))
        .child(
            // Retry button — clickable row that dispatches `RetryDiff`.
            // The action handler reads path/staged/untracked off the
            // current `Failed` state and re-runs the load with the same
            // routing the original call used.
            div()
                .id("diff-view-retry")
                .px(px(12.0))
                .py(px(6.0))
                .text_color(rctx.theme.status_info)
                .border_1()
                .border_color(rctx.theme.border_inactive)
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|s| s.bg(rctx.theme.bg_panel_alt))
                .on_click(cx.listener(|view, _: &gpui::ClickEvent, _, cx| {
                    view.retry(cx);
                    cx.notify();
                }))
                .child("Retry"),
        )
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
