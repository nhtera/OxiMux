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

pub mod file_header;
pub mod file_rail;
pub mod hunk_actions;
pub mod note_repo_handle;
pub mod paint;
pub mod render;
pub mod review_note_popover;
pub mod review_notes;
pub mod syntax;
pub mod word_diff;

use crate::actions::{ExpandDiff, RetryDiff, SendTextToActiveAgent};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::diff_view::file_header::{
    StickyHeader, build_first_row_of_file, build_row_owner, collect_headers, sticky_header_overlay,
};
use crate::shell::diff_view::file_rail::{RAIL_WIDTH, RailContext, file_rail};
use crate::shell::diff_view::note_repo_handle::note_repo;
use crate::shell::diff_view::paint::{
    FoldId, OverviewRun, PreparedRow, mark_notes, overview_ruler, overview_runs, prepare,
    prepare_split, render_rows, staging_card_overlay, widest_row_chars, widest_row_index,
};
use crate::shell::diff_view::render::{FilePlan, RenderCtx, build_render_plan};
use crate::shell::diff_view::review_note_popover::{
    ReviewNoteCallback, ReviewNoteOutcome, ReviewNotePopover,
};
use crate::shell::diff_view::review_notes::{NoteAnchor, ReviewNoteStore, format_notes_markdown};
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollStrategy, StatefulInteractiveElement as _, Styled,
    Subscription, Task, UniformListScrollHandle, Window, div, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, Sizable as _};
use oximux_core::{CombinedDiffScope, FileDiff, FileGroup, NoteSide};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::collections::{HashMap, HashSet};
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
    /// Range mode: a single file's diff across `base..head` (the
    /// "Committed on Branch" section's per-file view). Read-only like
    /// commit-detail — no staging chips — but keyed by a revision range +
    /// path rather than a SHA. `title` is the display label (file name).
    RangeLoading {
        base: String,
        head: String,
        path: PathBuf,
        title: String,
    },
    RangeReady {
        base: String,
        head: String,
        path: PathBuf,
        title: String,
        diffs: Vec<FileDiff>,
        expanded: bool,
    },
    RangeFailed {
        base: String,
        head: String,
        path: PathBuf,
        title: String,
        error: String,
    },
    /// Combined multi-file working-tree mode: every changed file in `scope`
    /// (all-changes / staged / untracked / branch-range) in one virtualized
    /// scroll. `groups[i]` tags `diffs[i]`'s partition so per-region staging
    /// routes the right side (Unstaged→stage, Staged→unstage) and read-only
    /// `Committed` files suppress chips. Reloads re-run `load_combined(scope)`
    /// so a stage inside any file refreshes the whole combined view in place.
    CombinedLoading {
        scope: CombinedDiffScope,
    },
    CombinedReady {
        scope: CombinedDiffScope,
        diffs: Vec<FileDiff>,
        /// Parallel to `diffs` — the partition tag driving staging routing.
        groups: Vec<FileGroup>,
        expanded: bool,
    },
    CombinedFailed {
        scope: CombinedDiffScope,
        error: String,
    },
}

/// Public visibility-decision payload returned by `DiffView::side_for_region`.
/// Drives the `hunk_actions` overlay's button gating without exposing
/// the full `DiffViewState` enum to the renderer.
#[derive(Debug, Clone, Copy)]
pub struct HunkActionSide {
    pub staged: bool,
    pub untracked: bool,
}

/// How a post-hunk-op reload re-fetches the diff. Single-file mode replays
/// `load()` with the same routing the initial fetch used; combined mode
/// replays `load_combined(scope)` so staging inside one file refreshes the
/// whole combined view instead of collapsing it to a single file.
enum ReloadTarget {
    Single {
        path: PathBuf,
        staged: bool,
        untracked: bool,
    },
    Combined {
        scope: CombinedDiffScope,
    },
}

/// Snapshot of the file under cursor + the routing fields needed for a
/// post-op reload. Kept private — `stage_hunk` / `unstage_hunk` /
/// `confirmed_discard_hunk` build one before spawning their tokio task.
/// `staged`/`untracked` are the file's OWN action side (per-file-group in
/// combined mode), gating which op is legal.
struct HunkTarget {
    staged: bool,
    untracked: bool,
    file: FileDiff,
    reload: ReloadTarget,
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
    /// Side-by-side toggle. `false` → unified/inline body (default); `true`
    /// → original | modified columns. Flipping it rebuilds `prepared` (the
    /// two modes emit different row sets) so it routes through
    /// `invalidate_prepared`.
    split: bool,
    /// Index of the widest prepared row — the `uniform_list` measurement
    /// item that sets the horizontal scroll range. Recomputed alongside
    /// `prepared`; `0` when there are no rows.
    prepared_widest: usize,
    /// Character count of the widest row's content — clamps the split-mode
    /// horizontal offset so you can't scroll past the longest line.
    prepared_widest_chars: usize,
    /// Synced horizontal scroll offset (px) for side-by-side columns. Inline
    /// mode uses the list's native h-scroll instead; split mode shifts each
    /// column's CONTENT by this (gutters stay sticky). Reset on rebuild.
    split_h_offset: f32,
    /// Region the pointer is currently over, as `(file_idx, region_idx)`.
    /// Drives the gutter-sliver widen across every row of that region. Pure
    /// view state — changing it never invalidates `prepared` (no re-highlight
    /// on hover), only triggers a cheap re-render of the visible rows.
    hovered_region: Option<(usize, usize)>,
    /// Prepared-row index the pointer is currently over (a changed row only;
    /// `None` over context). Floats the staging card at THIS row's on-screen
    /// Y — i.e. right where the pointer is — rather than at the region's
    /// first line, which (when `change_regions` merges nearby edits into one
    /// region) can be far above the change being hovered. Set together with
    /// `hovered_region`.
    hovered_row: Option<usize>,
    /// Overview-ruler runs (change positions as 0..1 fractions, coalesced
    /// per change block). Recomputed alongside `prepared`; empty when the
    /// body has no changes. Cached behind `Rc` so the per-frame render only
    /// paints the bars, never rescans the row list.
    overview: Rc<Vec<OverviewRun>>,
    /// File indices the user has folded. A folded file emits its header
    /// only (body suppressed in `prepare`). Pure view state; reset on every
    /// fresh selection (`load`/`load_commit`/`load_range`) so a new file
    /// always opens expanded. Toggling routes through `invalidate_prepared`.
    collapsed: HashSet<usize>,
    /// Context runs the user has expanded, keyed by `FoldId`
    /// (`(file_idx, first-hidden-line)`). A run whose id is present renders
    /// in full instead of behind a `⋯ N unchanged lines` expander. Pure view
    /// state; reset on every fresh selection (`load`/`load_commit`/
    /// `load_range`). Toggling routes through `invalidate_prepared`.
    expanded_folds: HashSet<FoldId>,
    /// The file_idx owning each prepared row (most recent header above it).
    /// Built alongside `prepared`; lets the sticky overlay resolve "which
    /// file sits at the top of the viewport" in O(1) from the scroll offset.
    row_owner: Rc<Vec<usize>>,
    /// Per-file header metadata, in file order, for the sticky overlay.
    /// Built alongside `prepared`.
    headers: Rc<Vec<StickyHeader>>,
    /// First prepared-row index of each file (its `FileHeader` row), indexed
    /// by `file_idx`. Built alongside `prepared`; the file rail uses it to
    /// jump the body to a clicked file in O(1).
    first_row_of_file: Rc<Vec<usize>>,
    /// Whether the in-diff file rail is revealed. Open by default for
    /// multi-file diffs (it mirrors the reference combined-diff navigator); a
    /// toolbar toggle flips it. Pure per-session view state — never
    /// invalidates `prepared`.
    rail_open: bool,
    /// Rail directory rows the user has collapsed, by full path. A folder
    /// whose path is present renders its chevron closed and hides its
    /// subtree. Pure view state.
    rail_collapsed_dirs: HashSet<PathBuf>,
    /// Live filter for the rail's file list. Lazily created the first time
    /// the rail renders (needs a `Window`); typing narrows the rows.
    rail_filter: Option<Entity<InputState>>,
    /// Re-render subscription on `rail_filter`'s change events.
    _rail_filter_sub: Option<Subscription>,
    /// One-shot scroll target consumed on the next prepared rebuild — set by
    /// a hunk op so the post-stage reload restores the reader's position
    /// instead of snapping to the top. `None` when idle.
    pending_scroll_anchor: Option<usize>,
    /// Review notes for the current diff scope, keyed by `(path, line, side)`.
    /// Mirrored from SQLite on each `*Ready` load and written back through the
    /// process-wide `DiffReviewNoteRepo`. The prepare pass reads it to mark
    /// noted lines; empty for non-`Ready` states.
    notes: ReviewNoteStore,
    /// Active compose/edit popover (per-request; `None` when idle). Mounted
    /// INSIDE this DiffView's render tree like `confirm_dialog` so it scopes
    /// to the diff surface the user is reading.
    note_popover: Option<Entity<ReviewNotePopover>>,
    /// Per-mount observer on the active popover — clears the slot when the
    /// popover closes. Reset each time a new popover mounts.
    _note_popover_observer: Option<Subscription>,
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
            prepared_widest: 0,
            prepared_widest_chars: 0,
            split_h_offset: 0.0,
            hovered_region: None,
            hovered_row: None,
            overview: Rc::new(Vec::new()),
            split: false,
            collapsed: HashSet::new(),
            expanded_folds: HashSet::new(),
            row_owner: Rc::new(Vec::new()),
            headers: Rc::new(Vec::new()),
            first_row_of_file: Rc::new(Vec::new()),
            rail_open: true,
            rail_collapsed_dirs: HashSet::new(),
            rail_filter: None,
            _rail_filter_sub: None,
            pending_scroll_anchor: None,
            notes: ReviewNoteStore::new(),
            note_popover: None,
            _note_popover_observer: None,
        }
    }

    /// Fold/unfold a single file's body. Cheap — only the row set changes,
    /// so it routes through `invalidate_prepared` (which drops the cached
    /// rows + stale hover) and re-renders.
    fn toggle_file_fold(&mut self, file_idx: usize) {
        if !self.collapsed.remove(&file_idx) {
            self.collapsed.insert(file_idx);
        }
        self.invalidate_prepared();
    }

    /// Expand a single collapsed context run so its hidden lines render in
    /// full. Cheap — only the row set changes, so it routes through
    /// `invalidate_prepared`. The fold's `FoldId` persists until the next
    /// fresh selection (a stale id for a run that no longer exists is inert).
    fn expand_fold(&mut self, fold_id: FoldId) {
        self.expanded_folds.insert(fold_id);
        self.invalidate_prepared();
    }

    /// Collapse every file (Collapse all) or expand every file (Expand all).
    /// Folds the set of files present in the current diff; expanding just
    /// clears the set.
    fn set_all_folded(&mut self, folded: bool, cx: &mut Context<Self>) {
        if folded {
            let n = self.current_file_count();
            self.collapsed = (0..n).collect();
        } else {
            self.collapsed.clear();
        }
        self.invalidate_prepared();
        cx.notify();
    }

    /// True when every file in the current diff is folded — drives the
    /// toolbar button label (Collapse all ↔ Expand all).
    fn all_folded(&self) -> bool {
        let n = self.current_file_count();
        n > 0 && self.collapsed.len() >= n
    }

    /// Number of files in the current Ready/CommitReady/RangeReady diff.
    fn current_file_count(&self) -> usize {
        match &self.state {
            DiffViewState::Ready { diffs, .. }
            | DiffViewState::CommitReady { diffs, .. }
            | DiffViewState::RangeReady { diffs, .. }
            | DiffViewState::CombinedReady { diffs, .. } => diffs.len(),
            _ => 0,
        }
    }

    /// First-visible row index, derived from the list's pixel scroll offset
    /// and the fixed row height. Every row is exactly `density.h_row` tall,
    /// so this is exact. Returns 0 when nothing is scrolled or measured.
    fn first_visible_row(&self) -> usize {
        let h = self.density.h_row;
        if h <= 0.0 {
            return 0;
        }
        let offset_y = f32::from(self.scroll_handle.0.borrow().base_handle.offset().y);
        ((-offset_y) / h).floor().max(0.0) as usize
    }

    /// Scroll the body so `row` sits at the top of the viewport. Used by the
    /// overview-ruler click-to-jump.
    pub fn scroll_to_row(&self, row: usize) {
        self.scroll_handle.scroll_to_item(row, ScrollStrategy::Top);
    }

    /// Scroll the body so `file_idx`'s header sits at the top. Used by the
    /// file-rail click-to-jump; a stale index (no matching row) is inert.
    pub(crate) fn scroll_to_file(&self, file_idx: usize) {
        if let Some(&row) = self.first_row_of_file.get(file_idx) {
            self.scroll_to_row(row);
        }
    }

    /// Reveal / hide the opt-in file rail. Pure view state — the body row set
    /// is unchanged, so this never invalidates `prepared`.
    fn toggle_rail(&mut self, cx: &mut Context<Self>) {
        self.rail_open = !self.rail_open;
        cx.notify();
    }

    /// Collapse / expand a rail directory row. Pure view state.
    fn toggle_rail_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if !self.rail_collapsed_dirs.remove(&dir) {
            self.rail_collapsed_dirs.insert(dir);
        }
        cx.notify();
    }

    /// Clear rail folder-collapse + filter state on a fresh selection so a new
    /// diff opens fully expanded and unfiltered (dropping the filter entity
    /// re-creates it lazily). Called from every `load*` entry point.
    fn reset_rail_state(&mut self) {
        self.rail_collapsed_dirs.clear();
        self.rail_filter = None;
        self._rail_filter_sub = None;
    }

    /// Flip inline ↔ side-by-side. Rebuilds the row set (the two modes differ
    /// in layout) and clears hover; cheap — the diff data is unchanged, only
    /// the flattening differs.
    fn toggle_split(&mut self, cx: &mut Context<Self>) {
        self.split = !self.split;
        self.invalidate_prepared();
        cx.notify();
    }

    /// Update the hovered region + row (from a body row's `on_hover`). The
    /// region drives the sliver widen; the row floats the staging card at the
    /// pointer's Y. Cheap — re-renders the visible rows without touching the
    /// `prepared` cache. No-op when unchanged so a stationary pointer doesn't
    /// spam `notify`.
    fn set_hover(
        &mut self,
        region: Option<(usize, usize)>,
        row: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.hovered_region != region || self.hovered_row != row {
            self.hovered_region = region;
            self.hovered_row = row;
            cx.notify();
        }
    }

    /// Invalidate the cached render rows so the next `render` rebuilds them
    /// from the current state. Called on every transition that changes the
    /// diff body (load, commit load, expand, seed).
    fn invalidate_prepared(&mut self) {
        self.prepared = None;
        // Region indices + row indices are only meaningful against the
        // current row set; a rebuild may renumber them, so drop any stale
        // hover (region for slivers + row for the card position).
        self.hovered_region = None;
        self.hovered_row = None;
        // A new body means a new max line length — reset the split scroll so
        // we don't start mid-line on a different file.
        self.split_h_offset = 0.0;
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
        // A fresh selection always opens fully expanded (no folded files, no
        // pre-expanded context runs from the prior file).
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
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
                view.reload_notes();
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
            DiffViewState::RangeFailed {
                base,
                head,
                path,
                title,
                ..
            } => {
                let (base, head, path, title) =
                    (base.clone(), head.clone(), path.clone(), title.clone());
                self.load_range(base, head, path, title, cx);
            }
            DiffViewState::CombinedFailed { scope, .. } => {
                let scope = scope.clone();
                self.load_combined(scope, cx);
            }
            _ => {}
        }
    }

    /// Begin loading the per-file diff for a commit. Bypasses the
    /// file/staged routing — uses `repo.commit_files(sha)` to fetch
    /// every file the commit touches, then mounts them in the same
    /// `Vec<FileDiff>` shape the unstaged/staged path uses. Hunk
    /// action chips do NOT render on this side (`side_for_region`
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
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
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
                view.reload_notes();
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

    /// Begin loading a single file's diff across `base..head` — the
    /// read-only per-file view opened from the "Committed on Branch"
    /// section. Mirrors `load_commit`'s async machinery but fetches
    /// `diff_for_range(base, head, path)` and lands in the `Range*`
    /// states (no staging chips, since the change is already committed).
    pub fn load_range(
        &mut self,
        base: String,
        head: String,
        path: PathBuf,
        title: String,
        cx: &mut Context<Self>,
    ) {
        self._op_task = None;
        self.invalidate_prepared();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        self.state = DiffViewState::RangeLoading {
            base: base.clone(),
            head: head.clone(),
            path: path.clone(),
            title: title.clone(),
        };
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<FileDiff>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let (base_f, head_f, path_f) = (base.clone(), head.clone(), path.clone());
                handle.spawn(async move {
                    let r = repo
                        .diff_for_range(&base_f, &head_f, &path_f)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    "no tokio runtime entered; range load skipped"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                view.apply_range_load_result(base, head, path, title, result);
                view.reload_notes();
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    fn apply_range_load_result(
        &mut self,
        base: String,
        head: String,
        path: PathBuf,
        title: String,
        result: Result<Vec<FileDiff>, String>,
    ) {
        match result {
            Ok(diffs) => {
                // Preserve `expanded` across reloads of the same range+path
                // (e.g. retry). A different range or file starts collapsed.
                let expanded = match &self.state {
                    DiffViewState::RangeReady {
                        base: pb,
                        head: ph,
                        path: pp,
                        expanded: pe,
                        ..
                    } if *pb == base && *ph == head && *pp == path => *pe,
                    _ => false,
                };
                self.state = DiffViewState::RangeReady {
                    base,
                    head,
                    path,
                    title,
                    diffs,
                    expanded,
                };
            }
            Err(error) => {
                self.state = DiffViewState::RangeFailed {
                    base,
                    head,
                    path,
                    title,
                    error,
                };
            }
        }
    }

    /// Begin loading a combined multi-file diff for `scope`. Fans out to
    /// `repo.diff_combined(scope)` (staged + unstaged + untracked, or a
    /// scoped subset) and lands in the `Combined*` states. The same
    /// `Vec<FileDiff>` render path serves single-file, commit, range, and
    /// combined views; only the staging routing differs (per-file-group).
    pub fn load_combined(&mut self, scope: CombinedDiffScope, cx: &mut Context<Self>) {
        // Same drop-on-entry + fresh-selection reset as `load()`.
        self._op_task = None;
        self.invalidate_prepared();
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        self.state = DiffViewState::CombinedLoading {
            scope: scope.clone(),
        };
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<oximux_core::CombinedDiff, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let scope_for_fetch = scope.clone();
                handle.spawn(async move {
                    let r = repo
                        .diff_combined(scope_for_fetch)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    "no tokio runtime entered; combined load skipped"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                view.apply_combined_result(scope, result);
                view.reload_notes();
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    fn apply_combined_result(
        &mut self,
        scope: CombinedDiffScope,
        result: Result<oximux_core::CombinedDiff, String>,
    ) {
        match result {
            Ok(combined) => {
                // Preserve `expanded` across reloads of the SAME scope (a hunk
                // op chains a combined reload); a different scope starts
                // collapsed.
                let expanded = match &self.state {
                    DiffViewState::CombinedReady {
                        scope: prev,
                        expanded: prev_expanded,
                        ..
                    } if *prev == scope => *prev_expanded,
                    _ => false,
                };
                self.state = DiffViewState::CombinedReady {
                    scope,
                    diffs: combined.diffs,
                    groups: combined.groups,
                    expanded,
                };
            }
            Err(error) => {
                self.state = DiffViewState::CombinedFailed { scope, error };
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
            DiffViewState::RangeReady { expanded, .. } => *expanded = true,
            DiffViewState::CombinedReady { expanded, .. } => *expanded = true,
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

    /// Test-only: stamp the view into `CombinedReady` with pre-fetched diffs
    /// + parallel group tags, bypassing the async `load_combined` chain.
    #[doc(hidden)]
    pub fn seed_combined_for_test(
        &mut self,
        scope: CombinedDiffScope,
        diffs: Vec<FileDiff>,
        groups: Vec<FileGroup>,
    ) {
        self.state = DiffViewState::CombinedReady {
            scope,
            diffs,
            groups,
            expanded: false,
        };
        self.invalidate_prepared();
    }

    /// Which action side applies to the region in `file_idx` — gating the
    /// floating staging card. In single-file `Ready` every region shares the
    /// view's `(staged, untracked)`. In `CombinedReady` it's resolved
    /// per-file from the group tag (`Unstaged`→stage, `Staged`→unstage,
    /// `Untracked`→whole-file only, `Committed`→read-only/no card). Returns
    /// `None` for commit/range views (read-only) and out-of-range indices.
    pub fn side_for_region(&self, file_idx: usize) -> Option<HunkActionSide> {
        match &self.state {
            DiffViewState::Ready {
                staged, untracked, ..
            } => Some(HunkActionSide {
                staged: *staged,
                untracked: *untracked,
            }),
            DiffViewState::CombinedReady { groups, .. } => match groups.get(file_idx)? {
                FileGroup::Unstaged => Some(HunkActionSide {
                    staged: false,
                    untracked: false,
                }),
                FileGroup::Staged => Some(HunkActionSide {
                    staged: true,
                    untracked: false,
                }),
                FileGroup::Untracked => Some(HunkActionSide {
                    staged: false,
                    untracked: true,
                }),
                // Already committed — no staging chrome.
                FileGroup::Committed => None,
            },
            _ => None,
        }
    }

    /// Build a STAGING snapshot for `file_idx`: a `FileDiff` whose `hunks`
    /// are the file's stageable change regions (from `change_regions`), not
    /// its full-file display hunks. Returns `None` when the view isn't a
    /// stageable mode (`Ready`/`CombinedReady`), the file is read-only
    /// (`Committed` group), or `file_idx` is out of range.
    ///
    /// Diffs are fetched with full-file context so the user can scroll the
    /// whole document, which collapses every edit into one giant hunk. The
    /// per-hunk Stage/Unstage/Discard chips index REGIONS, so the op path
    /// rebuilds a `FileDiff` carrying one `git apply`-ready hunk per region
    /// — then `repo.stage_hunks(&file, &[region_idx])` works unchanged with
    /// per-region (git add -p) granularity instead of whole-file ops.
    ///
    /// In combined mode the file's own path + group drive the op + reload:
    /// the op targets that file, and the post-op reload re-runs the combined
    /// scope (not a single-file load) so the whole view refreshes in place.
    fn hunk_target(&self, file_idx: usize) -> Option<HunkTarget> {
        let (orig, staged, untracked, reload) = match &self.state {
            DiffViewState::Ready {
                path,
                staged,
                untracked,
                diffs,
                ..
            } => {
                let orig = diffs.get(file_idx)?;
                let reload = ReloadTarget::Single {
                    path: path.clone(),
                    staged: *staged,
                    untracked: *untracked,
                };
                (orig, *staged, *untracked, reload)
            }
            DiffViewState::CombinedReady {
                scope,
                diffs,
                groups,
                ..
            } => {
                let orig = diffs.get(file_idx)?;
                let (staged, untracked) = match groups.get(file_idx)? {
                    FileGroup::Unstaged => (false, false),
                    FileGroup::Staged => (true, false),
                    FileGroup::Untracked => (false, true),
                    // Read-only: never stageable, even if a chip somehow fired.
                    FileGroup::Committed => return None,
                };
                let reload = ReloadTarget::Combined {
                    scope: scope.clone(),
                };
                (orig, staged, untracked, reload)
            }
            _ => return None,
        };
        let regions = oximux_core::change_regions(orig);
        let file = FileDiff {
            path: orig.path.clone(),
            status: orig.status.clone(),
            hunks: regions.into_iter().map(|r| r.stage_hunk).collect(),
            large: false,
        };
        Some(HunkTarget {
            staged,
            untracked,
            file,
            reload,
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
        self.spawn_hunk_op(target.reload, cx, async move {
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
        self.spawn_hunk_op(target.reload, cx, async move {
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
        self.spawn_hunk_op(target.reload, cx, async move {
            repo.discard_hunks(&file, &[hunk_idx])
                .await
                .map_err(|e| e.to_string())
        });
    }

    /// Shared tokio→oneshot→gpui machinery for stage / unstage / discard.
    /// Spawns the future on tokio, awaits on the gpui side, and reloads the
    /// diff via `reload` (single-file `load` or `load_combined`) with the
    /// same routing the initial fetch used. Errors from the underlying git
    /// op are logged; the reload still runs so the user sees the actual git
    /// state.
    fn spawn_hunk_op<F>(&mut self, reload: ReloadTarget, cx: &mut Context<Self>, op: F)
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        // Remember where the reader is BEFORE the op so the post-op reload
        // restores their position instead of snapping to the top.
        let anchor = self.first_visible_row();
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
                // `load`/`load_combined` clears any anchor via the
                // `collapsed.clear()`-adjacent reset, so set it AFTER the
                // reload is scheduled — it's consumed on the next prepared
                // rebuild once the Ready state lands.
                match reload {
                    ReloadTarget::Single {
                        path,
                        staged,
                        untracked,
                    } => view.load(path, staged, untracked, cx),
                    ReloadTarget::Combined { scope } => view.load_combined(scope, cx),
                }
                view.pending_scroll_anchor = Some(anchor);
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

    /// Mirror the current scope's persisted review notes into the in-memory
    /// store. Called after every `*Ready` load. Clears the store for
    /// non-`Ready` states (no diff_ref → no notes). No-op when no repo handle
    /// is installed (pure unit tests that never boot the app — notes degrade
    /// to in-memory-only). Reads SQLite synchronously: the table is tiny (a
    /// review is tens of notes) and local, so this stays off the async path.
    fn reload_notes(&mut self) {
        let Some(diff_ref) = diff_ref_for(&self.state) else {
            self.notes.clear();
            self.invalidate_prepared();
            return;
        };
        let Some(repo) = note_repo() else {
            return;
        };
        let scope = self.scope_key();
        match repo.list_for_scope(&scope, &diff_ref) {
            Ok(notes) => self.notes.load(notes),
            Err(err) => {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    %err,
                    "review-note load failed"
                );
                self.notes.clear();
            }
        }
        self.invalidate_prepared();
    }

    /// Open the compose/edit popover anchored to `anchor`. Pre-fills with the
    /// existing note body (if any). First-open-wins: a re-click while a
    /// popover is mounted is ignored so a half-typed note isn't replaced.
    pub fn open_note_popover(
        &mut self,
        anchor: NoteAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.note_popover.is_some() {
            return;
        }
        let existing = self.notes.get(&anchor).map(str::to_string);
        let weak = cx.entity().downgrade();
        let anchor_for_cb = anchor.clone();
        let on_commit: ReviewNoteCallback = Rc::new(move |outcome, _window, cx| {
            let _ = weak.update(cx, |view, cx| {
                view.apply_note_outcome(anchor_for_cb.clone(), outcome, cx);
            });
        });
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let popover = cx.new(|cx| {
            ReviewNotePopover::new(
                anchor, existing, on_commit, theme, density, typography, window, cx,
            )
        });
        // Focus the input so typing lands without a click first.
        popover.read(cx).input_focus_handle(cx).focus(window, cx);
        self._note_popover_observer =
            Some(cx.observe_in(&popover, window, |view, pop, _window, cx| {
                if pop.read(cx).is_closed() {
                    view.note_popover = None;
                    view._note_popover_observer = None;
                    cx.notify();
                }
            }));
        self.note_popover = Some(popover);
        cx.notify();
    }

    /// Apply a popover dismissal: Save upserts the note, Delete (or an emptied
    /// body) removes it, Cancel is inert. The store mirrors the DB write so
    /// the gutter marker updates without a full reload.
    fn apply_note_outcome(
        &mut self,
        anchor: NoteAnchor,
        outcome: ReviewNoteOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            ReviewNoteOutcome::Save(body) => {
                self.persist_note(&anchor, Some(&body));
                self.notes.set(anchor, body);
                self.invalidate_prepared();
            }
            ReviewNoteOutcome::Delete => {
                self.persist_note(&anchor, None);
                self.notes.remove(&anchor);
                self.invalidate_prepared();
            }
            ReviewNoteOutcome::Cancel => {}
        }
        cx.notify();
    }

    /// Write one note through to SQLite — `Some(body)` upserts, `None`
    /// deletes. No-op without a diff_ref (non-`Ready` state) or repo handle.
    /// Errors are logged, not surfaced: the in-memory store still reflects the
    /// user's edit, and the next load reconciles against the DB.
    fn persist_note(&self, anchor: &NoteAnchor, body: Option<&str>) {
        let Some(diff_ref) = diff_ref_for(&self.state) else {
            return;
        };
        let Some(repo) = note_repo() else {
            return;
        };
        let scope = self.scope_key();
        let res = match body {
            Some(b) => repo.upsert(&scope, &diff_ref, &anchor.path, anchor.side, anchor.line, b),
            None => repo.delete(&scope, &diff_ref, &anchor.path, anchor.side, anchor.line),
        };
        if let Err(err) = res {
            tracing::warn!(
                target: "oximux_app::diff_view",
                %err,
                "review-note persist failed"
            );
        }
    }

    /// Repository scope key for note rows — the worktree root path. Shared by
    /// load / persist / clear so the column stays identical across writes.
    fn scope_key(&self) -> String {
        self.repo.workdir().to_string_lossy().to_string()
    }

    /// Map each annotatable line's anchor to its diff line text, so the
    /// formatter can fence code context next to each note. Sourced from a
    /// fresh `build_render_plan` (the SAME `LinePlan` line numbers `mark_notes`
    /// anchors against) rather than from `self.prepared` — so it works
    /// view-mode-independently (inline AND split, which carries no per-line
    /// anchors) and before the first render. `expanded = true` so a collapsed
    /// large-diff file still yields context for its lines.
    fn note_context_map(&self) -> HashMap<NoteAnchor, String> {
        let mut map = HashMap::new();
        let diffs = match &self.state {
            DiffViewState::Ready { diffs, .. }
            | DiffViewState::CommitReady { diffs, .. }
            | DiffViewState::RangeReady { diffs, .. }
            | DiffViewState::CombinedReady { diffs, .. } => diffs,
            _ => return map,
        };
        for fp in build_render_plan(diffs, true) {
            let FilePlan::Hunked { path, hunks, .. } = fp else {
                continue;
            };
            for hunk in &hunks {
                for line in &hunk.rows {
                    let (side, n) = match (line.old_line, line.new_line) {
                        (Some(old), None) => (NoteSide::Old, old),
                        (_, Some(new)) => (NoteSide::New, new),
                        (None, None) => continue,
                    };
                    map.insert(NoteAnchor::new(path.clone(), n, side), line.content.clone());
                }
            }
        }
        map
    }

    /// Render the current scope's notes as a markdown prompt (file-grouped,
    /// each note carrying its line's code as a fenced context block). Empty
    /// string when there are no notes.
    fn notes_markdown(&self) -> String {
        let ctx = self.note_context_map();
        format_notes_markdown(&self.notes, |anchor| ctx.get(anchor).cloned())
    }

    /// Send every note to the workspace's active agent as one markdown prompt,
    /// via the same `SendTextToActiveAgent` action the terminal + palette use.
    /// No-op when there are no notes.
    fn send_notes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.notes_markdown();
        if text.is_empty() {
            return;
        }
        window.dispatch_action(Box::new(SendTextToActiveAgent { text }), cx);
    }

    /// Copy the markdown prompt to the clipboard — a non-agent escape hatch.
    fn copy_notes(&mut self, cx: &mut Context<Self>) {
        let text = self.notes_markdown();
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Drop every note for the current scope, in memory and in SQLite.
    fn clear_notes(&mut self, cx: &mut Context<Self>) {
        if self.notes.is_empty() {
            return;
        }
        if let Some(diff_ref) = diff_ref_for(&self.state)
            && let Some(repo) = note_repo()
        {
            let scope = self.scope_key();
            if let Err(err) = repo.clear_scope(&scope, &diff_ref) {
                tracing::warn!(
                    target: "oximux_app::diff_view",
                    %err,
                    "review-note clear failed"
                );
            }
        }
        // Clear the in-memory store even if the SQLite delete failed: the UI
        // reflects the user's intent now, and the next load's `reload_notes`
        // reconciles against the DB (a failed clear simply re-appears then).
        self.notes.clear();
        self.invalidate_prepared();
        cx.notify();
    }
}

/// Derive the diff-scope identity used to key review notes, from the current
/// view state. Pure (no I/O) so it's unit-testable. Returns `None` for
/// non-`Ready` states — a loading/failed/empty view carries no notes.
///
/// Stable, human-legible keys: re-opening the same diff later (same staged
/// side / commit / range / combined scope) re-attaches the same notes.
/// Untracked files fold into `worktree:unstaged` — an untracked change is a
/// not-yet-staged worktree edit, so its notes belong with the unstaged side.
pub(crate) fn diff_ref_for(state: &DiffViewState) -> Option<String> {
    match state {
        DiffViewState::Ready { staged, .. } => Some(
            if *staged {
                "worktree:staged"
            } else {
                "worktree:unstaged"
            }
            .to_string(),
        ),
        DiffViewState::CommitReady { sha, .. } => Some(format!("commit:{sha}")),
        DiffViewState::RangeReady { base, head, .. } => Some(format!("range:{base}..{head}")),
        DiffViewState::CombinedReady { scope, .. } => Some(format!("combined:{}", scope.tab_key())),
        _ => None,
    }
}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                }
                | DiffViewState::RangeReady {
                    diffs, expanded, ..
                }
                | DiffViewState::CombinedReady {
                    diffs, expanded, ..
                } => {
                    let plan = build_render_plan(diffs, *expanded);
                    // Stageable change regions per file — full-file context
                    // makes one giant hunk, so the renderer docks a chip bar
                    // per region (git add -p granularity) using these.
                    let regions: Vec<Vec<oximux_core::ChangeRegion>> =
                        diffs.iter().map(oximux_core::change_regions).collect();
                    // Hollow-vs-solid gutter slivers: only the combined view
                    // carries per-file group tags, so staged files render
                    // hollow there. Single-file / commit / range views pass an
                    // empty slice → every sliver solid (uniformly one state).
                    let staged_per_file: Vec<bool> = match &self.state {
                        DiffViewState::CombinedReady { groups, .. } => groups
                            .iter()
                            .map(|g| matches!(g, FileGroup::Staged))
                            .collect(),
                        _ => Vec::new(),
                    };
                    let rctx = RenderCtx {
                        theme: self.theme,
                        density: self.density,
                        typography: &self.typography,
                    };
                    let mut body = if self.split {
                        prepare_split(
                            &plan,
                            &regions,
                            &self.collapsed,
                            &self.expanded_folds,
                            &staged_per_file,
                            &rctx,
                        )
                    } else {
                        prepare(
                            &plan,
                            &regions,
                            &self.collapsed,
                            &self.expanded_folds,
                            &staged_per_file,
                            &rctx,
                        )
                    };
                    // Bake each line's note marker once per rebuild (off the
                    // per-frame path), parallel to the staged-sliver pass.
                    let paths: Vec<String> =
                        diffs.iter().map(|d| d.path.display().to_string()).collect();
                    mark_notes(&mut body, &paths, &self.notes);
                    Some(Rc::new(body))
                }
                _ => None,
            };
            self.prepared = rows;
            // Recompute the horizontal-scroll measurement row + its char
            // count alongside the rebuild (off the per-frame path).
            let (widest, chars) = self
                .prepared
                .as_ref()
                .map(|r| (widest_row_index(r), widest_row_chars(r)))
                .unwrap_or((0, 0));
            self.prepared_widest = widest;
            self.prepared_widest_chars = chars;
            // Overview-ruler runs come from the same rebuild so the ruler
            // paints off a cached list, never rescanning rows per frame.
            self.overview = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(overview_runs(r)))
                .unwrap_or_default();
            // Row→file map + per-file headers for the sticky overlay, also
            // off the per-frame path.
            self.row_owner = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(build_row_owner(r)))
                .unwrap_or_default();
            self.headers = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(collect_headers(r)))
                .unwrap_or_default();
            // First-row-of-file index for the file rail's click-to-jump.
            self.first_row_of_file = self
                .prepared
                .as_ref()
                .map(|r| Rc::new(build_first_row_of_file(r)))
                .unwrap_or_default();
            // A hunk op left a position to restore — apply it once the
            // rebuilt rows actually exist (NOT during the intermediate
            // Loading frame, where `prepared` is empty and the anchor would
            // be consumed before the reload lands), so staging doesn't snap
            // the reader to the top.
            let len = self.prepared.as_ref().map(|r| r.len()).unwrap_or(0);
            if len > 0
                && let Some(anchor) = self.pending_scroll_anchor.take()
            {
                self.scroll_handle
                    .scroll_to_item(anchor.min(len - 1), ScrollStrategy::Top);
            }
        }

        let rctx = RenderCtx {
            theme: self.theme,
            density: self.density,
            typography: &self.typography,
        };
        let hovered_region = self.hovered_region;
        let split = self.split;
        let split_h_offset = self.split_h_offset;
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
                render_rows(
                    rows,
                    hovered_region,
                    self.prepared_widest,
                    split,
                    split_h_offset,
                    &self.scroll_handle,
                    &rctx,
                    weak,
                )
                .into_any_element()
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
                // Read-only: the staging-card overlay is gated on `side`
                // (None here) so Stage/Unstage/Discard never appears on
                // commit-detail rows — nothing git supports against history.
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    self.prepared_widest,
                    split,
                    split_h_offset,
                    &self.scroll_handle,
                    &rctx,
                    weak,
                )
                .into_any_element()
            }
            DiffViewState::RangeLoading { title, .. } => {
                loading_state(&format!("Loading {title}…"), &rctx).into_any_element()
            }
            DiffViewState::RangeFailed { title, error, .. } => {
                failed_state(title, error, &rctx, cx).into_any_element()
            }
            DiffViewState::RangeReady { .. } => {
                // Read-only, like commit detail: `side = None` gates out the
                // staging card overlay.
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    self.prepared_widest,
                    split,
                    split_h_offset,
                    &self.scroll_handle,
                    &rctx,
                    weak,
                )
                .into_any_element()
            }
            DiffViewState::CombinedLoading { scope } => {
                loading_state(scope.title(), &rctx).into_any_element()
            }
            DiffViewState::CombinedFailed { scope, error } => {
                failed_state(scope.title(), error, &rctx, cx).into_any_element()
            }
            DiffViewState::CombinedReady { .. } => {
                // Stageable per-file-group: the card overlay resolves
                // `side_for_region` per the hovered file's tag (Unstaged →
                // Stage, Staged → Unstage, Committed → none).
                let rows = self.prepared.clone().unwrap_or_default();
                render_rows(
                    rows,
                    hovered_region,
                    self.prepared_widest,
                    split,
                    split_h_offset,
                    &self.scroll_handle,
                    &rctx,
                    weak,
                )
                .into_any_element()
            }
        };
        // The body owns its own scrolling now: `Ready`/`CommitReady` return
        // a `uniform_list` (flex_1 + min_h(0)) that virtualizes rows and
        // reports an exact content height, so scrolling reaches the true
        // end of the diff. Non-Ready states return a centered placeholder.
        let scroll_body = body;
        // A one-row toolbar above the body carries the inline ↔ side-by-side
        // toggle (only when a diff is actually shown). The button label flips
        // to name the OTHER mode, like the reference editors.
        let has_body = matches!(
            self.state,
            DiffViewState::Ready { .. }
                | DiffViewState::CommitReady { .. }
                | DiffViewState::RangeReady { .. }
                | DiffViewState::CombinedReady { .. }
        );
        // The file rail is a multi-file affordance only — a single-file diff
        // has nothing to navigate, so the toggle and rail never appear.
        let multi_file = has_body && self.headers.len() > 1;
        let rail_open = self.rail_open;
        let toolbar = has_body.then(|| {
            let label = if split { "Inline" } else { "Side by side" };
            let hover_bg = self.theme.bg_panel_alt;
            let all_folded = self.all_folded();
            let fold_label = if all_folded { "Expand all" } else { "Collapse all" };
            // Review-notes cluster — a count label + Send / Copy / Clear,
            // shown only once the current scope carries at least one note.
            // Send pipes the markdown prompt to the active agent; Copy is the
            // clipboard escape hatch; Clear drops the scope's notes.
            let notes_cluster = (!self.notes.is_empty()).then(|| {
                let n = self.notes.len();
                let t_sm = self.typography.t_body_sm;
                let r_chip = self.density.r_chip;
                let fg_muted = self.theme.fg_muted;
                let action_chip = move |id: &'static str, text: &'static str| {
                    div()
                        .id(id)
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(r_chip))
                        .text_size(px(t_sm))
                        .text_color(fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .child(text)
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(t_sm))
                            .text_color(self.theme.fg_subtle)
                            .child(format!("Notes ({n})")),
                    )
                    .child(action_chip("diff-notes-send", "Send").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, window, cx| {
                            view.send_notes(window, cx);
                        },
                    )))
                    .child(action_chip("diff-notes-copy", "Copy").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, _window, cx| {
                            view.copy_notes(cx);
                        },
                    )))
                    .child(action_chip("diff-notes-clear", "Clear").on_click(cx.listener(
                        |view, _: &gpui::ClickEvent, _window, cx| {
                            view.clear_notes(cx);
                        },
                    )))
            });
            // Leading rail toggle (multi-file only), pushed apart from the
            // view controls by a flex spacer so it sits at the left edge.
            let rail_toggle = multi_file.then(|| {
                let active_bg = self.theme.bg_panel_alt;
                let mut btn = div()
                    .id("diff-rail-toggle")
                    .flex()
                    .items_center()
                    .px(px(6.0))
                    .py(px(1.0))
                    .rounded(px(self.density.r_chip))
                    .cursor_pointer()
                    .hover(move |s| s.bg(active_bg))
                    .on_click(cx.listener(|view, _: &gpui::ClickEvent, _window, cx| {
                        view.toggle_rail(cx);
                    }))
                    .child(
                        Icon::default()
                            .path("icons/list-tree.svg")
                            .xsmall()
                            .text_color(if rail_open {
                                self.theme.fg_base
                            } else {
                                self.theme.fg_subtle
                            }),
                    );
                if rail_open {
                    btn = btn.bg(active_bg);
                }
                btn
            });
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(self.density.gap_inline))
                .flex_shrink_0()
                .h(px(self.density.h_row))
                .w_full()
                .px(px(self.density.pad_panel))
                .bg(self.theme.bg_panel)
                .border_b_1()
                .border_color(self.theme.border_inactive)
                .children(rail_toggle)
                // Changed-file count, next to the rail toggle (multi-file only).
                .children(multi_file.then(|| {
                    let n = self.headers.len();
                    div()
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .child(format!("{n} changed files"))
                }))
                // Spacer pushes the view controls to the right edge while the
                // rail toggle stays pinned left.
                .child(div().flex_1())
                .children(notes_cluster)
                .child(
                    // Collapse-all / Expand-all — folds every file to its
                    // header so a many-file diff reads as a tight index.
                    div()
                        .id("diff-collapse-all")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(4.0))
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(self.density.r_chip))
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_click(cx.listener(move |view, _: &gpui::ClickEvent, _window, cx| {
                            view.set_all_folded(!all_folded, cx);
                        }))
                        .child(
                            Icon::default()
                                .path("icons/list-collapse.svg")
                                .xsmall()
                                .text_color(self.theme.fg_subtle),
                        )
                        .child(fold_label),
                )
                .child(
                    div()
                        .id("diff-split-toggle")
                        .px(px(8.0))
                        .py(px(1.0))
                        .rounded(px(self.density.r_chip))
                        .text_size(px(self.typography.t_body_sm))
                        .text_color(self.theme.fg_muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_click(cx.listener(|view, _: &gpui::ClickEvent, _window, cx| {
                            view.toggle_split(cx);
                        }))
                        .child(label),
                )
        });
        // The opt-in rail, built only when revealed for a multi-file diff.
        // Active file = the one owning the row at the top of the viewport
        // (same exact pixel-offset math the sticky header uses). Group tags
        // (section headers) come from the combined view; commit / range
        // diffs render a flat list.
        // Lazily build the rail's filter input the first time the rail is
        // shown (it needs a `Window`, unavailable in `new`). A change event
        // re-renders so the filtered row set updates as the user types.
        if multi_file && rail_open && self.rail_filter.is_none() {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder("Filter files…"));
            self._rail_filter_sub = Some(cx.subscribe(
                &input,
                |_view, _input, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
            self.rail_filter = Some(input);
        }
        let rail = (multi_file && rail_open).then(|| {
            let active_file_idx = self
                .row_owner
                .get(self.first_visible_row())
                .copied()
                .unwrap_or(0);
            // Borrow the group tags directly from state — no per-frame clone.
            let rail_groups: Option<&[FileGroup]> = match &self.state {
                DiffViewState::CombinedReady { groups, .. } => Some(groups.as_slice()),
                _ => None,
            };
            let filter = self
                .rail_filter
                .as_ref()
                .map(|i| i.read(cx).value().to_lowercase())
                .unwrap_or_default();
            let weak_rail = cx.entity().downgrade();
            let rows = file_rail(
                RailContext {
                    headers: &self.headers,
                    groups: rail_groups,
                    active_file_idx,
                    collapsed_dirs: &self.rail_collapsed_dirs,
                    filter: &filter,
                    theme: self.theme,
                    density: self.density,
                },
                &self.typography,
                weak_rail,
            );
            // Rail = fixed-width column: filter box pinned on top, file tree
            // scrolling beneath it.
            let mut col = div()
                .flex_shrink_0()
                .w(px(RAIL_WIDTH))
                .h_full()
                .flex()
                .flex_col()
                .bg(self.theme.bg_panel)
                .border_r_1()
                .border_color(self.theme.border_inactive);
            if let Some(input) = self.rail_filter.as_ref() {
                col = col.child(
                    div()
                        .flex_shrink_0()
                        .w_full()
                        .p(px(6.0))
                        .border_b_1()
                        .border_color(self.theme.border_inactive)
                        .child(Input::new(input).small()),
                );
            }
            col.child(
                div()
                    .id("diff-rail-scroll")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(rows),
            )
        });
        // The body lives in a `flex_1 + min_h(0)` wrapper so the toolbar can
        // sit above it while the `uniform_list` inside still gets a DEFINITE
        // height (`h_full` of this wrapper) — preserving scroll-to-end.
        // Clearing hover on body-leave: the body rows ignore hover-leave (so
        // the pointer can travel onto the floating staging card), which means
        // nothing disarms `hovered_region` when the pointer exits the diff
        // entirely. This wrapper-level leave catches that, hiding a stranded
        // card. Re-entering a changed line (or the card) re-arms it.
        let weak_leave = cx.entity().downgrade();
        let mut body_wrap = div()
            .id("diff-body-wrap")
            // `relative` makes this the positioning context for the overview
            // ruler stacked on its right edge (added below).
            .relative()
            // `flex_1` (grow 1, basis 0) fills the width the rail leaves in
            // the content row; height stretches via the row's cross axis. No
            // `w_full` here — on the horizontal main axis it would fight the
            // rail's fixed width.
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .on_hover(move |hovered, _window, cx: &mut App| {
                if !*hovered {
                    let _ = weak_leave.update(cx, |view, cx| view.set_hover(None, None, cx));
                }
            })
            .child(scroll_body);
        // In split mode, a horizontal-intent wheel (trackpad swipe or
        // Shift+wheel) scrolls BOTH columns' content in sync. We never stop
        // propagation, so the list still handles vertical scroll from the
        // same event; we only act on the horizontal component.
        if split {
            let char_w = self.typography.t_body_sm * 0.6;
            let max_off = (self.prepared_widest_chars as f32 * char_w - 80.0).max(0.0);
            body_wrap = body_wrap.on_scroll_wheel(cx.listener(
                move |view, ev: &gpui::ScrollWheelEvent, _window, cx| {
                    if !view.split {
                        return;
                    }
                    let (dx, dy) = match ev.delta {
                        gpui::ScrollDelta::Pixels(p) => (f32::from(p.x), f32::from(p.y)),
                        gpui::ScrollDelta::Lines(l) => (l.x * char_w, l.y * char_w),
                    };
                    // Shift makes a vertical wheel scroll horizontally; else
                    // honor a horizontal-dominant trackpad delta.
                    let amount = if ev.modifiers.shift {
                        dy
                    } else if dx.abs() > dy.abs() {
                        dx
                    } else {
                        0.0
                    };
                    if amount != 0.0 {
                        let next = (view.split_h_offset - amount).clamp(0.0, max_off);
                        if next != view.split_h_offset {
                            view.split_h_offset = next;
                            cx.notify();
                        }
                    }
                },
            ));
        }
        // Sticky file header: a pinned twin of the first-visible file's
        // header so its name + stats stay on screen while the body scrolls
        // under it. Stacked above the rows but below the ruler. Gains a
        // shadow only when a real header has scrolled past the top.
        if has_body && !self.headers.is_empty() {
            let fv = self.first_visible_row();
            let file_idx = self.row_owner.get(fv).copied().unwrap_or(0);
            if let Some(header) = self.headers.iter().find(|h| h.file_idx == file_idx) {
                // No shadow while a file's own header is flush at the top —
                // the overlay then overlaps that row seamlessly.
                let stuck = !matches!(
                    self.prepared.as_ref().and_then(|r| r.get(fv)),
                    Some(PreparedRow::FileHeader { .. })
                );
                let weak_sticky = cx.entity().downgrade();
                body_wrap = body_wrap.child(sticky_header_overlay(
                    header,
                    stuck,
                    self.theme,
                    self.density,
                    &self.typography,
                    weak_sticky,
                ));
            }
        }
        // Overview ruler: a thin change-map on the body's right edge (only
        // when a diff with changes is shown). Stacked last so it paints over
        // the scrollable body; the body owns scrolling underneath it. Each
        // run is click-to-jump (maps its start fraction back to a row).
        if has_body && !self.overview.is_empty() {
            let total_rows = self.prepared.as_ref().map(|r| r.len()).unwrap_or(0);
            let weak_ruler = cx.entity().downgrade();
            body_wrap =
                body_wrap.child(overview_ruler(&self.overview, total_rows, &self.theme, weak_ruler));
        }
        // Floating Stage/Discard card for the hovered region — pinned to the
        // viewport's right edge at the anchor row's on-screen Y (so it's
        // always visible regardless of the changed line's length or the
        // horizontal scroll). Gated on the hovered region's per-file side:
        // single-file `Ready` and combined `Unstaged`/`Staged` files stage;
        // commit/range views and `Committed`/`Untracked`-resolved sides that
        // return `None` show no card.
        if has_body
            && let Some(region) = hovered_region
            && let Some(row) = self.hovered_row
            && let Some(side) = self.side_for_region(region.0)
        {
            let h = self.density.h_row;
            let offset_y = f32::from(self.scroll_handle.0.borrow().base_handle.offset().y);
            // The hovered row's on-screen Y — the card floats right at the
            // pointer, not at the (possibly far-above) region anchor. Clamp
            // ≥ 0 defensively; the hovered row is by definition on screen.
            let top = (row as f32 * h + offset_y).max(0.0);
            let weak_card = cx.entity().downgrade();
            if let Some(card) = staging_card_overlay(
                top,
                region.0,
                region.1,
                row,
                side,
                self.theme,
                self.density,
                &self.typography,
                weak_card,
            ) {
                body_wrap = body_wrap.child(card);
            }
        }
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
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.0))
                .child(dialog)
        });
        // Review-note compose/edit popover — same centered-overlay shape as
        // the confirm dialog, scoped to this diff tab.
        let note_overlay = self.note_popover.clone().map(|popover| {
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.0))
                .child(popover)
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
            .border_color(self.theme.border_inactive);
        if let Some(tb) = toolbar {
            root = root.child(tb);
        }
        // Below the toolbar: an opt-in left rail beside the body. The content
        // row owns the remaining height; the body keeps `flex_1` so it fills
        // whatever the rail leaves. Without a rail it's a thin pass-through.
        let mut content = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .w_full();
        if let Some(rail) = rail {
            content = content.child(rail);
        }
        content = content.child(body_wrap);
        root = root.child(content);
        if let Some(o) = overlay {
            root = root.child(o);
        }
        if let Some(o) = note_overlay {
            root = root.child(o);
        }
        root.into_any_element()
    }
}

fn loading_state(path: &str, rctx: &RenderCtx<'_>) -> impl IntoElement {
    // Minimal centered text — no icon, no animation. Matches the muted
    // single-line placeholder the rest of the loading surfaces use.
    div()
        .flex()
        .items_center()
        .justify_center()
        .h_full()
        .w_full()
        .p(px(rctx.density.pad_panel))
        .text_size(px(rctx.typography.t_body_sm))
        .text_color(rctx.theme.fg_subtle)
        .child(format!("Loading diff for {path}…"))
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
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .text_color(rctx.theme.status_error)
                .child(
                    Icon::default()
                        .path("icons/alert-triangle.svg")
                        .xsmall()
                        .text_color(rctx.theme.status_error),
                )
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
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
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
                .child(
                    Icon::default()
                        .path("icons/refresh-cw.svg")
                        .xsmall()
                        .text_color(rctx.theme.status_info),
                )
                .child("Retry"),
        )
}

#[cfg(test)]
mod diff_ref_tests {
    use super::*;
    use oximux_core::CombinedDiffScope;

    #[test]
    fn non_ready_states_have_no_diff_ref() {
        assert_eq!(diff_ref_for(&DiffViewState::Empty), None);
        assert_eq!(
            diff_ref_for(&DiffViewState::CombinedLoading {
                scope: CombinedDiffScope::AllChanges
            }),
            None
        );
    }

    #[test]
    fn worktree_ready_keys_by_staged_side() {
        let unstaged = DiffViewState::Ready {
            path: PathBuf::from("a.rs"),
            staged: false,
            untracked: false,
            diffs: vec![],
            expanded: false,
        };
        assert_eq!(diff_ref_for(&unstaged).as_deref(), Some("worktree:unstaged"));
        let staged = DiffViewState::Ready {
            path: PathBuf::from("a.rs"),
            staged: true,
            untracked: false,
            diffs: vec![],
            expanded: false,
        };
        assert_eq!(diff_ref_for(&staged).as_deref(), Some("worktree:staged"));
    }

    #[test]
    fn untracked_folds_into_unstaged() {
        let untracked = DiffViewState::Ready {
            path: PathBuf::from("new.rs"),
            staged: false,
            untracked: true,
            diffs: vec![],
            expanded: false,
        };
        assert_eq!(diff_ref_for(&untracked).as_deref(), Some("worktree:unstaged"));
    }

    #[test]
    fn commit_range_combined_keys() {
        let commit = DiffViewState::CommitReady {
            sha: "abc123".into(),
            short_oid: "abc1".into(),
            subject: "msg".into(),
            diffs: vec![],
            expanded: false,
        };
        assert_eq!(diff_ref_for(&commit).as_deref(), Some("commit:abc123"));

        let range = DiffViewState::RangeReady {
            base: "main".into(),
            head: "feat".into(),
            path: PathBuf::from("a.rs"),
            title: "a.rs".into(),
            diffs: vec![],
            expanded: false,
        };
        assert_eq!(diff_ref_for(&range).as_deref(), Some("range:main..feat"));

        let combined = DiffViewState::CombinedReady {
            scope: CombinedDiffScope::Staged,
            diffs: vec![],
            groups: vec![],
            expanded: false,
        };
        assert_eq!(
            diff_ref_for(&combined).as_deref(),
            Some("combined:Staged Changes")
        );
    }
}

