//! FileExplorer — virtualized file tree panel for the right sidebar.
//!
//! Subscribes to the StatusPoller watch channel for git decoration; loads
//! directories lazily via `fs_load::load_dir_cache`. Renders via
//! `gpui::uniform_list` for 60-fps scrolling on large trees.

pub mod file_icon;
pub mod fs_load;
pub mod header_render;
pub mod load_ops;
pub mod paint;
pub mod row_render;
pub mod status_display;
pub mod tree_state;

use crate::shell::file_explorer::header_render::render_header;
use crate::shell::file_explorer::paint::{PaintCtx, paint_row};
use crate::shell::file_explorer::row_render::build_row_plan;
use crate::shell::file_explorer::status_display::{
    BadgeStatus, build_folder_status_map, build_status_map,
};
use crate::shell::file_explorer::tree_state::{DirCache, TreeNode, filter_visible, flatten};
use crate::shell::file_tree_view::OnOpenFile;
use gpui::{
    AnyElement, App, Context, IntoElement, ParentElement, Render, Styled, Subscription, Task,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use oximux_core::FileStatus;
use oximux_git::PollState;
use oximux_settings::{Density, Theme, Typography};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Maximum directory depth that will be lazily expanded. Prevents accidental
/// infinite loops on repos with deep or cyclic paths (e.g. broken symlinks
/// that survived the symlink-skip in fs_load).
const MAX_EXPAND_DEPTH: usize = 12;

/// Virtualized file-tree panel. Holds lazy-loaded directory cache and git
/// status maps; renders flat row list via `uniform_list`.
pub struct FileExplorer {
    repo_root: PathBuf,
    poll_state: Option<PollState>,
    expanded: HashSet<PathBuf>,
    cache: HashMap<PathBuf, DirCache>,
    selected: Option<PathBuf>,
    /// Flattened visible rows — rebuilt on expand/collapse or cache update.
    rows: Vec<TreeNode>,
    status_map: HashMap<PathBuf, BadgeStatus>,
    folder_status_map: HashMap<PathBuf, BadgeStatus>,
    list_scroll: UniformListScrollHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// `true` once the root directory load has completed at least once.
    root_loaded: bool,
    /// When `false` (default), entries with `BadgeStatus::Ignored` are hidden
    /// from the flat row list. Toggle via the eye button in the header.
    show_ignored: bool,
    /// Clone of the last `files` slice used to build status maps. Skip rebuild
    /// if identical to the incoming slice (avoids redundant work on no-op polls).
    prev_files: Vec<FileStatus>,
    /// In-flight load tasks. Capped at MAX_LOAD_TASKS; oldest dropped first.
    _load_tasks: Vec<Task<()>>,
    /// Poll observer task. Drop to cancel.
    _poll_observer: Task<()>,
    /// Window-activation subscription for focus-regain refresh.
    _activation_sub: Subscription,
    /// Callback to open a clicked file as an editor tab in the active
    /// project's pane group. `None` in test contexts (no host wired) —
    /// falls back to a no-op so unit tests don't accidentally shell out.
    /// Pattern mirrors `file_tree_view::FileTreeView::on_open`.
    on_open: Option<OnOpenFile>,
}

impl FileExplorer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo_root: PathBuf,
        state_rx: tokio::sync::watch::Receiver<PollState>,
        theme: Theme,
        density: Density,
        typography: Typography,
        on_open: Option<OnOpenFile>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let poll_observer = Self::start_poll_observer(state_rx, cx);

        // Subscribe to window-activation so focus regain triggers a refresh of
        // all expanded directories (no full re-scan; reuses cache merge).
        let activation_sub = cx.observe_window_activation(window, |me, window, cx| {
            if window.is_window_active() {
                me.refresh_expanded(cx);
            }
        });

        let mut explorer = Self {
            repo_root: repo_root.clone(),
            poll_state: None,
            expanded: HashSet::new(),
            cache: HashMap::new(),
            selected: None,
            rows: Vec::new(),
            status_map: HashMap::new(),
            folder_status_map: HashMap::new(),
            list_scroll: UniformListScrollHandle::new(),
            theme,
            density,
            typography,
            root_loaded: false,
            show_ignored: false,
            prev_files: Vec::new(),
            _load_tasks: Vec::new(),
            _poll_observer: poll_observer,
            _activation_sub: activation_sub,
            on_open,
        };

        // Kick off root directory load on mount.
        let task = explorer.spawn_load_dir(repo_root.clone(), repo_root, true, cx);
        explorer._load_tasks.push(task);
        explorer
    }

    /// Update mirrored poll state and recompute status maps.
    /// Skips the rebuild if the files slice is identical to the previous poll
    /// (avoids redundant work on no-op status polls — M5).
    fn set_poll_state(&mut self, state: PollState, cx: &mut Context<Self>) {
        if let PollState::Ready(ref git_state) = state {
            if git_state.files != self.prev_files {
                self.status_map = build_status_map(&git_state.files);
                self.folder_status_map = build_folder_status_map(&git_state.files);
                self.prev_files = git_state.files.clone();
                // Status map drives the ignored-row filter; rebuild rows so the
                // hide/show state stays in sync with fresh poll output.
                self.recompute_rows();
            }
        } else {
            self.status_map.clear();
            self.folder_status_map.clear();
            self.prev_files.clear();
            self.recompute_rows();
        }
        self.poll_state = Some(state);
        cx.notify();
    }

    /// Rebuild the flat row list from current cache + expanded set, applying
    /// the `show_ignored` filter when toggled off. The pure filter is in
    /// `tree_state::filter_visible` so the descendant-drop logic stays
    /// independently unit-testable.
    fn recompute_rows(&mut self) {
        let all = flatten(&self.repo_root, &self.cache, &self.expanded);
        let ignored: Vec<PathBuf> = self
            .status_map
            .iter()
            .filter(|(_, s)| **s == BadgeStatus::Ignored)
            .map(|(p, _)| p.clone())
            .collect();
        self.rows = filter_visible(all, &ignored, self.show_ignored);
    }

    /// Flip the show/hide flag for ignored entries and refresh the row list.
    pub(crate) fn toggle_show_ignored(&mut self, cx: &mut Context<Self>) {
        self.show_ignored = !self.show_ignored;
        self.recompute_rows();
        cx.notify();
    }

    /// Collapse all expanded directories.
    pub(crate) fn collapse_all(&mut self, cx: &mut Context<Self>) {
        if self.expanded.is_empty() {
            return;
        }
        self.expanded.clear();
        self.recompute_rows();
        cx.notify();
    }

    /// Manually re-read every cached directory from disk (root + expanded).
    /// Useful when the watch-based refresh missed an external mutation.
    pub(crate) fn manual_refresh(&mut self, cx: &mut Context<Self>) {
        let repo_root = self.repo_root.clone();
        let task = self.spawn_load_dir(repo_root.clone(), repo_root, true, cx);
        self.push_task(task);
        self.refresh_expanded(cx);
    }

    /// `true` when the explorer is currently hiding ignored entries.
    pub fn show_ignored(&self) -> bool {
        self.show_ignored
    }

    /// `true` when there is at least one collapsable expanded directory.
    pub fn can_collapse_all(&self) -> bool {
        !self.expanded.is_empty()
    }

    /// `true` when at least one entry in the current poll state is Ignored —
    /// gates the visibility of the eye toggle (hidden when nothing to hide).
    pub fn has_ignored_entries(&self) -> bool {
        self.status_map.values().any(|s| *s == BadgeStatus::Ignored)
    }

    /// Toggle directory expanded state. Lazily loads children if not yet
    /// loaded. Refuses to expand beyond MAX_EXPAND_DEPTH.
    pub(crate) fn toggle_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.expanded.contains(&path) {
            self.expanded.remove(&path);
            self.recompute_rows();
            cx.notify();
            return;
        }

        // Depth guard: find the node's depth in the current row list.
        let depth = self
            .rows
            .iter()
            .find(|n| n.path == path)
            .map(|n| n.depth)
            .unwrap_or(0);
        if depth >= MAX_EXPAND_DEPTH {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %path.display(),
                "refusing to expand beyond max depth {MAX_EXPAND_DEPTH}"
            );
            return;
        }

        self.expanded.insert(path.clone());

        // Load if not yet cached and fully loaded.
        let needs_load = self
            .cache
            .get(&path)
            .map(|c| !c.loaded && !c.loading)
            .unwrap_or(true);
        if needs_load {
            let repo_root = self.repo_root.clone();
            let task = self.spawn_load_dir(path, repo_root, false, cx);
            self.push_task(task);
        } else {
            self.recompute_rows();
            cx.notify();
        }
    }

    /// Dispatch a file click to the host's open-file callback (opens the
    /// file as an editor tab in the active project's active pane group).
    /// When `on_open` is `None` (test wiring without a host) the click is
    /// silently dropped — must not shell out to `open(1)` because that
    /// would launch the file outside OxiMux, breaking the cockpit-tight
    /// contract: clicked files belong in the center pane.
    pub(crate) fn open_file(&self, path: PathBuf, window: &mut Window, cx: &mut App) {
        if let Some(cb) = self.on_open.as_ref() {
            (cb)(path, window, cx);
        }
    }

    /// Read-only slice of the current flat row list. Used by tests.
    pub fn rows(&self) -> &[TreeNode] {
        &self.rows
    }

    /// Read-only view of the per-file status map. Used by tests.
    pub fn status_map(&self) -> &HashMap<PathBuf, BadgeStatus> {
        &self.status_map
    }
}

impl Render for FileExplorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let count = self.rows.len();
        let header = render_header(self, cx);

        if count == 0 {
            let msg = if self.root_loaded {
                "No files"
            } else {
                "Loading…"
            };
            let body = div()
                .flex()
                .items_center()
                .justify_center()
                .flex_1()
                .w_full()
                .text_color(theme.fg_subtle)
                .text_size(px(self.typography.t_body_sm))
                .child(msg);
            return div()
                .flex()
                .flex_col()
                .h_full()
                .w_full()
                .bg(theme.bg_panel)
                .child(header)
                .child(body)
                .into_any_element();
        }

        // `cx.processor` provides `&mut FileExplorer` inside the uniform_list
        // closure so row clicks can call `toggle_dir` / `open_file`.
        let list = uniform_list(
            "file-explorer",
            count,
            cx.processor(
                |me: &mut FileExplorer,
                 range: std::ops::Range<usize>,
                 _window: &mut Window,
                 cx: &mut Context<FileExplorer>| {
                    // Snapshot fields that the closure reads — avoids holding
                    // a borrow on `me` across the row-building loop.
                    let rows = me.rows.clone();
                    let expanded = me.expanded.clone();
                    let selected = me.selected.clone();
                    let status_map = me.status_map.clone();
                    let folder_status_map = me.folder_status_map.clone();
                    let typography = me.typography.clone();
                    let cache = me.cache.clone();
                    let pctx = PaintCtx {
                        theme: me.theme,
                        density: me.density,
                        typography: &typography,
                    };

                    range
                        .map(|i| {
                            let node = &rows[i];
                            let is_expanded = expanded.contains(&node.path);
                            let is_selected = selected.as_ref() == Some(&node.path);
                            let rel = &node.relative_path;
                            let file_status = status_map.get(rel).copied();
                            let folder_status = folder_status_map.get(rel).copied();
                            let is_loading =
                                cache.get(&node.path).map(|c| c.loading).unwrap_or(false);
                            let plan = build_row_plan(
                                node,
                                is_expanded,
                                is_selected,
                                file_status,
                                folder_status,
                            );
                            let path = node.path.clone();
                            let is_dir = node.is_directory;
                            paint_row(plan, &pctx, path, is_dir, is_loading, cx).into_any_element()
                        })
                        .collect::<Vec<AnyElement>>()
                },
            ),
        )
        .track_scroll(&self.list_scroll)
        .h_full()
        .w_full();

        // Flex column with explicit h_full + bg so the uniform_list child has
        // a defined height to lay rows against. Avoid nested flex_1 — uniform_list
        // already sets overflow_y: scroll so it manages its own scroll area.
        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            .child(header)
            .child(list)
            .into_any_element()
    }
}

impl FileExplorer {
    /// Repo root path. Used by `header_render` to derive the panel title.
    pub fn repo_root(&self) -> &PathBuf {
        &self.repo_root
    }

    /// Theme snapshot. Used by `header_render` for icon/text colors.
    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Typography snapshot. Used by `header_render` for label sizing.
    pub fn typography_ref(&self) -> &Typography {
        &self.typography
    }
}
