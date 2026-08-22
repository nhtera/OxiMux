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
pub mod image_diff;
pub mod live_refresh;
pub mod note_repo_handle;
pub mod paint;
pub mod render;
pub mod review_note_popover;
pub mod review_notes;
pub mod syntax;
pub mod word_diff;

use crate::actions::{ExpandDiff, RetryDiff, SendTextToActiveAgent};
use crate::shell::confirm_dialog::{ConfirmCallback, ConfirmDialog, ConfirmPrompt};
use crate::shell::diff_view::file_header::StickyHeader;
use crate::shell::diff_view::live_refresh::{LiveQuery, LiveResult};
use crate::shell::diff_view::note_repo_handle::note_repo;
use crate::shell::diff_view::paint::{FoldId, OverviewRun, PreparedRow};
use crate::shell::diff_view::render::{FilePlan, Highlight, build_render_plan};
use crate::shell::diff_view::review_note_popover::{
    ReviewNoteCallback, ReviewNoteOutcome, ReviewNotePopover,
};
use crate::shell::diff_view::review_notes::{
    LineIndex, Note, NoteAnchor, ReviewNoteStore, format_notes_markdown,
};
use gpui::{
    App, AppContext, ClipboardItem, Context, Entity, FocusHandle, Focusable, ListAlignment,
    ListOffset, ListState, Subscription, Task, WeakEntity, Window, px,
};
use gpui_component::input::InputState;
use oximux_core::{CombinedDiffScope, FileDiff, FileGroup, NoteSide};
use oximux_editor::{EditorZoom, EditorZoomIn, EditorZoomOut, EditorZoomReset};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use oximux_storage::DiffReviewNoteRepo;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::oneshot;

/// How often an open working-tree diff re-asks git what it is showing.
///
/// Matched to the rail's `DIFF_REFRESH_TICK` on purpose: both spend one git
/// child process per tick, and there is no reason for a diff the user is
/// reading to lag behind the counts in the rail beside it. A tick that finds
/// nothing changed costs the process and stops — no invalidation, no repaint.
const LIVE_REFRESH_TICK: std::time::Duration = std::time::Duration::from_millis(2000);

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

/// Cached output of the expensive plan pass: `build_render_plan` (syntect
/// highlighting + word-diff pairing for every line) plus the per-file scans
/// that ride alongside it. Every field here is a pure function of the diff
/// data and the large-file `expanded` flag — folding a file, collapsing all,
/// flipping side-by-side, or toggling a review note never changes any of it.
/// Caching it lets those toggles re-flatten (cheap `prepare`) without paying
/// the syntect cost again, which is what made Collapse/Expand-all lag.
pub(crate) struct PlanCache {
    /// Per-file tokenised render plan — the syntect/word-diff output.
    plan: Vec<FilePlan>,
    /// Stageable change regions per file (git add -p granularity).
    regions: Vec<Vec<oximux_core::ChangeRegion>>,
    /// Per-file staged flag (combined view only; empty elsewhere → solid).
    staged_per_file: Vec<bool>,
    /// File paths in file order, for `mark_notes`.
    paths: Vec<String>,
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
    /// The heartbeat that keeps a working-tree diff showing the working tree.
    /// Held for the view's lifetime; dropping it stops the loop.
    _live_refresh_task: Option<Task<()>>,
    /// The current background fetch. Its own slot rather than sharing
    /// `_load_task`: a refresh must never cancel a load the user is waiting
    /// on, and a load must be free to cancel a refresh nobody asked for.
    _live_fetch_task: Option<Task<()>>,
    /// In-flight background refresh. Distinct from `_load_task`, which stays
    /// `Some` long after its load finished — this is the actual "one at a
    /// time" guard, so a slow git call cannot stack a queue of refreshes
    /// behind it.
    live_refresh_in_flight: bool,
    /// Whether this view's window is focused. Starts `true` so a view that
    /// has not rendered yet still refreshes: being wrong about the cost for
    /// one tick is cheaper than being wrong about the content indefinitely.
    window_active: bool,
    /// Window-activation observer, installed on first render because that is
    /// the first time this view is handed a `Window`. An observer rather than
    /// a per-render read: a view that is simply sitting there stops being
    /// rendered, and a focus gate that only updated during renders would
    /// freeze at whatever it last saw.
    _activation_sub: Option<Subscription>,
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
    /// Vertical scroll + per-item height cache for the variable-height diff
    /// body (`gpui::list`). Rows can differ in height (wrapped lines, image
    /// previews), so the list measures each item rather than assuming a
    /// uniform `h_row`. A prepared-row rebuild calls `reset(len)`; a font
    /// zoom (rows unchanged, heights changed) calls `remeasure()`.
    body_list: ListState,
    /// Body-zoom factor the `body_list` last measured at. When it changes the
    /// cached row heights are stale, so `remeasure()` runs before the next
    /// paint — the prepared rows are identical, so no `reset` (which would
    /// drop the scroll position).
    body_list_zoom: f32,
    /// Whether the `body_list` held a non-empty diff on the previous rebuild.
    /// A structural rebuild (row count changed) that follows a populated frame
    /// is an in-place edit (a fold expand/collapse) → keep the scroll position;
    /// one that follows an empty frame is a fresh diff (it passed through a
    /// Loading frame) → start at the top.
    body_list_was_populated: bool,
    /// Cached flattened render rows. Built once per (diff, expanded) change
    /// — NOT per frame — so syntax highlighting and word-diff pairing stay
    /// off the scroll path. `None` means "stale, rebuild on next render";
    /// every state transition that changes the body resets it.
    prepared: Option<Rc<Vec<PreparedRow>>>,
    /// Cached expensive plan (syntect + word-diff), shared across fold /
    /// collapse / split toggles. `None` → rebuild on next render. Only a
    /// fresh diff load or the large-file `expand` clears it (`invalidate_plan`);
    /// fold/collapse/split go through `invalidate_prepared`, which keeps this.
    plan_cache: Option<Rc<PlanCache>>,
    /// Monotonic counter bumped by `invalidate_plan` on every fresh diff /
    /// expand. The background highlight task captures it at spawn; a result
    /// whose generation no longer matches is stale (a newer diff superseded
    /// it) and is dropped instead of swapped in.
    plan_gen: u64,
    /// In-flight background syntax-highlight pass. Large highlightable diffs
    /// first paint uncolored (instant), then this task rebuilds the colored
    /// plan off the UI thread and swaps it in. Dropping it cancels (a new
    /// load replaces it); the `plan_gen` check guards already-running work.
    _highlight_task: Option<Task<()>>,
    /// Decoded image blobs for image-binary files in the current diff, keyed
    /// by the file's display path. Populated asynchronously after a load by
    /// `fetch_image_blobs`; the `prepare` flatten bakes the matching entry into
    /// each `PreparedRow::Image`. A path with no entry (still fetching / fetch
    /// failed) renders a "Loading…" placeholder. Cleared on every fresh load.
    images: HashMap<String, Rc<image_diff::ImageDiffData>>,
    /// In-flight image-blob fetch. Dropping aborts; replaced on every load.
    /// The `image_gen` check discards a result that a newer load superseded.
    _image_task: Option<Task<()>>,
    /// Monotonic counter bumped on each fresh image fetch. The async task
    /// captures it at spawn; a result whose generation no longer matches is
    /// stale (a newer load started) and is dropped instead of applied.
    image_gen: u64,
    /// File index whose path was just copied from its header — drives the
    /// transient copy → checkmark swap. Cleared after a short delay by
    /// `_copied_clear_task`. Pure feedback state; never invalidates `prepared`.
    recently_copied_file: Option<usize>,
    /// Reverts `recently_copied_file` to `None` a beat after a copy so the
    /// checkmark flashes briefly then returns to the copy glyph.
    _copied_clear_task: Option<Task<()>>,
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
    ///
    /// Every load reconciles the mirrored notes against the diff as it now
    /// reads, so a note whose line moved follows it and a note whose line is
    /// gone stops claiming one.
    notes: ReviewNoteStore,
    /// Active compose/edit popover (per-request; `None` when idle). Mounted
    /// INSIDE this DiffView's render tree like `confirm_dialog` so it scopes
    /// to the diff surface the user is reading.
    note_popover: Option<Entity<ReviewNotePopover>>,
    /// Per-mount observer on the active popover — clears the slot when the
    /// popover closes. Reset each time a new popover mounts.
    _note_popover_observer: Option<Subscription>,
    /// Weak handle to the hosting pane group, set after construction. Lets a
    /// diff file-header's "open in editor" affordance open the diffed file as
    /// an editor tab. `None` in headless tests (no live host).
    opener: Option<WeakEntity<crate::shell::pane_group::PaneGroup>>,
    /// Re-render when the editor-global font zoom changes from anywhere (an
    /// editor tab, or this diff's own Cmd+/-). The diff body shares the
    /// editor's zoom level, so a change elsewhere must repaint this view too.
    _zoom_sub: Subscription,
}

impl DiffView {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        // Editor-global font zoom changes from any editor (or this diff's own
        // Cmd+/-) must repaint the diff body so its code lines track the same
        // size.
        let _zoom_sub = cx.observe_global::<EditorZoom>(|_view, cx| cx.notify());
        // The heartbeat. Ticks for the view's whole life; `tick_live_refresh`
        // decides on each one whether there is anything worth asking, so a
        // commit view or an unfocused window costs a wakeup and nothing else.
        let _live_refresh_task = cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(LIVE_REFRESH_TICK).await;
                if weak
                    .update(cx, |view, cx| view.tick_live_refresh(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            repo,
            state: DiffViewState::Empty,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _load_task: None,
            _op_task: None,
            _live_refresh_task: Some(_live_refresh_task),
            _live_fetch_task: None,
            live_refresh_in_flight: false,
            window_active: true,
            _activation_sub: None,
            confirm_dialog: None,
            _confirm_dialog_observer: None,
            body_list: ListState::new(0, ListAlignment::Top, px(400.0)),
            body_list_zoom: 1.0,
            body_list_was_populated: false,
            prepared: None,
            plan_cache: None,
            plan_gen: 0,
            _highlight_task: None,
            images: HashMap::new(),
            _image_task: None,
            image_gen: 0,
            recently_copied_file: None,
            _copied_clear_task: None,
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
            opener: None,
            _zoom_sub,
        }
    }

    /// Give this diff view a weak handle to its hosting pane group so a
    /// file-header "open in editor" click can open the diffed file as an
    /// editor tab. Mirrors `TerminalView::set_opener`.
    pub fn set_opener(&mut self, opener: WeakEntity<crate::shell::pane_group::PaneGroup>) {
        self.opener = Some(opener);
    }

    /// Open the diffed file `rel_path` (workdir-relative, as git reports it)
    /// as an editor tab in the hosting pane group. No-op without an opener.
    pub fn open_file_in_editor(&self, rel_path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(opener) = self.opener.clone() else {
            return;
        };
        let abs = self.repo.workdir().join(rel_path);
        // Guard the no-such-file cases: a deleted-file header carries the old
        // path, and a commit/branch diff's path may not exist in the current
        // working tree (renamed/removed since). Opening the live file is right
        // when it exists; otherwise silently no-op rather than spawn a blank
        // editor for a path that isn't there.
        if !std::fs::metadata(&abs).map(|m| m.is_file()).unwrap_or(false) {
            tracing::debug!(path = %abs.display(), "diff: open-in-editor skipped; file not present in working tree");
            return;
        }
        let _ = opener.update(cx, |group, cx| {
            group.open_or_activate_editor_tab(abs, window, cx);
        });
    }

    /// Flash the copy → checkmark confirmation on `file_idx`'s header after its
    /// path is copied, then revert after a short beat. Pure feedback — the row
    /// set is unchanged, so it only re-renders (never invalidates `prepared`).
    fn flash_copied(&mut self, file_idx: usize, cx: &mut Context<Self>) {
        self.recently_copied_file = Some(file_idx);
        cx.notify();
        self._copied_clear_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1400))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.recently_copied_file == Some(file_idx) {
                    view.recently_copied_file = None;
                    cx.notify();
                }
            });
        }));
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

    /// First-visible row index — the list's scroll-top item. `list()` owns
    /// the scroll position in item space, so the sticky header + group-tag
    /// overlay read it directly (variable row heights mean there's no
    /// `offset / h_row` shortcut). `0` before the first layout.
    fn first_visible_row(&self, _cx: &App) -> usize {
        self.body_list.logical_scroll_top().item_ix
    }

    /// Scroll the body so `row` sits at the top of the viewport. Used by the
    /// overview-ruler + file-rail click-to-jump. `list()` resolves the pixel
    /// offset from each item's measured height, so it stays correct across a
    /// font-zoom or wrap-toggle change without recomputation.
    pub fn scroll_to_row(&self, row: usize) {
        self.body_list.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: px(0.0),
        });
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
        // NOTE: deliberately keeps `plan_cache`. Fold / collapse-all / split /
        // note toggles route here, and none of them change the tokenised plan
        // (only which rows the flatten emits). Anything that DOES change the
        // diff data or the large-file `expand` flag calls `invalidate_plan`.
        // Region indices + row indices are only meaningful against the
        // current row set; a rebuild may renumber them, so drop any stale
        // hover (region for slivers + row for the card position).
        self.hovered_region = None;
        self.hovered_row = None;
        // A new body means a new max line length — reset the split scroll so
        // we don't start mid-line on a different file.
        self.split_h_offset = 0.0;
    }

    /// Invalidate BOTH the tokenised plan and the flattened rows. Use only
    /// when the underlying diff data changes (a fresh load / seed / post-stage
    /// reload) or the large-file `expand` flag flips — those are the sole
    /// inputs to `build_render_plan`. Fold / collapse / split must NOT call
    /// this (it would re-run syntect for nothing); they call
    /// `invalidate_prepared` and reuse the cached plan.
    fn invalidate_plan(&mut self) {
        self.plan_cache = None;
        // Supersede any in-flight background highlight: bump the generation so
        // a result that's already computing is discarded on arrival, and drop
        // the task handle so a not-yet-started one is cancelled outright.
        self.plan_gen = self.plan_gen.wrapping_add(1);
        self._highlight_task = None;
        self.invalidate_prepared();
    }

    /// The diffs of whichever `*Ready` state is active, or `&[]` otherwise.
    /// Lets the image fetch enumerate files without re-matching every state.
    fn current_diffs(&self) -> &[FileDiff] {
        match &self.state {
            DiffViewState::Ready { diffs, .. }
            | DiffViewState::CommitReady { diffs, .. }
            | DiffViewState::RangeReady { diffs, .. }
            | DiffViewState::CombinedReady { diffs, .. } => diffs,
            _ => &[],
        }
    }

    /// Kick off the async fetch of image-binary previews for the current diff.
    /// Each image file's "before" side comes from the `HEAD` blob and its
    /// "after" side from the working-tree file on disk — the working-tree-vs-
    /// HEAD pairing the SCM panel shows. Added/untracked files have no HEAD
    /// blob (no `old`); deleted files have no working-tree file (no `new`).
    /// On completion the decoded blobs land in `self.images` and a cheap
    /// re-flatten (`invalidate_prepared`) bakes them into the image rows.
    ///
    /// Called after every successful load; a no-op when the diff carries no
    /// image binaries. `image_gen` guards against a stale result from a load
    /// the user has already navigated away from.
    fn fetch_image_blobs(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .current_diffs()
            .iter()
            .filter(|d| {
                matches!(d.status, oximux_core::DiffStatus::Binary)
                    && image_diff::is_image_path(d.path.as_path())
            })
            .map(|d| d.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        self.image_gen = self.image_gen.wrapping_add(1);
        let gen_id = self.image_gen;
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Vec<(String, image_diff::ImageDiffData)>>();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                target: "oximux_app::diff_view",
                "no tokio runtime entered; image preview fetch skipped"
            );
            return;
        };
        handle.spawn(async move {
            let mut out: Vec<(String, image_diff::ImageDiffData)> = Vec::with_capacity(paths.len());
            for p in paths {
                let Some(format) = image_diff::gpui_format_for(p.as_path()) else {
                    continue;
                };
                // After = working-tree file (absent for a deletion).
                let abs = repo.workdir().join(&p);
                let new_side = tokio::fs::read(&abs)
                    .await
                    .ok()
                    .filter(|b| !b.is_empty())
                    .map(|b| image_diff::ImageSide::from_bytes(format, b));
                // Before = HEAD blob (absent for an addition / untracked file).
                let old_side = repo
                    .read_blob_at("HEAD", p.as_path())
                    .await
                    .ok()
                    .flatten()
                    .filter(|b| !b.is_empty())
                    .map(|b| image_diff::ImageSide::from_bytes(format, b));
                if old_side.is_some() || new_side.is_some() {
                    out.push((
                        p.display().to_string(),
                        image_diff::ImageDiffData {
                            old: old_side,
                            new: new_side,
                        },
                    ));
                }
            }
            let _ = tx.send(out);
        });
        let task = cx.spawn(async move |this, cx| {
            let Ok(results) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                if view.image_gen != gen_id {
                    return; // a newer load superseded this fetch
                }
                if results.is_empty() {
                    return;
                }
                for (key, data) in results {
                    view.images.insert(key, Rc::new(data));
                }
                // Plan is unchanged (image-ness is already in it) — only the
                // baked-in pixels are new, so re-flatten without re-highlight.
                view.invalidate_prepared();
                cx.notify();
            });
        });
        self._image_task = Some(task);
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
        self.invalidate_plan();
        // A fresh selection always opens fully expanded (no folded files, no
        // pre-expanded context runs from the prior file).
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        // Drop the prior file's image previews so a fast switch never flashes
        // a stale picture; `fetch_image_blobs` repopulates after the load.
        self.images.clear();
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
                view.fetch_image_blobs(cx);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    /// Install the window-focus observer, on the first render that hands this
    /// view a `Window`.
    ///
    /// Called from `render`. Idempotent — the subscription is taken once and
    /// then keeps reporting on its own, which is the point: a view nobody is
    /// interacting with stops rendering, so anything that only updated during
    /// a render would be frozen at whatever it last happened to see.
    pub(crate) fn arm_live_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self._activation_sub.is_some() {
            return;
        }
        self.window_active = window.is_window_active();
        self._activation_sub = Some(cx.observe_window_activation(window, |view, window, cx| {
            let active = window.is_window_active();
            let regained = active && !view.window_active;
            view.window_active = active;
            // Coming back is the moment the answer is most likely to be stale
            // — the user was away, and away is when things change. Waiting out
            // a tick here would show them the old diff for up to two seconds
            // after they had already started reading it.
            if regained {
                view.refresh_content(cx);
            }
        }));
    }

    /// One beat of the heartbeat: refresh if there is anything to refresh.
    fn tick_live_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.window_active {
            return;
        }
        self.refresh_content(cx);
    }

    /// Re-ask git for what this view is showing, and adopt the answer only if
    /// it differs from what is on screen.
    ///
    /// Deliberately not `load()`. A load is a navigation: it resets folds,
    /// collapses expanded context, drops the scroll to the top and blanks the
    /// body to "Loading…" — all correct when a person picked a different file,
    /// all hostile when nobody asked for anything. This keeps the view exactly
    /// as the reader left it and swaps the content underneath.
    ///
    /// The comparison is the load-bearing part. Most ticks find the diff
    /// byte-identical, and an unconditional apply would re-tokenise the body,
    /// throw away the syntax-highlight cache and repaint a view nothing had
    /// changed about — several times a minute, forever.
    fn refresh_content(&mut self, cx: &mut Context<Self>) {
        if self.live_refresh_in_flight {
            return;
        }
        let Some(query) = LiveQuery::for_state(&self.state) else {
            return;
        };
        // A hunk op has its own reload chained behind it. Letting a refresh
        // land in the middle would show the pre-op diff again for a tick.
        if self._op_task.is_some() {
            return;
        }
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<LiveResult>();
        let query_for_fetch = query.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let fetched = match query_for_fetch {
                        LiveQuery::Single {
                            path,
                            staged,
                            untracked,
                        } => {
                            // Asked before the diff, and only for untracked
                            // files: git cannot describe a file that is not
                            // there, so without this its error is
                            // indistinguishable from a transient one and the
                            // view would keep showing a deleted file forever.
                            if untracked && !repo.workdir().join(&path).exists() {
                                let _ = tx.send(LiveResult::Gone);
                                return;
                            }
                            let r = if untracked {
                                repo.diff_for_untracked(&path).await
                            } else {
                                repo.diff_for_path(&path, staged).await
                            };
                            r.ok().map(LiveResult::Single)
                        }
                        LiveQuery::Combined { scope } => {
                            repo.diff_combined(scope).await.ok().map(LiveResult::Combined)
                        }
                    };
                    let _ = tx.send(fetched.unwrap_or(LiveResult::Unavailable));
                });
            }
            // No runtime: the same degradation every other fetch here takes.
            // Silent, because this one runs on a timer and a warning per tick
            // would bury the log.
            Err(_) => return,
        }
        self.live_refresh_in_flight = true;
        let task = cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = this.update(cx, |view, cx| {
                view.live_refresh_in_flight = false;
                let Ok(result) = result else {
                    return;
                };
                view.apply_live_result(query, result, cx);
            });
        });
        self._live_fetch_task = Some(task);
    }

    /// Adopt a background refresh, or decline it.
    ///
    /// Declines in three cases, all of them "this answer is not about what is
    /// on screen any more": the fetch failed, the view moved on to a different
    /// diff while the git call was out, or nothing changed.
    ///
    /// A failure is dropped rather than surfaced. `load()` turns an error into
    /// a `Failed` state because a person is sitting there waiting for a file
    /// they asked for; nobody asked for this one, and replacing a diff that is
    /// on screen and readable with an error banner — because a `git diff`
    /// happened to collide with an index lock — would break a working view to
    /// report a problem that is usually gone by the next tick.
    fn apply_live_result(&mut self, query: LiveQuery, result: LiveResult, cx: &mut Context<Self>) {
        // The view may have been navigated elsewhere while the fetch was out.
        if LiveQuery::for_state(&self.state).as_ref() != Some(&query) {
            return;
        }
        let anchor = self.first_visible_row(cx);
        match (result, &self.state) {
            (LiveResult::Single(diffs), DiffViewState::Ready { diffs: shown, .. })
                if &diffs == shown => {}
            (
                LiveResult::Single(diffs),
                DiffViewState::Ready {
                    path,
                    staged,
                    untracked,
                    ..
                },
            ) => {
                let (path, staged, untracked) = (path.clone(), *staged, *untracked);
                self.invalidate_plan();
                self.apply_load_result(path, staged, untracked, Ok(diffs));
                self.finish_live_refresh(anchor, cx);
            }
            (
                LiveResult::Combined(combined),
                DiffViewState::CombinedReady {
                    diffs: shown,
                    groups: shown_groups,
                    ..
                },
            ) if &combined.diffs == shown && &combined.groups == shown_groups => {}
            (LiveResult::Combined(combined), DiffViewState::CombinedReady { scope, .. }) => {
                let scope = scope.clone();
                self.invalidate_plan();
                self.apply_combined_result(scope, Ok(combined));
                self.finish_live_refresh(anchor, cx);
            }
            (
                LiveResult::Gone,
                DiffViewState::Ready {
                    path,
                    staged,
                    untracked,
                    ..
                },
            ) => {
                let (path, staged, untracked) = (path.clone(), *staged, *untracked);
                self.invalidate_plan();
                self.state = DiffViewState::Failed {
                    path,
                    staged,
                    untracked,
                    error: "This file is no longer in the working tree.".to_string(),
                };
                // No scroll anchor to keep — there is no body to scroll. The
                // notes for the scope are dropped by `reload_notes` alongside
                // the state, since a non-`Ready` state carries none.
                self.reload_notes();
                cx.notify();
            }
            // Fetch failed, or the state shape no longer matches the answer.
            _ => {}
        }
    }

    /// Shared tail of an adopted refresh: put the reader back where they were,
    /// then let everything that reads the diff catch up with it.
    ///
    /// The scroll anchor matters more here than on a load. A load follows a
    /// click, so landing at the top reads as the result of that click; this
    /// happens while someone is reading, and moving the page under them is the
    /// one thing a background refresh must never do.
    fn finish_live_refresh(&mut self, anchor: usize, cx: &mut Context<Self>) {
        self.pending_scroll_anchor = Some(anchor);
        // The content moved, so notes anchored to it may have moved with it —
        // exactly the drift `reload_notes` reconciles.
        self.reload_notes();
        self.fetch_image_blobs(cx);
        cx.notify();
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
            // A turn diff has no fetch to retry: its content came from the caller,
            // and `diff_combined` rejects the scope by design. Retrying would swap
            // the real parse error for a confusing internal one and could never
            // succeed, so the failure stands — reopen Review to try again.
            DiffViewState::CombinedFailed { scope: CombinedDiffScope::TurnDiff { .. }, .. } => {}
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
        self.invalidate_plan();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        self.images.clear();
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
                view.fetch_image_blobs(cx);
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
        self.invalidate_plan();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        self.images.clear();
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
                view.fetch_image_blobs(cx);
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
        self.invalidate_plan();
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        // Drop the prior scope's notes immediately. `reload_notes` repopulates
        // on the next `*Ready`; clearing here keeps `notes` empty during the
        // Loading window so nothing reads stale anchors.
        self.notes.clear();
        self.images.clear();
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
                view.fetch_image_blobs(cx);
                cx.notify();
            });
        });
        self._load_task = Some(task);
    }

    /// Render a diff the caller ALREADY HAS, instead of fetching one from the
    /// repo — an agent turn's accumulated diff, opened from the chat's turn-end
    /// card.
    ///
    /// This is the whole virtual path: parse with the same
    /// [`oximux_git::parse_unified_diff`] the fetch path uses, then hand the
    /// result to the same [`Self::apply_combined_result`] the fetch path ends at.
    /// No shellout, no task, no second viewer — the only difference from
    /// [`Self::load_combined`] is where the bytes came from.
    ///
    /// Every file is tagged [`FileGroup::Committed`] (read-only): a turn diff is
    /// a record of what happened, and the working tree may have moved since, so
    /// offering Stage/Discard against it would act on a file that no longer
    /// matches what is on screen. Read-only also means no hunk op, which means
    /// nothing can trigger the reload path back into git for a scope git cannot
    /// fetch.
    pub fn load_virtual(&mut self, scope: CombinedDiffScope, raw_diff: &str, cx: &mut Context<Self>) {
        // Same drop-on-entry + fresh-selection reset as `load_combined`.
        self._op_task = None;
        self._load_task = None;
        self.invalidate_plan();
        self.collapsed.clear();
        self.expanded_folds.clear();
        self.reset_rail_state();
        self.notes.clear();
        self.images.clear();
        let result = oximux_git::parse_unified_diff(raw_diff)
            .map(|diffs| {
                let groups = vec![FileGroup::Committed; diffs.len()];
                oximux_core::CombinedDiff { diffs, groups }
            })
            .map_err(|e| e.to_string());
        self.apply_combined_result(scope, result);
        self.reload_notes();
        self.fetch_image_blobs(cx);
        cx.notify();
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
        self.invalidate_plan();
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
        self.invalidate_plan();
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
        self.invalidate_plan();
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
            mode: None,
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

    /// Open the confirm modal for "Discard this hunk?". On confirm,
    /// runs `discard_hunks` and reloads. No-op when the view isn't
    /// Ready, the file is untracked, the staged side is on screen
    /// (discard is worktree-only — user must unstage first), or the
    /// index is out of range. First-open-wins: a re-fire while a
    /// dialog is already mounted is ignored so a rapid double-click
    /// doesn't swap the modal out from under the user's cursor.
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
            on_confirm,
            confirm_label: Some("Discard".into()),
            on_cancel: Some(on_cancel),
            secondary: None,
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
        let anchor = self.first_visible_row(cx);
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

    /// Cmd+/- font zoom while the diff is focused. Drives the same
    /// editor-global zoom the editor tabs use, so code size stays consistent
    /// across both surfaces; the global observer repaints. `set_global`
    /// inserts the global on first use, so no boot-time install is needed.
    fn on_zoom_in(&mut self, _: &EditorZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        let next = current_zoom(cx).zoomed_in();
        cx.set_global(next);
        cx.notify();
    }

    fn on_zoom_out(&mut self, _: &EditorZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        let next = current_zoom(cx).zoomed_out();
        cx.set_global(next);
        cx.notify();
    }

    fn on_zoom_reset(&mut self, _: &EditorZoomReset, _window: &mut Window, cx: &mut Context<Self>) {
        cx.set_global(EditorZoom::reset());
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
        self.reconcile_notes(&repo, &scope, &diff_ref);
        self.invalidate_prepared();
    }

    /// Re-decide each loaded note's anchor against the diff as it now reads,
    /// and write the conclusions back.
    ///
    /// This runs on load rather than on save because the drift happens while
    /// nobody is looking: the file is edited, a hunk is staged, the poller
    /// reloads. By the time the notes are read back their line numbers have
    /// already stopped meaning what they meant, and the load is the first
    /// moment there is both a note and a diff to compare it against.
    ///
    /// Moves are persisted so the work is done once. A detached note is left
    /// exactly as stored — its line is a record of where it was written, not
    /// a claim about where it is, and keeping it is what lets the note
    /// re-attach if the code comes back.
    fn reconcile_notes(&mut self, repo: &DiffReviewNoteRepo, scope: &str, diff_ref: &str) {
        // Checked before building anything: the context map walks a fresh
        // render plan over every file in the diff, and a diff nobody has
        // annotated — which is nearly all of them — must not pay for that on
        // every poll-driven reload.
        if self.notes.is_empty() {
            return;
        }
        let outcome = self.notes.reconcile(&self.note_context_map());
        if outcome.is_noop() {
            return;
        }
        let moves: Vec<(String, NoteSide, u32, u32)> = outcome
            .moves
            .iter()
            .map(|(anchor, to)| (anchor.path.clone(), anchor.side, anchor.line, *to))
            .collect();
        if let Err(err) = repo.reanchor(scope, diff_ref, &moves) {
            // The in-memory store has already followed the code, so the view
            // is right for this session; only the saved copy is stale, and the
            // next load reconciles it again from the same evidence.
            tracing::warn!(
                target: "oximux_app::diff_view",
                %err,
                "review-note re-anchor failed"
            );
        }
        tracing::debug!(
            target: "oximux_app::diff_view",
            moved = outcome.moves.len(),
            detached = outcome.newly_detached,
            reattached = outcome.reattached,
            "review notes reconciled against the current diff"
        );
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
                // Record the line as it reads at the moment of writing. This
                // is the only point where the note and its subject are known
                // to agree, and every later reconcile is measured from it.
                let anchor_text = self.anchor_text_at(&anchor).unwrap_or_default();
                self.persist_note(&anchor, Some(&body), &anchor_text);
                self.notes.set(anchor, Note::new(body, anchor_text));
                self.invalidate_prepared();
            }
            ReviewNoteOutcome::Delete => {
                self.persist_note(&anchor, None, "");
                self.notes.remove(&anchor);
                self.invalidate_prepared();
            }
            ReviewNoteOutcome::Cancel => {}
        }
        cx.notify();
    }

    /// The current text of the diff line an anchor names, if the diff still
    /// has that line.
    fn anchor_text_at(&self, anchor: &NoteAnchor) -> Option<String> {
        self.note_context_map().get(anchor).cloned()
    }

    /// Write one note through to SQLite — `Some(body)` upserts, `None`
    /// deletes. No-op without a diff_ref (non-`Ready` state) or repo handle.
    /// Errors are logged, not surfaced: the in-memory store still reflects the
    /// user's edit, and the next load reconciles against the DB.
    fn persist_note(&self, anchor: &NoteAnchor, body: Option<&str>, anchor_text: &str) {
        let Some(diff_ref) = diff_ref_for(&self.state) else {
            return;
        };
        let Some(repo) = note_repo() else {
            return;
        };
        let scope = self.scope_key();
        let res = match body {
            Some(b) => repo.upsert(
                &scope,
                &diff_ref,
                &anchor.path,
                anchor.side,
                anchor.line,
                b,
                anchor_text,
            ),
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
    fn note_context_map(&self) -> LineIndex {
        let mut map = LineIndex::new();
        let diffs = match &self.state {
            DiffViewState::Ready { diffs, .. }
            | DiffViewState::CommitReady { diffs, .. }
            | DiffViewState::RangeReady { diffs, .. }
            | DiffViewState::CombinedReady { diffs, .. } => diffs,
            _ => return map,
        };
        // Note-anchor mapping only reads line numbers — skip the syntect pass.
        for fp in build_render_plan(diffs, true, Highlight::Off) {
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

    /// Drop only the notes whose line is gone, in memory and in SQLite.
    ///
    /// The gutter is untouched by design — a detached note never had a marker
    /// — so this repaints the toolbar's count and nothing else.
    fn clear_detached_notes(&mut self, cx: &mut Context<Self>) {
        let dropped = self.notes.drain_detached();
        if dropped.is_empty() {
            return;
        }
        for anchor in &dropped {
            self.persist_note(anchor, None, "");
        }
        cx.notify();
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

/// The current editor-global font zoom (default when never set this session).
fn current_zoom(cx: &App) -> EditorZoom {
    cx.try_global::<EditorZoom>().copied().unwrap_or_default()
}

/// Row-height factor the diff body scales by to track the editor's font zoom:
/// the zoomed code-font size over the base `t_body_sm`. `1.0` when unzoomed (or
/// the base is degenerate). The body row height multiplies by this so the
/// fixed-height virtualized rows grow with the code and every body pixel
/// computation (sticky header, staging card, scroll anchor) stays aligned.
/// Typography sizes are zoomed per-field by [`scaled_typography`] (each tracks
/// the editor's absolute px delta), so only the row height uses this ratio —
/// anchored to `t_body_sm` because that's the code-line font the rows host.
fn body_zoom_factor(base_pt: f32, zoom: EditorZoom) -> f32 {
    if base_pt <= 0.0 {
        return 1.0;
    }
    f32::from(zoom.effective_px(px(base_pt))) / base_pt
}

/// Clone `base` with every font size zoomed by the editor's absolute px delta
/// — the same `effective_px` the editor applies to its mono font — so diff-body
/// text matches editor text at the same zoom level (rather than a single ratio
/// that would drift the larger sizes). Returns the base unchanged at the
/// default zoom. Font families + weights are untouched.
fn scaled_typography(base: &Typography, zoom: EditorZoom) -> Typography {
    if zoom == EditorZoom::default() {
        return base.clone();
    }
    let mut t = base.clone();
    t.t_sub_label = f32::from(zoom.effective_px(px(t.t_sub_label)));
    t.t_label_xs = f32::from(zoom.effective_px(px(t.t_label_xs)));
    t.t_label_caps = f32::from(zoom.effective_px(px(t.t_label_caps)));
    t.t_body_sm = f32::from(zoom.effective_px(px(t.t_body_sm)));
    t.t_brand = f32::from(zoom.effective_px(px(t.t_brand)));
    t.t_body_md = f32::from(zoom.effective_px(px(t.t_body_md)));
    t.t_body_lg = f32::from(zoom.effective_px(px(t.t_body_lg)));
    t
}

impl Focusable for DiffView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
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
    fn scaled_typography_is_identity_at_default_zoom() {
        let base = Typography::default();
        let same = scaled_typography(&base, EditorZoom::default());
        assert_eq!(same.t_body_sm, base.t_body_sm);
        assert_eq!(same.t_label_caps, base.t_label_caps);
        assert_eq!(same.t_body_lg, base.t_body_lg);
    }

    #[test]
    fn scaled_typography_applies_zoom_delta_per_field() {
        let base = Typography::default();
        // Two zoom-in steps = +2px absolute delta per field (the same delta the
        // editor applies to its mono font), assuming each stays within the
        // [8,32] clamp — true for the cockpit defaults used here.
        let zoom = EditorZoom::default().zoomed_in().zoomed_in();
        let bigger = scaled_typography(&base, zoom);
        assert_eq!(bigger.t_body_sm, base.t_body_sm + 2.0);
        assert_eq!(bigger.t_label_caps, base.t_label_caps + 2.0);
        assert_eq!(bigger.t_sub_label, base.t_sub_label + 2.0);
        assert_eq!(bigger.t_body_lg, base.t_body_lg + 2.0);
        // Font families + weights are untouched — only sizes zoom.
        assert_eq!(bigger.family_mono, base.family_mono);
    }

    #[test]
    fn body_zoom_factor_tracks_code_font_ratio() {
        // h_row scales by the zoomed-over-base ratio of the code font.
        let base = 10.0_f32;
        assert_eq!(body_zoom_factor(base, EditorZoom::default()), 1.0);
        let inned = body_zoom_factor(base, EditorZoom::default().zoomed_in().zoomed_in());
        assert_eq!(inned, 12.0 / 10.0);
        // Degenerate base never divides by zero.
        assert_eq!(body_zoom_factor(0.0, EditorZoom::default().zoomed_in()), 1.0);
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

