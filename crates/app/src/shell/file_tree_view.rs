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
    Styled, Subscription, UniformListScrollHandle, Window, div, prelude::FluentBuilder, px, rgb,
    uniform_list,
};
use oximux_editor::{FileTree, FileTreeEvent, FileTreeNode, TreeNodeId};
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
pub type OnOpenFile = Arc<dyn Fn(PathBuf, &mut Window, &mut App) + 'static>;

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
    selected: Option<TreeNodeId>,
    scroll: UniformListScrollHandle,
    on_open: OnOpenFile,
    /// Optional pulse from the host on every render: "what file is the
    /// focused editor showing right now?" Used to highlight the matching
    /// row separately from the click-selection. `None` until the host
    /// provides a query.
    on_query_active_path: Option<OnQueryActivePath>,
    _sub: Subscription,
}

impl FileTreeView {
    pub fn new(
        tree: Entity<FileTree>,
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
        let mut expanded_ids = HashSet::new();
        expanded_ids.insert(root_id);

        // Kick off root walk; result arrives async as `FileTreeEvent::Loaded`.
        tree.update(cx, |t, cx| t.expand(root_id, cx));

        Self {
            tree,
            rows: Vec::new(),
            expanded_ids,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            on_open,
            on_query_active_path,
            _sub: sub,
        }
    }

    fn handle_tree_event(&mut self, ev: &FileTreeEvent, cx: &mut Context<Self>) {
        match ev {
            FileTreeEvent::Loaded { .. } => {
                self.rebuild_rows(cx);
                cx.notify();
            }
            FileTreeEvent::Refresh(id) => {
                // TODO(step-5): after Refresh, step-3's `remove_subtree` evicts
                // and reallocates child `TreeNodeId`s — any stale ids still in
                // `expanded_ids` will be ignored by `build_rows_recursive`
                // (nodes(id) → None → silent), causing previously-expanded
                // subdirs to collapse. Fix when step 5 wires the real workspace:
                //   self.expanded_ids
                //       .retain(|id| self.tree.read(cx).node(*id).is_some());
                let id = *id;
                self.tree.update(cx, |t, cx| t.expand(id, cx));
            }
            FileTreeEvent::WatchError(msg) => {
                tracing::warn!(target: "oximux_app::file_tree_view", "watch error: {msg}");
            }
        }
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let tree_handle = self.tree.clone();
        let tree = tree_handle.read(cx);
        let root_id = tree.root_id();
        self.rows = build_display_rows(root_id, &|id| tree.node(id).cloned(), &self.expanded_ids);
    }

    fn handle_dir_click(&mut self, id: TreeNodeId, loaded: bool, cx: &mut Context<Self>) {
        if self.expanded_ids.contains(&id) {
            self.expanded_ids.remove(&id);
            // Clear `selected` if it pointed inside the subtree we just
            // collapsed — otherwise the highlight returns when the dir is
            // re-expanded, which is surprising. We can't cheaply tell if
            // the prior selection was a descendant, so the safe default is
            // to keep selection on the collapsing dir itself.
            self.selected = Some(id);
            self.rebuild_rows(cx);
        } else {
            self.expanded_ids.insert(id);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(id);
        (self.on_open)(path, window, cx);
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
const ROW_HEIGHT: f32 = 22.0;
const INDENT_PX: f32 = 14.0;
const CHEVRON_COL: f32 = 14.0;
const ICON_COL: f32 = 16.0;
const TITLEBAR_PAD: f32 = 30.0;

const BG_PANEL: u32 = 0x141518;
const FG_DEFAULT: u32 = 0xCBD0D8;
const FG_DIM: u32 = 0x6A707A;
const HOVER_BG: u32 = 0x22262C;
const SELECT_BG: u32 = 0x2E343D;
/// Active-file highlight: distinct from click-selection so the user can
/// see both signals at once (e.g. the active file is `main.rs` but they
/// just clicked `lib.rs` to preview). Darker accent + brighter foreground.
const ACTIVE_BG: u32 = 0x1F3340;
const ACTIVE_FG: u32 = 0xE2E8F0;

impl Render for FileTreeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.rows.len();
        // Resolve the focused-editor file path once per render. Cached
        // into the uniform_list closure so every row's match check is a
        // cheap `==` instead of an Arc call per row.
        let active_path = self.on_query_active_path.as_ref().and_then(|q| q(cx));
        let list = uniform_list(
            "file-tree-view",
            count,
            cx.processor({
                let active_path = active_path.clone();
                move |me: &mut Self,
                      range: std::ops::Range<usize>,
                      _window: &mut Window,
                      cx: &mut Context<Self>| {
                    let rows = me.rows.clone();
                    let selected = me.selected;
                    let active_path = active_path.clone();
                    range
                        .map(|i| {
                            render_row(rows[i].clone(), selected, active_path.as_deref(), cx)
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
            .bg(rgb(BG_PANEL))
            .text_color(rgb(FG_DEFAULT))
            .text_size(px(12.0))
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
        && active_path.map(|p| p == row.path.as_path()).unwrap_or(false);
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
        .text_size(px(9.0))
        .text_color(rgb(FG_DIM))
        .child(chevron);

    let icon_el = div()
        .w(px(ICON_COL))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.0))
        .child(icon);

    let label_el = div()
        .flex_1()
        .overflow_hidden()
        .when(is_sentinel, |s| s.text_color(rgb(FG_DIM)).italic())
        .child(row.label.clone());

    let mut el = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(ROW_HEIGHT))
        .mx(px(4.0))
        .pl(indent)
        .pr(px(6.0))
        .rounded(px(4.0))
        .when(is_active, |s| s.bg(rgb(ACTIVE_BG)).text_color(rgb(ACTIVE_FG)))
        .when(is_selected && !is_active, |s| s.bg(rgb(SELECT_BG)))
        .when(!is_sentinel, |s| s.hover(|h| h.bg(rgb(HOVER_BG))))
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
                cx.listener(move |me, _: &MouseDownEvent, window, cx| {
                    me.handle_file_click(id, click_path.clone(), window, cx);
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
                    FilePathDragPayload {
                        path: drag_path,
                    },
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
