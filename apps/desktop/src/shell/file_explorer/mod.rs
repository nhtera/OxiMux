//! FileExplorer — virtualized file tree panel for the right sidebar.
//!
//! Subscribes to the StatusPoller watch channel for git decoration; loads
//! directories lazily via `fs_load::load_dir_cache`. Renders via
//! `gpui::uniform_list` for 60-fps scrolling on large trees.

pub mod create_ops;
pub mod file_icon;
pub mod file_mutations;
pub mod file_tree_context_menu;
pub mod file_tree_view;
pub mod fs_load;
pub mod header_render;
pub mod load_ops;
pub mod paint;
pub mod rename_ops;
pub mod row_render;
pub mod status_display;
pub mod tree_state;

use crate::shell::file_explorer::header_render::render_header;
use crate::shell::file_explorer::paint::{PaintCtx, paint_row};
use crate::shell::file_explorer::row_render::build_row_plan;
use crate::shell::file_explorer::status_display::{
    BadgeStatus, build_folder_status_map, build_status_map,
};
use crate::shell::file_explorer::tree_state::{
    DirCache, TreeNode, ancestor_dirs, filter_visible, flatten,
};
use crate::shell::file_tree_view::OnOpenFile;
use gpui::{
    AnyElement, App, ClipboardItem, Context, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollStrategy, Styled, Subscription, Task,
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
    /// Inline-rename state. `Some` while the user is editing a row's
    /// basename; the row builder swaps the matching path's label for an
    /// `Input` widget bound to this entity. See `rename_ops.rs`.
    pub(crate) renaming: Option<rename_ops::RenameState>,
    /// Inline-create state. `Some` while the user is naming a new file or
    /// folder; `recompute_rows` injects a placeholder row under the parent
    /// that the row builder mounts an `Input` on. See `create_ops.rs`.
    pub(crate) creating: Option<create_ops::CreateState>,
    /// Path queued by `reveal_path` to be scrolled into view. Ancestor
    /// directory loads are async, so the target row may not exist yet when
    /// reveal is requested; the scroll is retried after each load lands and
    /// cleared once it succeeds.
    pending_reveal: Option<PathBuf>,
    /// Keyboard focus for the panel. Clicking a row focuses this handle so the
    /// explorer keeps key focus (preview opens without stealing it), enabling
    /// the row-scoped shortcuts: Enter = rename, Cmd+Backspace = delete,
    /// Cmd+Alt+C / Cmd+Alt+Shift+C = copy absolute / relative path.
    focus_handle: FocusHandle,
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
            renaming: None,
            creating: None,
            pending_reveal: None,
            focus_handle: cx.focus_handle(),
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
        self.inject_create_placeholder();
    }

    /// Splice the inline-create placeholder row into the flat list (no-op when
    /// not creating). It lands as the first child under its parent directory,
    /// or at the end of the list when creating at the repo root (whose row is
    /// never shown). The row carries the create sentinel path so the row
    /// builder mounts the `Input` on it.
    fn inject_create_placeholder(&mut self) {
        let Some(create) = self.creating.as_ref() else {
            return;
        };
        let parent_depth = self
            .rows
            .iter()
            .find(|n| n.path == create.parent)
            .map(|n| n.depth);
        let node = TreeNode {
            name: String::new(),
            path: create_ops::create_sentinel(&create.parent),
            relative_path: PathBuf::new(),
            is_directory: create.is_dir,
            depth: parent_depth.map(|d| d + 1).unwrap_or(0),
        };
        match self.rows.iter().position(|n| n.path == create.parent) {
            Some(idx) => self.rows.insert(idx + 1, node),
            None => self.rows.push(node),
        }
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

    /// Reveal `path` in the tree: expand every ancestor directory, mark the
    /// row selected, and scroll it into view. Ancestor directory loads are
    /// async, so the scroll is deferred via `pending_reveal` and retried as
    /// each load lands (see `try_scroll_pending_reveal`). Bypasses the
    /// `toggle_dir` depth guard — an explicit reveal is always honored.
    pub(crate) fn reveal_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let ancestors = ancestor_dirs(&self.repo_root, &path);
        let mut pending_loads = Vec::new();
        for dir in &ancestors {
            self.expanded.insert(dir.clone());
            let needs_load = self
                .cache
                .get(dir)
                .map(|c| !c.loaded && !c.loading)
                .unwrap_or(true);
            if needs_load {
                pending_loads.push(dir.clone());
            }
        }
        self.selected = Some(path.clone());
        self.pending_reveal = Some(path);
        for dir in pending_loads {
            let repo_root = self.repo_root.clone();
            let task = self.spawn_load_dir(dir, repo_root, false, cx);
            self.push_task(task);
        }
        self.recompute_rows();
        // Scroll now if the chain is already loaded; otherwise the async
        // load completions will retry until the row exists.
        self.try_scroll_pending_reveal();
        cx.notify();
    }

    /// If a reveal is pending and its row is now present in the flat list,
    /// scroll it into view and clear the request. No-op otherwise. Called
    /// after every directory-load completion.
    pub(super) fn try_scroll_pending_reveal(&mut self) {
        let Some(target) = self.pending_reveal.clone() else {
            return;
        };
        if let Some(ix) = self.rows.iter().position(|n| n.path == target) {
            self.list_scroll.scroll_to_item(ix, ScrollStrategy::Center);
            self.pending_reveal = None;
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
            // Single-click opens a reusable preview tab (italic, replaced by
            // the next single-click). A preview tab is promoted to permanent
            // by double-clicking the tab chip, or by editing it.
            (cb)(path, true, window, cx);
        }
    }

    /// Give the panel keyboard focus. Called on row click so the explorer's
    /// row-scoped shortcuts receive keystrokes even though opening a file
    /// focuses the editor.
    pub(crate) fn focus_panel(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
    }

    /// Row-scoped keyboard shortcuts, dispatched while the panel holds focus
    /// and a row is selected. Mirrors the context-menu actions so the hints
    /// shown there (↵, ⌘⌫, ⌘⌥C, ⌘⌥⇧C) are real:
    /// - Enter → rename the selected entry,
    /// - Cmd+Backspace → delete (move to Trash, with confirm),
    /// - Cmd+Alt+C → copy absolute path,
    /// - Cmd+Alt+Shift+C → copy path relative to the repo root.
    fn handle_explorer_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While an inline rename or create is active the Input owns keystrokes
        // (Enter commits, Esc cancels) — don't intercept with row shortcuts.
        if self.renaming.is_some() || self.creating.is_some() {
            return;
        }
        let Some(path) = self.selected.clone() else {
            return;
        };
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        match ks.key.as_str() {
            "enter" if !m.platform && !m.alt && !m.control && !m.shift => {
                window.dispatch_action(
                    Box::new(crate::actions::FileTreeRename {
                        path: path.to_string_lossy().into_owned(),
                    }),
                    cx,
                );
            }
            "backspace" if m.platform && !m.alt && !m.control && !m.shift => {
                window.dispatch_action(
                    Box::new(crate::actions::FileTreeDelete {
                        path: path.to_string_lossy().into_owned(),
                    }),
                    cx,
                );
            }
            "c" if m.platform && m.alt => {
                let text = if m.shift {
                    path.strip_prefix(&self.repo_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned()
                } else {
                    path.to_string_lossy().into_owned()
                };
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            _ => {}
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
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
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

                    // Snapshot the rename target (if any) once, before the
                    // per-row loop, so each iteration just does a cheap
                    // PathBuf::eq instead of touching the entity tree.
                    let rename_path = me.renaming.as_ref().map(|r| r.path.clone());
                    let rename_input = me.renaming.as_ref().map(|r| r.input.clone());
                    // The create placeholder is matched by its sentinel path,
                    // so the input mounts only on the injected row.
                    let create_sentinel = me
                        .creating
                        .as_ref()
                        .map(|c| create_ops::create_sentinel(&c.parent));
                    let create_input = me.creating.as_ref().map(|c| c.input.clone());
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
                            // This row gets an inline input only when it matches
                            // the rename target or the create sentinel. Cloning
                            // the entity handle is cheap (Arc bump).
                            let inline = if rename_path.as_ref() == Some(&path) {
                                rename_input.clone().map(paint::InlineEdit::Rename)
                            } else if create_sentinel.as_ref() == Some(&path) {
                                create_input.clone().map(paint::InlineEdit::Create)
                            } else {
                                None
                            };
                            paint_row(plan, &pctx, path, is_dir, is_loading, inline, cx)
                                .into_any_element()
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
        //
        // Right-click anywhere inside the list region that isn't on a row
        // opens the background context menu (New File / New Folder at the
        // workspace root). Row right-clicks call `stop_propagation`, so
        // they're filtered out before reaching this listener.
        let bg_root = self.repo_root.clone();
        div()
            .id("file-explorer-shell")
            .track_focus(&self.focus_handle)
            .key_context("FileExplorer")
            .on_key_down(cx.listener(|me, ev: &KeyDownEvent, window, cx| {
                me.handle_explorer_key(ev, window, cx);
            }))
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            // Background left-click (in the empty area below the rows)
            // cancels any in-flight inline rename. The renaming row
            // calls `stop_propagation` on its own mouse_down so clicks
            // inside the row (input, icon, chevron) don't reach here;
            // clicks on OTHER rows are handled at the row level (those
            // also cancel). Without this, clicking blank space below
            // the list would leave an orphaned input mounted.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|me, _: &gpui::MouseDownEvent, _window, cx| {
                    me.cancel_rename(cx);
                    me.cancel_create(cx);
                }),
            )
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |me, ev: &gpui::MouseDownEvent, window, cx| {
                    // Background right-click also discards any inline edit.
                    me.cancel_rename(cx);
                    me.cancel_create(cx);
                    window.dispatch_action(
                        Box::new(crate::actions::OpenFileTreeBackgroundMenuAt {
                            x: ev.position.x.into(),
                            y: ev.position.y.into(),
                            root: bg_root.to_string_lossy().into_owned(),
                        }),
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
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
