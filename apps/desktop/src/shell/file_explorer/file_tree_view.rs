//! FileTreeView — virtualized file-tree pane subscribed to a step-3
//! `Entity<FileTree>`. Lazy expand on click, fires `on_open` for files.
//!
//! Rendering uses `gpui::uniform_list` directly (same as `FileExplorer`)
//! rather than `gpui-component::Tree`: Tree's automatic expand-on-click
//! fights our `FileTree` lazy walker model, so we own expansion state
//! here (`expanded_ids: HashSet<TreeNodeId>`) and rebuild the row list
//! whenever it changes or a `FileTreeEvent::Loaded` arrives.

use gpui::{
    AnyElement, App, AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, UniformListScrollHandle, Window, div, prelude::FluentBuilder, px,
    uniform_list,
};
use oximux_editor::{FileTree, FileTreeEvent, FileTreeNode, TreeNodeId};
use oximux_settings::{Density, Theme, Typography};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use crate::shell::openable_text_file::is_openable_text_file;
use crate::shell::pane_group::file_drag::{FilePathDragPayload, FilePathDragPreview};

/// Click-on-file callback. `Arc` rather than `Box` so the closure can be
/// cloned into every row's `cx.listener` without ownership ceremony.
/// `Send` is deliberately dropped (the callback only ever fires from the
/// GPUI main thread inside `handle_file_click`); none of the background
/// executor or tokio paths capture this Arc.
/// `(path, preview, window, app)`. `preview = true` for a single-click open
/// (reusable italic preview tab); `false` for a double-click (permanent tab).
pub type OnOpenFile = Arc<dyn Fn(PathBuf, bool, &mut Window, &mut App) + 'static>;

/// Diff open callback. Same shape as `OnOpenFile` but carries two
/// discriminators: `staged` (index-vs-HEAD when true, worktree-vs-index
/// when false) and `untracked` (file is not yet in git — read from disk
/// to synthesize an all-additions diff). Lives here next to `OnOpenFile`
/// because both are right-sidebar-injected callbacks consumed by the
/// same set of panels.
///
/// Signature: `(path, staged, untracked, window, app)`. `untracked` is
/// honored only when `staged == false` — a staged untracked file is a
/// contradiction (staging promotes it to "added in index").
pub type OnOpenDiff = Arc<dyn Fn(PathBuf, bool, bool, &mut Window, &mut App) + 'static>;

/// Active-file query callback. Invoked once per render to find the file
/// currently open in the focused pane; the matching tree row is then
/// rendered with the active-file highlight in addition to (or instead of)
/// the click-selected row. `None` means no editor leaf is focused, or
/// the focused editor's file lives outside the tree's root.
pub type OnQueryActivePath = Arc<dyn Fn(&App) -> Option<PathBuf> + 'static>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowKind {
    Dir {
        id: TreeNodeId,
        loaded: bool,
        /// True iff this dir is currently in the view's `expanded_ids` set
        /// — i.e., the user wants to see its children. Distinct from
        /// `loaded`, which is a permanent "walker has visited" flag that
        /// never resets on collapse. The chevron renders against this, not
        /// against `loaded`, so a collapsed-after-load dir shows `▸`.
        expanded: bool,
    },
    File {
        id: TreeNodeId,
    },
    /// Sentinel inserted under an unloaded expanded dir so the row appears
    /// expandable while the background walker is still running.
    Placeholder {
        parent_id: TreeNodeId,
    },
    /// Sentinel for an empty loaded dir — gives the user feedback that the
    /// walker finished and the directory really is empty.
    EmptyDir {
        parent_id: TreeNodeId,
    },
}

#[derive(Clone, Debug)]
pub struct DisplayRow {
    pub label: String,
    pub depth: usize,
    pub kind: RowKind,
    pub path: PathBuf,
}

pub struct FileTreeView {
    tree: Entity<FileTree>,
    rows: Vec<DisplayRow>,
    expanded_ids: HashSet<TreeNodeId>,
    /// Path-keyed mirror of the user's expanded set. A watcher-driven
    /// refresh evicts a directory's subtree and re-allocates fresh
    /// `TreeNodeId`s for its children, so `expanded_ids` (keyed on the now-
    /// stale ids) would silently collapse every re-expanded subdir. We
    /// re-resolve the open set by path on each `Loaded` so expansion
    /// survives a refresh. Kept in sync with `expanded_ids` on click.
    expanded_paths: HashSet<PathBuf>,
    selected: Option<TreeNodeId>,
    scroll: UniformListScrollHandle,
    on_open: OnOpenFile,
    /// Optional pulse from the host on every render: "what file is the
    /// focused editor showing right now?" Used to highlight the matching
    /// row separately from the click-selection. `None` until the host
    /// provides a query.
    on_query_active_path: Option<OnQueryActivePath>,
    /// Active app palette, threaded from the host (same `Theme` the rest of
    /// the shell uses) rather than constructed locally — so the file tree
    /// tracks the app theme instead of pinning a hardcoded dark palette.
    theme: Theme,
    _sub: Subscription,
}

impl FileTreeView {
    pub fn new(
        tree: Entity<FileTree>,
        theme: Theme,
        on_open: OnOpenFile,
        on_query_active_path: Option<OnQueryActivePath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Register the subscription BEFORE kicking off the first expand so
        // we cannot miss the first `Loaded` event. Pattern mirrors
        // source_control/mod.rs:117-128.
        let sub = cx.subscribe_in(
            &tree,
            window,
            |me: &mut Self, _tree, ev: &FileTreeEvent, _window, cx| {
                me.handle_tree_event(ev, cx);
            },
        );

        let root_id = tree.read(cx).root_id();
        let root_path = tree.read(cx).root_path().to_path_buf();
        let mut expanded_ids = HashSet::new();
        expanded_ids.insert(root_id);
        let mut expanded_paths = HashSet::new();
        expanded_paths.insert(root_path);

        // Kick off root walk; result arrives async as `FileTreeEvent::Loaded`.
        tree.update(cx, |t, cx| t.expand(root_id, cx));

        Self {
            tree,
            rows: Vec::new(),
            expanded_ids,
            expanded_paths,
            theme,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            on_open,
            on_query_active_path,
            _sub: sub,
        }
    }

    fn handle_tree_event(&mut self, ev: &FileTreeEvent, cx: &mut Context<Self>) {
        match ev {
            FileTreeEvent::Loaded { children, .. } => {
                self.reconcile_expanded(children, cx);
                self.rebuild_rows(cx);
                cx.notify();
            }
            FileTreeEvent::Refresh(id) => {
                // Re-walk the invalidated dir. `store_children` evicts the old
                // subtree and re-allocates child ids, so the surviving open set
                // is reconciled by path in the `Loaded` handler below (which
                // this expand will trigger).
                let id = *id;
                self.tree.update(cx, |t, cx| t.expand(id, cx));
            }
            FileTreeEvent::WatchError(msg) => {
                tracing::warn!(target: "oximux_app::file_tree_view", "watch error: {msg}");
            }
        }
    }

    /// Re-resolve the user's open set by path after a directory's children
    /// are (re)loaded. A refresh re-allocates child `TreeNodeId`s, so:
    ///   1. drop ids that no longer resolve (their subtree was evicted), then
    ///   2. for each freshly-loaded dir whose path the user had open, re-insert
    ///      its new id and re-expand it if unloaded — which cascades the same
    ///      reconcile to its own children one level deeper.
    fn reconcile_expanded(&mut self, children: &[FileTreeNode], cx: &mut Context<Self>) {
        {
            let tree = self.tree.read(cx);
            self.expanded_ids.retain(|id| tree.node(*id).is_some());
        }
        let mut to_expand: Vec<TreeNodeId> = Vec::new();
        for child in children {
            if child.is_dir && self.expanded_paths.contains(&child.path) {
                self.expanded_ids.insert(child.id);
                if !child.loaded {
                    to_expand.push(child.id);
                }
            }
        }
        for id in to_expand {
            self.tree.update(cx, |t, cx| t.expand(id, cx));
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let tree_handle = self.tree.clone();
        let tree = tree_handle.read(cx);
        let root_id = tree.root_id();
        self.rows = build_display_rows(root_id, &|id| tree.node(id).cloned(), &self.expanded_ids);
    }

    fn handle_dir_click(&mut self, id: TreeNodeId, loaded: bool, cx: &mut Context<Self>) {
        // Mirror the toggle into the path-keyed set so the open state survives
        // a refresh that re-allocates this node's id. Descendant paths are left
        // intact on collapse so re-expanding restores the prior subtree (the
        // same behavior the id-keyed set gave).
        let dir_path = self.tree.read(cx).node(id).map(|n| n.path.clone());
        if self.expanded_ids.contains(&id) {
            self.expanded_ids.remove(&id);
            if let Some(path) = &dir_path {
                self.expanded_paths.remove(path);
            }
            // Clear `selected` if it pointed inside the subtree we just
            // collapsed — otherwise the highlight returns when the dir is
            // re-expanded, which is surprising. We can't cheaply tell if
            // the prior selection was a descendant, so the safe default is
            // to keep selection on the collapsing dir itself.
            self.selected = Some(id);
            self.rebuild_rows(cx);
        } else {
            self.expanded_ids.insert(id);
            if let Some(path) = dir_path {
                self.expanded_paths.insert(path);
            }
            self.selected = Some(id);
            if !loaded {
                // Walker will fire `Loaded` later; rebuild now so the
                // placeholder appears immediately under the chevron.
                self.tree.update(cx, |t, cx| t.expand(id, cx));
            }
            self.rebuild_rows(cx);
        }
        cx.notify();
    }

    fn handle_file_click(
        &mut self,
        id: TreeNodeId,
        path: PathBuf,
        preview: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(id);
        (self.on_open)(path, preview, window, cx);
        cx.notify();
    }
}

/// Pure helper: walks the `FileTree` node graph plus the view's expanded
/// set and produces a flat list of `DisplayRow`s. Extracted as a free fn
/// so unit tests can exercise it without a GPUI runtime — the `nodes`
/// callback abstracts `FileTree::node`.
pub fn build_display_rows(
    root_id: TreeNodeId,
    nodes: &impl Fn(TreeNodeId) -> Option<FileTreeNode>,
    expanded: &HashSet<TreeNodeId>,
) -> Vec<DisplayRow> {
    let mut out = Vec::new();
    build_rows_recursive(root_id, nodes, expanded, 0, &mut out);
    out
}

fn build_rows_recursive(
    id: TreeNodeId,
    nodes: &impl Fn(TreeNodeId) -> Option<FileTreeNode>,
    expanded: &HashSet<TreeNodeId>,
    depth: usize,
    out: &mut Vec<DisplayRow>,
) {
    let Some(node) = nodes(id) else {
        return;
    };
    let label = node
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| node.path.display().to_string());

    if node.is_dir {
        let is_expanded = expanded.contains(&id);
        out.push(DisplayRow {
            label,
            depth,
            kind: RowKind::Dir {
                id,
                loaded: node.loaded,
                expanded: is_expanded,
            },
            path: node.path.clone(),
        });
        if is_expanded {
            if !node.loaded {
                out.push(DisplayRow {
                    label: "…".into(),
                    depth: depth + 1,
                    kind: RowKind::Placeholder { parent_id: id },
                    path: node.path.clone(),
                });
            } else if node.children.is_empty() {
                out.push(DisplayRow {
                    label: "(empty)".into(),
                    depth: depth + 1,
                    kind: RowKind::EmptyDir { parent_id: id },
                    path: node.path.clone(),
                });
            } else {
                for &child_id in &node.children {
                    build_rows_recursive(child_id, nodes, expanded, depth + 1, out);
                }
            }
        }
    } else {
        out.push(DisplayRow {
            label,
            depth,
            kind: RowKind::File { id },
            path: node.path.clone(),
        });
    }
}

// Visual palette — kept local so the spike does not depend on the
// workspace `Theme` registry. Hex values mirror the dark cockpit
// background used elsewhere; swap for real theme tokens when this
// view is mounted into the workspace shell.

/// File-tree row height. Intentionally 2px tighter than
/// `density.h_row = 24` so the explorer can fit more rows in the same
/// vertical space — file-tree navigation is scan-heavy and benefits
/// from compact rows; documented locally instead of adding a global
/// `density.h_row_compact` token until a second compact surface appears.
const FILE_TREE_ROW_H: f32 = 22.0;
const INDENT_PX: f32 = 14.0;
const CHEVRON_COL: f32 = 14.0;
const ICON_COL: f32 = 16.0;
const TITLEBAR_PAD: f32 = 30.0;

impl Render for FileTreeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        // Theme / typography are resolved per-render and threaded into
        // render_row so the closure captures stable Copy values rather
        // than re-instantiating per row.
        let theme = self.theme;
        // Resolved from the appearance rather than the constant: this view
        // keeps no typography of its own to sync, so the zoom has to reach it
        // here or the file tree stays at 100% while everything beside it moves.
        let typography = oximux_settings::appearance::typography(cx);
        let density = oximux_settings::appearance::density(cx);
        // Resolve the focused-editor file path once per render. Cached
        // into the uniform_list closure so every row's match check is a
        // cheap `==` instead of an Arc call per row.
        let active_path = self.on_query_active_path.as_ref().and_then(|q| q(cx));
        let list = uniform_list(
            "file-tree-view",
            count,
            cx.processor({
                let active_path = active_path.clone();
                let typography = typography.clone();
                move |me: &mut Self,
                      range: std::ops::Range<usize>,
                      _window: &mut Window,
                      cx: &mut Context<Self>| {
                    let rows = me.rows.clone();
                    let selected = me.selected;
                    let active_path = active_path.clone();
                    let typography = typography.clone();
                    range
                        .map(|i| {
                            render_row(
                                rows[i].clone(),
                                selected,
                                active_path.as_deref(),
                                theme,
                                density,
                                &typography,
                                cx,
                            )
                            .into_any_element()
                        })
                        .collect::<Vec<AnyElement>>()
                }
            }),
        )
        .track_scroll(&self.scroll)
        .h_full()
        .w_full();

        div()
            .flex()
            .flex_col()
            .h_full()
            .w_full()
            .bg(theme.bg_panel)
            .text_color(theme.fg_base)
            .text_size(px(typography.t_body_sm))
            // Top padding clears the transparent titlebar (traffic lights)
            // so the root row is not hidden behind the window controls.
            .pt(px(TITLEBAR_PAD))
            .pb(px(6.0))
            .child(list)
    }
}

fn render_row(
    row: DisplayRow,
    selected: Option<TreeNodeId>,
    active_path: Option<&std::path::Path>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut Context<FileTreeView>,
) -> impl IntoElement {
    let indent = px(row.depth as f32 * INDENT_PX);
    let chevron = match &row.kind {
        RowKind::Dir { expanded: true, .. } => "▾",
        RowKind::Dir {
            expanded: false, ..
        } => "▸",
        _ => " ",
    };
    let icon = match &row.kind {
        RowKind::Dir { .. } => "📁",
        RowKind::File { .. } => "📄",
        RowKind::Placeholder { .. } | RowKind::EmptyDir { .. } => " ",
    };
    let kind = row.kind.clone();
    let path = row.path.clone();
    let is_selected = match &row.kind {
        RowKind::Dir { id, .. } | RowKind::File { id } => Some(*id) == selected,
        _ => false,
    };
    // Highlight the row corresponding to the currently-active editor
    // (separate from click-selection). For directories the comparison
    // is meaningless (active files are files), so this only fires on
    // File rows.
    let is_active = matches!(&row.kind, RowKind::File { .. })
        && active_path
            .map(|p| p == row.path.as_path())
            .unwrap_or(false);
    let is_sentinel = matches!(
        &row.kind,
        RowKind::Placeholder { .. } | RowKind::EmptyDir { .. }
    );

    // Stable id per row enables `.hover(...)` interactivity. Use depth +
    // path so revealing/hiding the same path during expand/collapse
    // doesn't collide with the previous row's identity.
    let row_id = ElementId::Name(format!("ft-row-{}-{}", row.depth, row.path.display()).into());

    let chevron_el = div()
        .w(px(CHEVRON_COL))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(chevron);

    let icon_el = div()
        .w(px(ICON_COL))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(typography.t_body_sm))
        .child(icon);

    let label_el = div()
        .flex_1()
        .overflow_hidden()
        .when(is_sentinel, |s| s.text_color(theme.fg_subtle).italic())
        .child(row.label.clone());

    // Row chrome states (mirrors the convention used in source-control rows):
    //   - idle           → transparent (no bg)
    //   - hover          → hover_overlay (alpha-white transient lift)
    //   - selected only  → bg_panel_alt + 1px border_active hairline so the
    //                      click feedback survives without overpainting hover
    //   - active editor  → theme.selection (the same blue used for text
    //                      selection — semantically "this is what you're
    //                      focused on") + fg_base for the row text
    let mut el = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(FILE_TREE_ROW_H))
        .mx(px(4.0))
        .pl(indent)
        .pr(px(6.0))
        .rounded(px(density.r_xs))
        .when(is_active, |s| {
            s.bg(theme.selection).text_color(theme.fg_base)
        })
        .when(is_selected && !is_active, |s| {
            s.bg(theme.bg_panel_alt)
                .border_1()
                .border_color(theme.border_active)
        })
        .when(!is_sentinel && !is_active && !is_selected, |s| {
            s.hover(|h| h.bg(theme.hover_overlay))
        })
        .child(chevron_el)
        .child(icon_el)
        .child(label_el);

    // Only `Dir` and `File` rows are clickable; sentinels are inert.
    match kind {
        RowKind::Dir { id, loaded, .. } => {
            el = el.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |me, _: &MouseDownEvent, _window, cx| {
                    me.handle_dir_click(id, loaded, cx);
                }),
            );
            // Right-click — dispatch a workspace-level action carrying
            // the clicked dir path so `WorkspaceRoot` can open the shared
            // FileTreeContextMenu (reduced item set for directories).
            let ctx_path = path.clone();
            el = el.on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::actions::OpenFileTreeContextMenuAt {
                            x: ev.position.x.into(),
                            y: ev.position.y.into(),
                            path: ctx_path.to_string_lossy().into_owned(),
                            is_dir: true,
                        }),
                        cx,
                    );
                    cx.stop_propagation();
                },
            );
        }
        RowKind::File { id } => {
            let click_path = path.clone();
            el = el.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |me, ev: &MouseDownEvent, window, cx| {
                    // Single-click opens a reusable preview tab; double-click
                    // (or more) opens it as a permanent tab.
                    let preview = ev.click_count < 2;
                    me.handle_file_click(id, click_path.clone(), preview, window, cx);
                }),
            );
            // Drag-from-sidebar wiring. GPUI's drag activation distance
            // means small motions still trigger the click handler above,
            // so click-to-open is unaffected. Non-text rows
            // (binaries, system metadata) are gated below so a stray
            // drag from `.DS_Store` doesn't promote it to an editor tab.
            if is_openable_text_file(&path) {
                let drag_path = path.clone();
                let drag_label = SharedString::from(row.label.clone());
                el = el.on_drag(
                    FilePathDragPayload { path: drag_path },
                    move |_payload, _offset, _window, cx| {
                        let label = drag_label.clone();
                        cx.new(|_| FilePathDragPreview::new(label))
                    },
                );
            }
            // Right-click — files get the full menu (Open / Open to the
            // Side / Copy Path / Copy Relative Path / Reveal in Finder).
            let ctx_path = path.clone();
            el = el.on_mouse_down(
                MouseButton::Right,
                move |ev: &MouseDownEvent, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::actions::OpenFileTreeContextMenuAt {
                            x: ev.position.x.into(),
                            y: ev.position.y.into(),
                            path: ctx_path.to_string_lossy().into_owned(),
                            is_dir: false,
                        }),
                        cx,
                    );
                    cx.stop_propagation();
                },
            );
        }
        RowKind::Placeholder { .. } | RowKind::EmptyDir { .. } => {}
    }
    el
}
