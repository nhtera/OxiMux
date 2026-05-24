//! Drag plumbing for file rows dragged out of the Files-tab sidebar.
//!
//! Mirrors `tab_drag.rs` but carries a filesystem path instead of a
//! `(group, tab)` reference. The sidebar row's `.on_drag` hands a
//! `FilePathDragPayload` to GPUI; matching
//! `.on_drag_move::<FilePathDragPayload>` / `.on_drop::<FilePathDragPayload>`
//! handlers on each pane body consume it. GPUI dispatches by TypeId so
//! this flow never cross-fires with `TabDragPayload`.

use std::path::PathBuf;

use gpui::{Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px, rgb};

/// Payload attached to an in-flight file-row drag. Carries the absolute
/// filesystem path so the drop site can open the file directly without
/// having to dereference back into the file tree.
#[derive(Clone, Debug)]
pub struct FilePathDragPayload {
    pub path: PathBuf,
}

/// Floating chip painted under the cursor while dragging a file row.
/// Uses hardcoded sidebar-palette colors (not the workspace `Theme`) so
/// the preview entity can be constructed from the file-tree without
/// having to thread a `Theme` parameter through `FileTreeView::new`.
pub struct FilePathDragPreview {
    label: SharedString,
}

impl FilePathDragPreview {
    pub fn new(label: SharedString) -> Self {
        Self { label }
    }
}

/// Sidebar-palette colors. Match `file_tree_view.rs`'s local consts so
/// the drag preview reads as part of the same surface.
const BG_PREVIEW: u32 = 0x22262C;
const BORDER_PREVIEW: u32 = 0x4B5563;
const FG_PREVIEW: u32 = 0xE2E8F0;

impl Render for FilePathDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .h(px(28.0))
            .px(px(12.0))
            .rounded(px(4.0))
            .bg(rgb(BG_PREVIEW))
            .border_1()
            .border_color(rgb(BORDER_PREVIEW))
            .text_size(px(11.0))
            .text_color(rgb(FG_PREVIEW))
            .shadow_md()
            .child(SharedString::from("\u{1F4C4} "))
            .child(self.label.clone())
    }
}
