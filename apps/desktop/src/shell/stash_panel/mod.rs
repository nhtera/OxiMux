//! StashPanel — list git stash entries with per-row Apply / Pop / Drop.
//!
//! Drop is destructive and irreversible in the UI, so the panel never fires
//! it: the row emits [`DropStashRequested`], the shell host mounts a
//! `ConfirmDialog`, and only its confirm callback reaches
//! [`StashPanel::drop_confirmed`]. An event, not a flag — a flag has no
//! subscriber, which is precisely why Drop shipped doing nothing.
//!
//! Apply and Pop fire directly. **Neither is undone by the reflog**: after a
//! pop the stash's reflog entry is gone and the only way back is the commit
//! sha, which is why every destructive path here logs one before it fires.
//!
//! Layout:
//!   - Always-rendered header: chevron + "STASHES (N)" + refresh + "+" push.
//!   - Body: list (or "No stashes" placeholder). Hidden when collapsed
//!     (default). Power-user surface; eats no visual real estate when
//!     unused.
//!
//! One row is `row.rs`; the strings it paints come from `list_render.rs`,
//! which stays free of GPUI so the formatting is unit-testable.
//!
//! Runtime: refresh + ops use `tokio::runtime::Handle::try_current` + the
//! same log+no-op fallback as DiffView / CommitDialog. Refresh is
//! single-flight via `_refresh_task: Option<Task<()>>` — dropping cancels.
//! Ops are detached instead (see `ops.rs`): cancelling a destructive op
//! mid-subprocess loses its result.

pub mod branch_dialog;
pub mod context_menu;
pub mod file_row;
mod form;
pub mod keyboard;
pub mod list_render;
pub mod ops;
pub mod push_dialog;
pub mod rename_dialog;
pub mod resize;
pub mod row;
pub mod tree_view;

use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Task,
    Window, div, px,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants},
    scroll::ScrollableElement as _,
};
use oximux_core::{StashEntry, StashFile, StashFileOrigin, StashRef, ViewMode};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum StashListState {
    Idle,
    Loading,
    Ready(Vec<StashEntry>),
    Failed(String),
}

/// One stash's file list, fetched on first expand.
///
/// `Loading` and `Failed` are states the row paints, not states it hides:
/// an expansion that renders nothing at all is indistinguishable from a
/// stash that touched no files, and a failure that renders nothing is
/// indistinguishable from success.
#[derive(Debug)]
pub enum StashFilesState {
    Loading,
    Ready(Vec<StashFile>),
    Failed(String),
}

/// Emitted when the user clicks a file inside an expanded stash. The host
/// opens a read-only diff tab for it.
///
/// `origin` is load-bearing and travels with the request rather than being
/// re-derived downstream: it selects the revision the file is read from, and
/// the two are not interchangeable — see [`StashFileOrigin`].
#[derive(Debug, Clone)]
pub struct ShowStashFileRequested {
    pub sha: String,
    pub path: PathBuf,
    pub origin: StashFileOrigin,
    /// What to call the stash in the tab title. Carried here because the
    /// host subscribes to the panel, not to the list, and a bare file name
    /// gives the user no way to tell two stashes' copies of one path apart.
    pub label: String,
}

/// Emitted when the user clicks the header `+` button. The host
/// (`SourceControlPanel` via `WorkspaceRoot`) subscribes and mounts a
/// `PushStashDialog`. Routed through an event rather than a direct
/// callback so the panel stays free of host-modal coupling.
#[derive(Debug, Clone, Copy)]
pub struct PushStashRequested;

/// Emitted when the user clicks a row's `Drop`. The host mounts a
/// `ConfirmDialog` and, on confirm, calls [`StashPanel::drop_confirmed`].
///
/// Carries everything the dialog copy needs, because an index tells the user
/// nothing about which stash is about to disappear.
#[derive(Debug, Clone)]
pub struct DropStashRequested {
    /// The `stash@{N}` address the row was PAINTED with — for display and for
    /// the context menu, never for the git call. The op re-resolves `sha` to a
    /// live address at fire time; see `ops.rs`.
    pub stash_ref: StashRef,
    /// The stash's immutable identity, and what the op actually acts on.
    pub sha: String,
    pub message: String,
    pub relative: String,
    pub branch: String,
}

/// Emitted when the user picks `Open All Changes` on a stash row. The host
/// opens one read-only tab holding every file the stash touches.
#[derive(Debug, Clone)]
pub struct ShowStashAllRequested {
    pub sha: String,
    /// What to call the stash in the tab title, resolved from the same list
    /// the row was painted from.
    pub label: String,
}

/// Emitted when the user picks `Branch from Stash…`. The host prompts for a
/// name and, on confirm, calls [`StashPanel::branch_from_stash`].
///
/// Carries the stash's message so the dialog can offer a default name; the
/// user is naming a branch after work they described once already.
#[derive(Debug, Clone)]
pub struct BranchFromStashRequested {
    pub sha: String,
    /// The `stash@{N}` the row was PAINTED with — a tiebreaker for the
    /// resolve, never a target. Same contract as [`DropStashRequested`].
    pub painted: usize,
    pub message: String,
}

/// Emitted when the user picks `Restore This File…` on a file inside a stash.
/// The host mounts a confirm dialog and, on confirm, calls
/// [`StashPanel::restore_file_confirmed`].
///
/// Only ever emitted for a [`StashFileOrigin::Tracked`] file — an untracked
/// one is not in the stash commit's tree and `git checkout <sha> -- <path>`
/// can only fail on it. The menu omits the item entirely for those rows; this
/// is the second gate, on the panel side, where the origin is actually known.
#[derive(Debug, Clone)]
pub struct RestoreStashFileRequested {
    pub sha: String,
    pub path: PathBuf,
}

/// Emitted when the user picks `Rename…` on a stash row. The host prompts for
/// a new message and, on confirm, calls [`StashPanel::rename_confirmed`].
///
/// Routed through the host like Drop and Branch, and for a sharper reason than
/// either: renaming `stash@{N}` drops and re-stores every entry above it, so
/// the dialog is where the user is told the operation is bigger than the row
/// they clicked.
#[derive(Debug, Clone)]
pub struct RenameStashRequested {
    pub sha: String,
    /// The `stash@{N}` the row was PAINTED with — a tiebreaker for the
    /// resolve, never a target. Same contract as [`DropStashRequested`].
    pub painted: usize,
    /// The message to pre-fill the field with, so a rename is an edit rather
    /// than a retype.
    pub message: String,
    /// How many entries sit above this one, which is how many the sequence
    /// will take off and put back. Shown in the dialog: a rename at the bottom
    /// of a deep stack is not the small operation the verb suggests.
    pub depth: usize,
}

pub struct StashPanel {
    repo: Repository,
    state: StashListState,
    /// Body visibility flag. Default `true` — the stash list is a
    /// power-user surface; keeping it collapsed by default avoids
    /// burning vertical real estate in the SCM tab for users who don't
    /// rely on git stash. The header (with `STASHES (N)`) is always
    /// rendered so the count is glanceable even when collapsed.
    collapsed: bool,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Scroll position for the capped stash body. Wired through
    /// `track_scroll` on the overflow region and consumed by
    /// `vertical_scrollbar` so the thumb tracks the user's wheel/drag.
    /// Mirrors `GitPanel::scroll_handle` (`git_panel/mod.rs:113`).
    scroll_handle: ScrollHandle,
    /// Serialises the resolve→fire window of every stash op.
    ///
    /// Ops are detached so they cannot cancel each other, which leaves them
    /// free to overlap: two confirms landing within one subprocess (~25 ms)
    /// both resolve against the pre-mutation stack, so the second fires on an
    /// index the first has already invalidated. `stash_drop`'s sha assertion
    /// catches that and `rollback` undoes it, but a rollback is a loud error
    /// and a reordered stack — the wrong outcome for a legitimate pair of
    /// clicks. Holding this across resolve-and-fire makes the race
    /// unreachable, and leaves the assertion as the last resort it was meant
    /// to be rather than the expected path.
    op_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    _refresh_task: Option<Task<()>>,
    /// Which stashes are showing their file list, keyed by **sha**.
    ///
    /// Not by `stash@{N}`: the stack is shared with every worktree and with
    /// the user's terminal, so an index-keyed expansion silently re-points at
    /// a different stash the moment anything below it is dropped. Survives a
    /// refresh — see [`StashPanel::adopt_list`] for how it is reconciled
    /// against the list that comes back.
    expanded: HashSet<String>,
    /// sha → its files. Invalidated wholesale by `refresh()`.
    files: HashMap<String, StashFilesState>,
    /// In-flight file fetches, **one slot per sha**.
    ///
    /// Not the single `Option<Task>` the refresh path uses. Dropping a task
    /// cancels it, so a shared slot means expanding a second stash within one
    /// subprocess (~50 ms) cancels the first one's completion handler — and
    /// because a sha that already has a cache entry is never refetched, the
    /// first row would read "Loading…" permanently, with collapsing and
    /// re-expanding unable to clear it.
    ///
    /// Bounded by the number of stashes, since inserting the same key
    /// replaces the old slot; `refresh()` empties it.
    fetch_tasks: HashMap<String, Task<()>>,
    /// Body height the user chose, in pixels — persisted, and what the drag
    /// handle and keyboard rail mutate. What gets PAINTED is
    /// [`StashPanel::painted_height`], which trims this against the budget
    /// the section currently shares with the graph.
    ///
    /// Replaced Phase 1's private eight-row cap. That cap was an admitted
    /// guess whose only job was to stop the section eating the changed-files
    /// list; a real handle does the same job and lets the user disagree.
    stash_height: gpui::Pixels,
    /// Global k/v store, so a chosen height survives a restart. `None` is
    /// test wiring — in-memory only.
    settings_repo: Option<oximux_storage::SettingsRepo>,
    /// `true` while a drag-resize is in flight, so the handle's highlight
    /// stays lit (hover styles are suppressed mid-drag).
    resizing: bool,
    /// `(height_at_drag_start, cursor_y_at_first_tick)`. The cursor origin is
    /// latched lazily because drag-start cannot read the pointer.
    drag_anchor: Option<(f32, Option<f32>)>,
    /// Tallest this section may currently be, pushed down each render by
    /// `SourceControlPanel` — the only place that can see both sections'
    /// heights. `None` before the first push.
    section_ceiling: Option<f32>,
    /// Where the keyboard cursor is, or `None` when nothing is selected.
    ///
    /// **Panel state, not focus state.** A dialog takes focus away from the
    /// panel — `Cmd+Backspace` opens one on purpose — and a cursor stored as
    /// focus would be gone by the time the user pressed Escape. It survives
    /// the round trip because nothing here is tied to a `FocusHandle`;
    /// `adopt_list` is the only thing that clears it, and only when the row it
    /// names has actually left the stack.
    cursor: Option<keyboard::StashCursor>,
    /// Flat or tree for the files inside an expanded stash.
    ///
    /// Persisted GLOBALLY — see
    /// [`KEY_SCM_STASH_VIEW_MODE`](crate::app_settings::scm_layout_settings::KEY_SCM_STASH_VIEW_MODE)
    /// for why a per-worktree key would be wrong for a stack every worktree
    /// shares.
    view_mode: ViewMode,
    /// Collapsed folders in tree mode, keyed by the stash they belong to.
    ///
    /// Per-sha, because two stashes can both contain `src/` and collapsing one
    /// must not collapse the other. Cleared wholesale by `adopt_list` along
    /// with the file cache the paths were derived from.
    collapsed_dirs: HashMap<String, HashSet<PathBuf>>,
    /// The resize rail's OWN focus handle.
    ///
    /// Not `focus_handle`, which the panel's root container already tracks:
    /// sharing one handle between two elements makes the rail read as focused
    /// whenever anything in the section is, and it paints its focus-ring
    /// accent permanently — a bright blue line under the stash list that
    /// looks like a selection. Found live; the graph avoids it by having no
    /// root `track_focus` at all.
    resize_focus: FocusHandle,
}

impl EventEmitter<PushStashRequested> for StashPanel {}
impl EventEmitter<DropStashRequested> for StashPanel {}
impl EventEmitter<ShowStashFileRequested> for StashPanel {}
impl EventEmitter<ShowStashAllRequested> for StashPanel {}
impl EventEmitter<BranchFromStashRequested> for StashPanel {}
impl EventEmitter<RestoreStashFileRequested> for StashPanel {}
impl EventEmitter<RenameStashRequested> for StashPanel {}

impl StashPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Repository,
        initial_height: gpui::Pixels,
        settings_repo: Option<oximux_storage::SettingsRepo>,
        theme: Theme,
        density: Density,
        typography: Typography,
        cx: &mut Context<Self>,
    ) -> Self {
        // Read before the struct is built so the first paint is already in the
        // user's layout — flipping after mount would show one frame of flat.
        let view_mode = settings_repo
            .as_ref()
            .map(crate::app_settings::scm_layout_settings::load_stash_view_mode)
            .unwrap_or_default();
        let mut panel = Self {
            repo,
            state: StashListState::Idle,
            collapsed: true,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            scroll_handle: ScrollHandle::new(),
            op_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            _refresh_task: None,
            expanded: HashSet::new(),
            files: HashMap::new(),
            fetch_tasks: HashMap::new(),
            stash_height: initial_height,
            settings_repo,
            resizing: false,
            drag_anchor: None,
            section_ceiling: None,
            cursor: None,
            view_mode,
            collapsed_dirs: HashMap::new(),
            resize_focus: cx.focus_handle(),
        };
        panel.refresh(cx);
        panel
    }

    pub fn state(&self) -> &StashListState {
        &self.state
    }

    /// Whether the body is currently hidden. Header stays rendered
    /// regardless so the count and `+` push affordance are always
    /// reachable.
    pub fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    /// Flip the body visibility. Wired to the header's chevron click.
    pub fn toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        self.collapsed = !self.collapsed;
        cx.notify();
    }

    /// Re-read the stash list, honouring `stash_list`'s 15 s read-TTL.
    ///
    /// `false` is right for the render path: the TTL only ever leaves us
    /// stale against an *external* writer, and self-heals within 15 s of the
    /// user actually looking at the section.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_inner(false, cx);
    }

    /// Re-read ignoring the TTL. For the header button (the user is asking
    /// precisely because they suspect the list is stale) and for the tail of
    /// our own ops, which must never sit on a change they just made.
    pub fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_inner(true, cx);
    }

    fn refresh_inner(&mut self, force: bool, cx: &mut Context<Self>) {
        // Keep showing the list we have while the new one loads. Every op now
        // ends in a refresh, so blanking to "Loading stashes…" each time both
        // flashes the section and — load-bearing — empties the sha→message
        // snapshot `drop_confirmed` takes for its rollback, which would leave
        // a wrongly-dropped stash restored under a synthesised label with its
        // real message gone for good.
        if !matches!(self.state, StashListState::Ready(_)) {
            self.state = StashListState::Loading;
        }
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<StashEntry>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.stash_list(force).await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    "no tokio runtime; stash_list skipped (step 14 wires runtime)"
                );
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |panel, cx| panel.apply_list_result(result, cx));
        });
        self._refresh_task = Some(task);
    }

    /// Land a freshly-read stash list.
    ///
    /// Split out from [`StashPanel::refresh_inner`] because everything worth
    /// asserting about a refresh — what the expansion state keeps, what the
    /// file cache drops, what gets refetched — happens here, while the part
    /// it was embedded in is a subprocess and a thread hop. A `#[gpui::test]`
    /// cannot drive that hop: the test scheduler panics the moment a tokio
    /// worker wakes a GPUI task. So the logic is reachable on its own and the
    /// subprocess is proved separately, in `crates/git`.
    pub fn apply_list_result(
        &mut self,
        result: Result<Vec<StashEntry>, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(entries) => {
                self.state = StashListState::Ready(entries);
                self.adopt_list(cx);
            }
            Err(e) => self.state = StashListState::Failed(e),
        }
        cx.notify();
    }

    /// Reconcile the expansion state against a list that has just landed, and
    /// invalidate the file cache it was built from.
    ///
    /// Three things happen here, and they are one step on purpose:
    ///
    /// * **The cache is dropped.** A refresh exists precisely because the
    ///   stack may have changed under us, and a stash's files are as stale as
    ///   its entry. (The stash commit itself is immutable, so this is
    ///   conservative rather than necessary — but a cache that outlives the
    ///   list it was keyed against is the kind of thing that only surfaces
    ///   once someone re-stashes over the same content.)
    /// * **Expansions for shas that are gone are forgotten**, so dropping a
    ///   stash does not leave its expansion behind to be re-adopted by a
    ///   future entry.
    /// * **Rows still expanded refetch.** Without this the cache clear above
    ///   would leave every open row blank until the user collapsed and
    ///   re-expanded it, and the fetch cannot be kicked from `render` —
    ///   `notify()` during a render pass is dropped.
    fn adopt_list(&mut self, cx: &mut Context<Self>) {
        let StashListState::Ready(entries) = &self.state else {
            return;
        };
        let live: HashSet<String> = entries.iter().map(|e| e.sha.clone()).collect();
        self.files.clear();
        self.fetch_tasks.clear();
        self.expanded.retain(|sha| live.contains(sha));
        // Collapsed folders are paths inside the file cache that was just
        // dropped; a set that outlives it would re-apply to a refetched list
        // it was never keyed against.
        self.collapsed_dirs.retain(|sha, _| live.contains(sha));
        // The cursor survives a refresh — that is the whole reason it is panel
        // state — but not the disappearance of the row it names. Dropping to
        // the first row rather than to nothing keeps the keyboard path alive
        // after a Drop the user just confirmed.
        if let Some(cursor) = &self.cursor
            && !live.contains(cursor.sha())
        {
            self.cursor = entries
                .first()
                .map(|e| keyboard::StashCursor::Stash(e.sha.clone()));
        }
        for sha in self.expanded.iter().cloned().collect::<Vec<_>>() {
            self.fetch_files(sha, cx);
        }
    }

    /// Whether `sha`'s file list is showing.
    pub fn is_expanded(&self, sha: &str) -> bool {
        self.expanded.contains(sha)
    }

    /// Current file layout for expanded stashes.
    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Flip flat ⇄ tree and persist the choice. Wired to the header toggle.
    ///
    /// The collapsed-folder set is deliberately NOT cleared: switching to flat
    /// and back should return the tree the user left, not one re-opened at
    /// every level.
    pub fn toggle_view_mode(&mut self, cx: &mut Context<Self>) {
        self.view_mode = self.view_mode.toggled();
        if let Some(repo) = &self.settings_repo {
            crate::app_settings::scm_layout_settings::save_stash_view_mode(repo, self.view_mode);
        }
        cx.notify();
    }

    /// Whether one folder inside one stash's tree is collapsed.
    pub fn is_dir_collapsed(&self, sha: &str, dir: &std::path::Path) -> bool {
        self.collapsed_dirs
            .get(sha)
            .is_some_and(|set| set.contains(dir))
    }

    /// Flip one folder's collapse state inside one stash's tree.
    pub fn toggle_dir(&mut self, sha: &str, dir: PathBuf, cx: &mut Context<Self>) {
        let set = self.collapsed_dirs.entry(sha.to_string()).or_default();
        if !set.remove(&dir) {
            set.insert(dir);
        }
        cx.notify();
    }

    /// The collapsed set for one stash, for the tree renderer.
    pub(super) fn collapsed_for(&self, sha: &str) -> HashSet<PathBuf> {
        self.collapsed_dirs.get(sha).cloned().unwrap_or_default()
    }

    /// Flip one stash's expansion, fetching its files the first time.
    ///
    /// Collapsing keeps the cache: re-expanding is then free, which is what
    /// makes the chevron cheap to poke at.
    pub fn toggle_expanded(&mut self, sha: String, cx: &mut Context<Self>) {
        if self.expanded.remove(&sha) {
            cx.notify();
            return;
        }
        self.expanded.insert(sha.clone());
        if !self.files.contains_key(&sha) {
            self.fetch_files(sha, cx);
        }
        cx.notify();
    }

    /// This stash's files, if any have been fetched.
    pub fn files_for(&self, sha: &str) -> Option<&StashFilesState> {
        self.files.get(sha)
    }

    /// Ask the host to open one stash file's diff, resolving the file's
    /// `origin` from the cached list.
    ///
    /// The context menu calls this instead of building a
    /// [`ShowStashFileRequested`] itself. `origin` selects which revision the
    /// file is read from and the two are not interchangeable, so it is looked
    /// up from the same list the row was painted from rather than carried
    /// through an action payload that could only carry it as a bool. A file
    /// whose stash is no longer cached is a no-op: the menu can only have
    /// been opened from a painted row, so this means the list moved under it.
    pub fn request_file_diff(&mut self, sha: &str, path: &std::path::Path, cx: &mut Context<Self>) {
        let Some(StashFilesState::Ready(files)) = self.files.get(sha) else {
            return;
        };
        let Some(file) = files.iter().find(|f| f.path == path) else {
            return;
        };
        let origin = file.origin;
        let label = self.label_of_stash(sha);
        cx.emit(ShowStashFileRequested {
            sha: sha.to_string(),
            path: path.to_path_buf(),
            origin,
            label,
        });
    }

    /// Ask the host to open every file in `sha` in one tab.
    ///
    /// The label is resolved here, from the list the row was painted from, for
    /// the same reason `request_file_diff` resolves `origin` here: the menu
    /// carries an action payload and the panel owns the data.
    pub fn request_stash_all(&mut self, sha: &str, cx: &mut Context<Self>) {
        cx.emit(ShowStashAllRequested {
            sha: sha.to_string(),
            label: self.label_of_stash(sha),
        });
    }

    /// Ask the host to prompt for a branch name for `sha`.
    pub fn request_branch_from_stash(&mut self, sha: &str, painted: usize, cx: &mut Context<Self>) {
        cx.emit(BranchFromStashRequested {
            sha: sha.to_string(),
            painted,
            message: match &self.state {
                StashListState::Ready(entries) => entries
                    .iter()
                    .find(|e| e.sha == sha)
                    .map(|e| e.message.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            },
        });
    }

    /// Ask the host to prompt for a new message for `sha`.
    ///
    /// `depth` is the row's position, which is exactly how many entries above
    /// it the rename will drop and put back. It is resolved here, from the
    /// list the row was painted from, for the same reason `request_file_diff`
    /// resolves `origin` here: the menu carries a payload, the panel owns the
    /// data. A stash that has left the stack is a no-op — the menu can only
    /// have been opened from a painted row, so this means the list moved.
    pub fn request_rename_stash(&mut self, sha: &str, painted: usize, cx: &mut Context<Self>) {
        let StashListState::Ready(entries) = &self.state else {
            return;
        };
        let Some((depth, entry)) = entries.iter().enumerate().find(|(_, e)| e.sha == sha) else {
            return;
        };
        cx.emit(RenameStashRequested {
            sha: sha.to_string(),
            painted,
            message: entry.message.clone(),
            depth,
        });
    }

    /// Ask the host to confirm restoring one file out of a stash.
    ///
    /// Refuses an untracked file rather than emitting: it lives in the
    /// parentless `^3`, not in the stash commit's tree, so the restore could
    /// only fail — and a destructive confirm dialog that can never succeed is
    /// worse than no menu item at all. The menu already omits the item for
    /// those rows; this is the gate on the side that owns `origin`.
    pub fn request_file_restore(
        &mut self,
        sha: &str,
        path: &std::path::Path,
        cx: &mut Context<Self>,
    ) {
        let Some(StashFilesState::Ready(files)) = self.files.get(sha) else {
            return;
        };
        let Some(file) = files.iter().find(|f| f.path == path) else {
            return;
        };
        if file.origin != StashFileOrigin::Tracked {
            return;
        }
        cx.emit(RestoreStashFileRequested {
            sha: sha.to_string(),
            path: path.to_path_buf(),
        });
    }

    /// What to call `sha` in a tab title or a dialog — its message, or an
    /// empty string when the list has moved on.
    fn label_of_stash(&self, sha: &str) -> String {
        match &self.state {
            StashListState::Ready(entries) => entries
                .iter()
                .find(|e| e.sha == sha)
                .map(list_render::row_message)
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// How many files a stash touches — `None` until the fetch lands.
    ///
    /// `None` rather than `0` on purpose: the list is lazy, so a row that has
    /// never been expanded knows nothing, and a confident `0` would be a
    /// claim we have not earned.
    pub fn file_count(&self, sha: &str) -> Option<usize> {
        match self.files.get(sha)? {
            StashFilesState::Ready(files) => Some(files.len()),
            _ => None,
        }
    }

    /// Land one stash's file list. See [`StashPanel::apply_list_result`] for
    /// why the completion handler is reachable on its own.
    pub fn apply_files_result(
        &mut self,
        sha: &str,
        result: Result<Vec<StashFile>, String>,
        cx: &mut Context<Self>,
    ) {
        // Land the result only where this fetch left its `Loading` marker.
        // `adopt_list` may have emptied the map between the subprocess
        // finishing and this hop, and re-inserting would resurrect a list the
        // refresh just retired.
        //
        // Deliberately NOT gated on `expanded`: a row collapsed mid-fetch
        // still gets its result cached, so re-expanding it is instant — and,
        // load-bearing, so its `Loading` marker is replaced. Dropping the
        // result instead would leave that marker behind, and since a sha that
        // already has an entry is never refetched, the row would be stuck on
        // "Loading…" for good.
        if !matches!(self.files.get(sha), Some(StashFilesState::Loading)) {
            return;
        }
        self.files.insert(
            sha.to_string(),
            match result {
                Ok(files) => StashFilesState::Ready(files),
                Err(e) => StashFilesState::Failed(e),
            },
        );
        cx.notify();
    }

    /// Read one stash's file list off tokio and cache the outcome.
    fn fetch_files(&mut self, sha: String, cx: &mut Context<Self>) {
        // Marked before the runtime check, so an expanded row shows
        // "Loading files…" rather than an empty gap — the same shape
        // `refresh_inner` leaves the list in when there is no runtime.
        self.files.insert(sha.clone(), StashFilesState::Loading);
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<Vec<StashFile>, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let query = sha.clone();
                handle.spawn(async move {
                    let r = repo.stash_files(&query).await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::stash_panel",
                    "no tokio runtime; stash_files skipped"
                );
                return;
            }
        }
        let key = sha.clone();
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |panel, cx| panel.apply_files_result(&key, result, cx));
        });
        // Keyed insert, so a second expand cannot cancel the first. The slot
        // is deliberately NOT cleared from inside the task above — that would
        // drop the task handle while its own future is being polled.
        self.fetch_tasks.insert(sha, task);
    }
}

impl Focusable for StashPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StashPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        // Releasing the mouse produces no further drag-move tick, so the flag
        // and the latched anchor are cleared on the next render instead —
        // same as the graph's handle (`graph.rs:524`).
        if self.resizing && !cx.has_active_drag() {
            self.resizing = false;
            self.drag_anchor = None;
        }
        let count = match &self.state {
            StashListState::Ready(entries) => entries.len(),
            _ => 0,
        };
        let header = self.render_header(count, cx);

        let mut container = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .bg(self.theme.bg_panel);
        // Mouse drag-resize handle at the section's top edge, only when there
        // is a body to resize. The matching `on_drag_move` lives on the
        // workspace root.
        if !self.collapsed {
            container = container.child(self.render_drag_handle(cx));
        }
        container = container.child(header);

        if !self.collapsed {
            let body = match &self.state {
                StashListState::Idle | StashListState::Loading => placeholder(
                    "Loading stashes…",
                    self.theme,
                    self.density,
                    &self.typography,
                )
                .into_any_element(),
                StashListState::Failed(err) => placeholder(
                    &format!("stash list failed: {err}"),
                    self.theme,
                    self.density,
                    &self.typography,
                )
                .into_any_element(),
                StashListState::Ready(entries) if entries.is_empty() => {
                    placeholder("No stashes yet", self.theme, self.density, &self.typography)
                        .into_any_element()
                }
                StashListState::Ready(entries) => {
                    let mut col = div().flex().flex_col().w_full();
                    for entry in entries.iter().cloned() {
                        col = col.child(self.render_row(entry, cx));
                    }
                    col.into_any_element()
                }
            };
            // Bound the body and give the overflow its own scroll region.
            //
            // This section is `flex_shrink_0` and so is the graph below it,
            // which leaves the changed-files block as the SCM column's only
            // flexible child — it absorbs 100% of any height deficit. An
            // unbounded stash list therefore does not push itself off-screen,
            // it squeezes CHANGES until the header is guillotined. Bounding
            // the section's appetite is half the fix; the floor on the file
            // block (`files_floor`) is the other half.
            //
            // The bound is now the user's own, not Phase 1's eight-row guess:
            // `painted_height` is what they dragged to, trimmed to what the
            // budget shared with the graph currently allows. A definite height
            // rather than a `max_h`, so the section keeps the size it was
            // given even when the list is short — otherwise the handle would
            // jump under the cursor as rows expand and collapse.
            //
            // `.id()` is load-bearing: `overflow_y_scroll` without a stateful
            // id silently does nothing (the GPUI trap documented at
            // `git_panel/mod.rs:527`). `relative()` anchors the scrollbar
            // overlay, and `track_scroll` + `vertical_scrollbar` share
            // `scroll_handle` so the thumb mirrors the scroll position.
            container = container.child(
                div()
                    .id("stash-panel-scroll")
                    // Focus and the cursor key context live on the LIST, not
                    // on the section container.
                    //
                    // The resize rail below is a sibling with its own focus
                    // handle and its own `on_key_down` for Arrow / Shift+Arrow
                    // / Home / End. If the context sat on their common parent,
                    // a focused rail would still have `StashPanel` on its
                    // dispatch chain, the arrow keys would match the cursor
                    // bindings, and the rail's own handler would be racing a
                    // keymap action for the same keystroke. Scoping to the
                    // list means each surface owns the arrows while it is the
                    // one being driven.
                    .track_focus(&self.focus_handle)
                    .key_context(keyboard::STASH_PANEL_KEY_CONTEXT)
                    .on_action(cx.listener(Self::on_cursor_up))
                    .on_action(cx.listener(Self::on_cursor_down))
                    .on_action(cx.listener(Self::on_cursor_expand))
                    .on_action(cx.listener(Self::on_cursor_collapse))
                    .on_action(cx.listener(Self::on_cursor_activate))
                    .on_action(cx.listener(Self::on_cursor_drop))
                    .relative()
                    .w_full()
                    .h(self.painted_height())
                    // Clicking anywhere in the list focuses the panel, which
                    // is what puts `StashPanel` on the dispatch chain and
                    // makes the arrow keys live. DEFERRED: focus taken
                    // synchronously inside a mouse handler is clobbered by
                    // GPUI's own post-click focus dispatch — the trap the rail
                    // and the tab bar both hit — so it is handed to the next
                    // frame instead.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|panel, _: &gpui::MouseDownEvent, window, cx| {
                            let handle = panel.focus_handle.clone();
                            window.defer(cx, move |window, cx| window.focus(&handle, cx));
                        }),
                    )
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .child(body)
                    .vertical_scrollbar(&self.scroll_handle),
            );
            // Keyboard resize rail, below the list so Tab arrives here after
            // the rows.
            container = container.child(self.render_resize_rail(window, cx));
        }

        container
    }
}

impl StashPanel {
    /// Header: chevron toggle + "STASHES (N)" label + push button.
    /// Always rendered, even when the body is collapsed, so the count
    /// stays visible at a glance and the `+` action is always
    /// reachable. Chevron points down when open, right when collapsed
    /// (matches the SCM-section convention).
    fn render_header(&self, count: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::shell::source_control::style::ScmStyle;
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let style = ScmStyle::new(density, typography);
        let collapsed = self.collapsed;
        let chevron = if collapsed {
            "icons/chevron-right.svg"
        } else {
            "icons/chevron-down.svg"
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_row))
            .px(px(density.pad_panel))
            .gap(px(density.gap_inline))
            .border_b_1()
            .border_color(theme.border_inactive)
            .text_size(px(typography.t_label_caps))
            .text_color(theme.fg_muted)
            .child(
                // Whole chevron+label area is clickable, mirroring
                // collapsible-section UX elsewhere in the panel.
                div()
                    .id("stash-header-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(density.gap_inline))
                    .flex_1()
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        panel.toggle_collapsed(cx);
                    }))
                    .child(
                        Icon::default()
                            .path(chevron)
                            .size(px(style.icon))
                            .text_color(theme.fg_muted),
                    )
                    .child(format!("STASHES ({count})")),
            )
            .child(
                // Flat ⇄ tree for the files inside an expanded stash. Same
                // icon pair and same "name the destination, not the state"
                // tooltip wording as the CHANGES toolbar, so one control
                // taught twice is one control.
                Button::new("stash-view-mode")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path(match self.view_mode {
                        ViewMode::Flat => "icons/list-tree.svg",
                        ViewMode::Tree => "icons/list-collapse.svg",
                    }))
                    .tooltip(match self.view_mode {
                        ViewMode::Flat => "Switch to tree view",
                        ViewMode::Tree => "Switch to flat view",
                    })
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        panel.toggle_view_mode(cx);
                    })),
            )
            .child(
                Button::new("stash-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/refresh-cw.svg"))
                    .tooltip("Re-read the stash list")
                    .on_click(cx.listener(|panel, _: &ClickEvent, _window, cx| {
                        // Forced: the user is clicking this precisely because
                        // they think the cached list is behind git.
                        panel.force_refresh(cx);
                    })),
            )
            .child(
                Button::new("stash-push-new")
                    .ghost()
                    .xsmall()
                    .icon(Icon::default().path("icons/plus.svg"))
                    .tooltip("Push new stash")
                    .on_click(cx.listener(|_panel, _: &ClickEvent, _window, cx| {
                        cx.emit(PushStashRequested);
                    })),
            )
            .into_any_element()
    }

}

fn placeholder(
    msg: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(density.h_action_row))
        .p(px(density.pad_panel))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(msg.to_string())
}
