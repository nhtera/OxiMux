//! Tree-view body rendering for the SCM panel.
//!
//! Consumed by `changed_files::section` when `rctx.view_mode == Tree`.
//! Owns the folder-row visual (chevron + folder icon + name + child-count +
//! rollup badge + hover-action cluster) and the leaf-row indent wrap;
//! leaf content itself is delegated to the existing `row_renderer::row`
//! helper so flat and tree modes paint identical leaf rows.
//!
//! Pure helpers — all stateful interactions plug into [`GitPanel`] via
//! `cx.listener`. Plain click toggles a folder's collapsed state;
//! Cmd+click collapses sibling folders (keeps the clicked one expanded);
//! Shift+click selects every leaf in the folder's subtree.

use crate::shell::git_panel::GitPanel;
use crate::shell::git_panel::changed_files::RenderCtx;
use crate::shell::git_panel::discard_confirm::DiscardAllArea;
use crate::shell::git_panel::row_renderer::{RowKind, row};
use crate::shell::source_control::style as sc_style;
use crate::shell::source_control::tree::{
    NodeKind, NodeStatus, RenderRow, TreeNode, TreeSection, sibling_dirs, subtree_leaves,
};
use gpui::{
    AnyElement, ClickEvent, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, div, px,
};
use gpui_component::{
    Disableable as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
};
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
///
/// `tree` is the freshly-built section tree; folder rows compute their
/// subtree leaves + sibling-dir list against it at render time so the
/// values can be captured into the 'static click-handler closures
/// (`section_rows` itself is only borrowed for the render pass).
pub(super) fn render_tree_row(
    flat_row: RenderRow,
    tree: &TreeNode,
    section_rows: &[&FileStatus],
    kind: RowKind,
    rctx: &RenderCtx<'_>,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let depth_px = depth_indent_px(flat_row.depth);
    match flat_row.kind {
        NodeKind::Dir => folder_row(flat_row, tree, kind, depth_px, rctx, cx),
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
    tree: &TreeNode,
    kind: RowKind,
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
    let row_id = SharedString::from(format!("git-tree-dir-{}", flat_row.path.display()));

    // Pre-compute the owned path lists the click / hover handlers
    // need: subtree leaves drive Stage all / Unstage all / Discard
    // all in folder and Shift+click subtree multi-select; sibling
    // dirs drive Cmd+click collapse-siblings.
    let subtree = subtree_leaves(tree, &flat_row.path);
    let siblings = sibling_dirs(tree, &flat_row.path);

    let click_path = folder_path.clone();
    let click_subtree = subtree.clone();
    let click_siblings = siblings.clone();
    let right_click_folder = folder_path.clone();
    let right_click_leaves = subtree.clone();
    let right_click_is_staged = matches!(kind, RowKind::Staged);
    let right_click_is_untracked = matches!(kind, RowKind::Untracked);

    // Hover cluster is suppressed when:
    // - filter is active (subtree may be a partial slice — the user
    //   filtered down and a "Stage all in folder" would silently act
    //   on the visible subset, which is exactly what they want, but
    //   the affordance reads as full-section, so we hide it for now);
    // - the folder has zero leaves (no-op state shouldn't render).
    //
    // TODO: filter-active suppression could be lifted once the
    // wording reads as "Stage these N filtered files"; for now match
    // the section-header policy that already hides on filter.
    let show_hover_cluster = !rctx.filter_active && !subtree.is_empty();

    let mut content = div()
        .id(row_id.clone())
        .group(row_id.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .h(px(rctx.density.h_tab))
        .pl(px(depth_px + sc_style::PAD_H))
        .pr(px(sc_style::PAD_H))
        .text_size(px(sc_style::BODY_TEXT))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |panel, ev: &MouseDownEvent, _window, cx| {
                if ev.modifiers.shift {
                    panel.select_paths_replace(click_subtree.clone(), cx);
                } else if ev.modifiers.platform {
                    panel.collapse_dirs(click_siblings.clone(), cx);
                } else {
                    panel.toggle_collapsed_dir(click_path.clone(), cx);
                }
            }),
        )
        // Right-click on a folder row — open `GitRowContextMenu`
        // in Folder scope over the pre-computed subtree leaves.
        // Same dispatch surface as the hover cluster (Stage all /
        // Discard all in folder) but reachable from any pointer
        // position.
        .on_mouse_down(
            MouseButton::Right,
            move |ev: &MouseDownEvent, window, cx| {
                let leaves: Vec<String> = right_click_leaves
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                let x: f32 = ev.position.x.into();
                let y: f32 = ev.position.y.into();
                window.dispatch_action(
                    Box::new(crate::actions::OpenGitRowContextMenuAt {
                        x,
                        y,
                        path: right_click_folder.to_string_lossy().into_owned(),
                        is_staged: right_click_is_staged,
                        is_untracked: right_click_is_untracked,
                        is_folder: true,
                        selection_paths: Vec::new(),
                        folder_leaves: leaves,
                    }),
                    cx,
                );
                cx.stop_propagation();
            },
        )
        .child(Icon::new(chevron).size_3().text_color(theme.fg_subtle))
        .child(Icon::new(folder_icon).size_3().text_color(theme.fg_muted))
        .child(display_name);

    // Child-count chip + rollup status badge sit right-aligned. Chip
    // is always present (even at 1); rollup badge is suppressed when
    // every leaf is Deleted (folder shows no badge in that case).
    let count_chip = div()
        .ml_auto()
        .text_color(theme.fg_subtle)
        .child(format!("{}", flat_row.child_count));
    content = content.child(count_chip);
    if let Some(badge) = rollup_badge(flat_row.rollup_status, theme) {
        content = content.child(badge);
    }

    if show_hover_cluster {
        let cluster = folder_hover_cluster(
            kind,
            subtree,
            row_id.clone(),
            theme,
            rctx.discard_pending,
            cx,
        );
        content = content.child(
            div()
                .ml(px(6.0))
                .invisible()
                .group_hover(row_id.clone(), |s| s.visible())
                // Eat MouseDown inside the cluster so the parent folder
                // row's MouseDown handler does NOT fire — without this,
                // clicking a cluster button (Stage all / Unstage all /
                // Discard all) would also toggle the folder's collapsed
                // state because GPUI fires `on_mouse_down` on the
                // parent before the Button's `on_click` (MouseUp) lands.
                .on_mouse_down(MouseButton::Left, |_ev, _win, cx| {
                    cx.stop_propagation();
                })
                .child(cluster),
        );
    }

    content.into_any_element()
}

/// Per-folder action cluster: same shape as the section-header cluster
/// (Stage / Unstage primary + Discard / Delete destructive). Routes
/// through the same `stage_paths_bulk` / `unstage_paths_bulk` /
/// `discard_area` methods so partial-stage semantics + the
/// type-to-confirm modal are identical to section-level actions.
///
/// `paths` is the folder's subtree leaves (pre-computed by the
/// caller — empty subtrees are suppressed earlier so we never render
/// a no-op cluster).
fn folder_hover_cluster(
    kind: RowKind,
    paths: Vec<std::path::PathBuf>,
    folder_row_id: SharedString,
    theme: oximux_settings::Theme,
    discard_pending: bool,
    cx: &mut Context<GitPanel>,
) -> AnyElement {
    let area = area_for(kind);
    let mut cluster = div().flex().flex_row().items_center().gap(px(2.0));

    let primary_paths = paths.clone();
    let primary_id = SharedString::from(format!("git-tree-dir-primary-{}", folder_row_id.as_ref()));
    cluster = cluster.child(match kind {
        RowKind::Unstaged | RowKind::Untracked => Button::new(primary_id)
            .ghost()
            .xsmall()
            .icon(
                Icon::default()
                    .path("icons/plus.svg")
                    .size(px(FOLDER_BTN_PX))
                    .text_color(theme.status_added),
            )
            .tooltip("Stage all in folder")
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.stage_paths_bulk(primary_paths.clone(), cx);
            }))
            .into_any_element(),
        RowKind::Staged => Button::new(primary_id)
            .ghost()
            .xsmall()
            .icon(
                Icon::default()
                    .path("icons/minus.svg")
                    .size(px(FOLDER_BTN_PX))
                    .text_color(theme.status_removed),
            )
            .tooltip("Unstage all in folder")
            .on_click(cx.listener(move |panel, _: &ClickEvent, _window, cx| {
                panel.unstage_paths_bulk(primary_paths.clone(), cx);
            }))
            .into_any_element(),
    });

    let destructive_paths = paths.clone();
    let destructive_id =
        SharedString::from(format!("git-tree-dir-destructive-{}", folder_row_id.as_ref()));
    let (dest_icon, dest_tooltip) = match area {
        DiscardAllArea::Untracked => ("icons/delete.svg", "Delete all in folder"),
        _ => ("icons/undo.svg", "Discard all in folder"),
    };
    let mut destructive_btn = Button::new(destructive_id)
        .ghost()
        .xsmall()
        .icon(
            Icon::default()
                .path(dest_icon)
                .size(px(FOLDER_BTN_PX))
                .text_color(theme.fg_muted),
        )
        .tooltip(dest_tooltip);
    if discard_pending {
        destructive_btn = destructive_btn.disabled(true);
    } else {
        destructive_btn = destructive_btn.on_click(cx.listener(
            move |panel, _: &ClickEvent, _window, cx| {
                panel.discard_area(area, destructive_paths.clone(), cx);
            },
        ));
    }
    cluster = cluster.child(destructive_btn);

    cluster.into_any_element()
}

/// Map a `RowKind` to the `DiscardAllArea` used by `discard_area`. Same
/// mapping as `changed_files::area_for`; kept private to this module so
/// the section-header and folder-hover clusters share intent without
/// either reaching across module boundaries.
fn area_for(kind: RowKind) -> DiscardAllArea {
    match kind {
        RowKind::Unstaged => DiscardAllArea::Unstaged,
        RowKind::Staged => DiscardAllArea::Staged,
        RowKind::Untracked => DiscardAllArea::Untracked,
    }
}

/// Folder hover-action button icon size — matches the section-header
/// `SECTION_BTN_PX` so the affordances at folder vs. section level read
/// as a single visual family.
const FOLDER_BTN_PX: f32 = 14.0;

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
