//! Tree-view body rendering for the SCM panel.
//!
//! Consumed by `changed_files::section` when `rctx.view_mode == Tree`.
//! Owns the folder-row visual (chevron + folder icon + name + child-count +
//! rollup badge) and the leaf-row indent wrap; leaf content itself is
//! delegated to the existing `row_renderer::row` helper so flat and tree
//! modes paint identical leaf rows.
//!
//! Pure helpers — all stateful interactions plug into [`GitPanel`] via
//! `cx.listener`. Folder collapse-toggle is wired; folder hover-action
//! cluster and Cmd/Shift modifier handling are not yet implemented.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::changed_files::RenderCtx;
use crate::shell::git_panel::row_renderer::{RowKind, row};
use crate::shell::source_control::style as sc_style;
use crate::shell::source_control::tree::{NodeKind, NodeStatus, RenderRow, TreeSection};
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, px,
};
use gpui_component::{Icon, IconName};
use oximux_core::FileStatus;

/// Horizontal indent per tree depth level. 12 px / level, depth cap 8
/// (deeper paths still render — the cap just stops indenting further so
/// the panel doesn't horizontally clip on pathologically nested trees).
pub(crate) const TREE_INDENT_PX: f32 = 12.0;
const TREE_DEPTH_INDENT_CAP: u8 = 8;

/// Map a `RowKind` (the changed-files section discriminator) to the
/// matching `TreeSection` that `tree::build_tree` consumes. The two
/// enums are intentionally separate so the tree module stays
/// independent of the panel's per-row rendering taxonomy.
pub(super) fn section_for(kind: RowKind) -> TreeSection {
    match kind {
        RowKind::Staged => TreeSection::Staged,
        RowKind::Unstaged => TreeSection::Unstaged,
        RowKind::Untracked => TreeSection::Untracked,
    }
}

/// Render one tree row — either a folder or a leaf. Leaves delegate to
/// the existing `row()` helper (same badge / name / hover cluster as
/// flat mode); folders use the local `folder_row` helper. The outer
/// container wraps both with `pl(depth * TREE_INDENT_PX)` so the row
/// background extends full-width and only the inner content shifts —
/// matches the file-explorer tree visual.
pub(super) fn render_tree_row(
    flat_row: RenderRow,
    section_rows: &[&FileStatus],
    kind: RowKind,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let depth_px = depth_indent_px(flat_row.depth);
    match flat_row.kind {
        NodeKind::Dir => folder_row(flat_row, depth_px, rctx, cx),
        NodeKind::File => leaf_row(flat_row, section_rows, kind, depth_px, rctx, cx),
    }
}

fn leaf_row(
    flat_row: RenderRow,
    section_rows: &[&FileStatus],
    kind: RowKind,
    depth_px: f32,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    // Lookup the FileStatus by path. Linear scan — fine at SCM
    // working-tree sizes (10s–100s of files). A HashMap could replace
    // this if profiling ever surfaces a hot path on a 5k-file repo.
    let Some(file) = section_rows
        .iter()
        .copied()
        .find(|f| f.path == flat_row.path)
    else {
        // Should never happen: every RenderRow.path came from this
        // exact section_rows slice. Empty div instead of a panic so a
        // race condition (poll tick mid-render) degrades gracefully.
        return div().into_any_element();
    };
    div()
        .pl(px(depth_px))
        .child(row(file, kind, rctx, cx))
        .into_any_element()
}

fn folder_row(
    flat_row: RenderRow,
    depth_px: f32,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let folder_path = flat_row.path.clone();
    let is_collapsed = rctx.collapsed_dirs.contains(&folder_path);
    let chevron = if is_collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    let folder_icon = if is_collapsed {
        IconName::Folder
    } else {
        IconName::FolderOpen
    };
    let display_name = flat_row
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| flat_row.path.display().to_string());
    let theme = rctx.theme;
    let row_id = gpui::SharedString::from(format!("git-tree-dir-{}", flat_row.path.display()));
    let click_path = folder_path.clone();

    let mut content = div()
        .id(row_id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(rctx.density.h_tab))
        .pl(px(depth_px + sc_style::PAD_H))
        .pr(px(sc_style::PAD_H))
        .text_size(px(sc_style::TEXT))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, _: &MouseDownEvent, _window, cx| {
                panel.toggle_collapsed_dir(click_path.clone(), cx);
            }),
        )
        .child(Icon::new(chevron).size_3().text_color(theme.fg_subtle))
        .child(Icon::new(folder_icon).size_3().text_color(theme.fg_muted))
        .child(display_name);

    // Child-count chip + rollup status badge sit right-aligned. Chip
    // is always present (even at 1); rollup badge is suppressed when
    // every leaf is Deleted (matches the reference UX — `flat_row.rollup_status`
    // is None in that case).
    let count_chip = div()
        .ml_auto()
        .text_color(theme.fg_subtle)
        .child(format!("{}", flat_row.child_count));
    content = content.child(count_chip);
    if let Some(badge) = rollup_badge(flat_row.rollup_status, theme) {
        content = content.child(badge);
    }

    content.into_any_element()
}

fn rollup_badge(status: Option<NodeStatus>, theme: oximux_settings::Theme) -> Option<AnyElement> {
    let s = status?;
    let (text, colour) = match s {
        NodeStatus::Modified => ("M", theme.status_warning),
        NodeStatus::Added => ("A", theme.status_added),
        NodeStatus::Untracked => ("U", theme.git.untracked),
        NodeStatus::Renamed => ("R", theme.status_added),
        NodeStatus::Copied => ("C", theme.status_added),
        NodeStatus::Conflict => ("!", theme.status_error),
        // Deleted is filtered out by the tree's rollup pass — keep
        // the arm so the match is exhaustive and a future variant
        // doesn't silently fall through to a wrong colour.
        NodeStatus::Deleted => return None,
    };
    Some(
        div()
            .pl(px(6.0))
            .text_color(colour)
            .child(text.to_string())
            .into_any_element(),
    )
}

fn depth_indent_px(depth: u8) -> f32 {
    let capped = depth.min(TREE_DEPTH_INDENT_CAP);
    f32::from(capped) * TREE_INDENT_PX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_indent_caps_at_eight_levels() {
        assert_eq!(depth_indent_px(0), 0.0);
        assert_eq!(depth_indent_px(1), TREE_INDENT_PX);
        assert_eq!(depth_indent_px(7), 7.0 * TREE_INDENT_PX);
        assert_eq!(depth_indent_px(8), 8.0 * TREE_INDENT_PX);
        // Past the cap: still 8 levels of indent, never more.
        assert_eq!(depth_indent_px(12), 8.0 * TREE_INDENT_PX);
        assert_eq!(depth_indent_px(255), 8.0 * TREE_INDENT_PX);
    }

    #[test]
    fn section_for_maps_row_kind_to_tree_section() {
        assert_eq!(section_for(RowKind::Staged), TreeSection::Staged);
        assert_eq!(section_for(RowKind::Unstaged), TreeSection::Unstaged);
        assert_eq!(section_for(RowKind::Untracked), TreeSection::Untracked);
    }
}
