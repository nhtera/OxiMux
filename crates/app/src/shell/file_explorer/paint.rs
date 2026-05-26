//! Row painting for the file explorer.
//!
//! `paint_row` converts a `RowPlan` + click metadata into a GPUI element with
//! an attached `on_mouse_down` listener that dispatches back to `FileExplorer`.
//! Lucide SVG icons, generous row spacing, right-aligned status badge or
//! ignored-slash decoration, rounded subtle hover/selection background.

use crate::shell::file_explorer::FileExplorer;
use crate::shell::file_explorer::file_icon::icon_for_name;
use crate::shell::file_explorer::row_render::{NodeIcon, RowPlan};
use crate::shell::file_explorer::status_display::BadgeStatus;
use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    div, prelude::FluentBuilder, px, svg,
};
use gpui_component::{Icon, IconName};
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;

/// Style + layout tokens threaded into `paint_row` to avoid exceeding the
/// 7-argument limit imposed by clippy's `too_many_arguments` lint.
pub struct PaintCtx<'a> {
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
}

/// Paint one explorer row. Click events dispatch to the entity via
/// `cx.listener` so `toggle_dir` / `open_file` have full entity access.
///
/// `is_loading` — when `true` for a directory, appends "…" after the name to
/// signal that children are being fetched.
pub fn paint_row(
    plan: RowPlan,
    ctx: &PaintCtx<'_>,
    path: PathBuf,
    is_dir: bool,
    is_loading: bool,
    cx: &mut Context<FileExplorer>,
) -> impl IntoElement {
    // Indent: 12px per depth level + 6px base padding. The row itself stays
    // transparent so the highlight pill (drawn inset by 4px) gives the
    // selected/hover state rounded edges instead of edge-to-edge fill.
    let indent_px = plan.depth as f32 * 12.0 + 6.0;
    let fg = if plan.italic_dim {
        ctx.theme.fg_subtle
    } else {
        ctx.theme.fg_base
    };
    let icon_color = ctx.theme.fg_muted;

    // Lucide chevron — `chevron-right` for closed dirs, `chevron-down` for open.
    // Files get an empty 12px spacer so names line up.
    let chevron_el: gpui::AnyElement = match plan.icon {
        NodeIcon::FolderClosed => Icon::new(IconName::ChevronRight)
            .size_3()
            .text_color(icon_color)
            .into_any_element(),
        NodeIcon::FolderOpen => Icon::new(IconName::ChevronDown)
            .size_3()
            .text_color(icon_color)
            .into_any_element(),
        NodeIcon::File => div().w(px(12.0)).into_any_element(),
    };

    // Lucide folder/file glyph. Files route through the per-name lookup so
    // recognizable types (Cargo.*, *.rs, *.xml, ...) get a distinguishing
    // glyph; unknown files fall back to the upstream generic file icon.
    let node_icon_el: gpui::AnyElement = match plan.icon {
        NodeIcon::FolderClosed => Icon::new(IconName::Folder)
            .size_4()
            .text_color(icon_color)
            .into_any_element(),
        NodeIcon::FolderOpen => Icon::new(IconName::FolderOpen)
            .size_4()
            .text_color(icon_color)
            .into_any_element(),
        NodeIcon::File => match icon_for_name(&plan.name) {
            Some(path) => svg()
                .path(path)
                .size(px(16.0))
                .text_color(icon_color)
                .into_any_element(),
            None => Icon::new(IconName::File)
                .size_4()
                .text_color(icon_color)
                .into_any_element(),
        },
    };

    let display_name = if is_dir && is_loading {
        format!("{}…", plan.name)
    } else {
        plan.name.clone()
    };

    let badge_color = plan.badge.map(|(status, _)| match status {
        BadgeStatus::Modified => ctx.theme.git.modified,
        BadgeStatus::Added => ctx.theme.git.added,
        BadgeStatus::Deleted => ctx.theme.git.deleted,
        BadgeStatus::Renamed => ctx.theme.git.renamed,
        BadgeStatus::Untracked => ctx.theme.git.untracked,
        BadgeStatus::Copied => ctx.theme.git.copied,
        BadgeStatus::Ignored => ctx.theme.git.ignored,
    });
    let badge_label = plan.badge.map(|(_, l)| l);

    let click_path = path.clone();
    // Stable id per row — required for `.hover()` interactivity in GPUI.
    let row_id = gpui::ElementId::Name(format!("fe-row-{}", path.display()).into());
    let hover_bg = ctx.theme.bg_panel_alt;
    let selection_bg = ctx.theme.selection;

    // Right-click dispatches the shared `FileTreeContextMenu` action.
    // Captured separately from the left-click closure so each handler
    // owns its own clone of the path (avoids the borrow gymnastics of
    // sharing a single capture across both).
    let ctx_path = path.clone();
    let mut row = div()
        .id(row_id)
        .flex()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(26.0))
        .mx(px(4.0))
        .pl(px(indent_px))
        .pr(px(8.0))
        .rounded(px(4.0))
        .text_size(px(ctx.typography.t_body_sm))
        .text_color(fg)
        .when(plan.selected, |s| s.bg(selection_bg))
        .hover(|s| s.bg(hover_bg))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |me, _: &MouseDownEvent, window, cx| {
                if is_dir {
                    me.toggle_dir(click_path.clone(), cx);
                } else {
                    me.selected = Some(click_path.clone());
                    // Route through the host callback so the file opens
                    // as an editor tab in the active pane group, not via
                    // macOS `open` (which launched the file outside the
                    // cockpit). Test contexts pass `on_open=None`, in
                    // which case this silently no-ops.
                    me.open_file(click_path.clone(), window, cx);
                    cx.notify();
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            move |ev: &MouseDownEvent, window, cx| {
                window.dispatch_action(
                    Box::new(crate::actions::OpenFileTreeContextMenuAt {
                        x: ev.position.x.into(),
                        y: ev.position.y.into(),
                        path: ctx_path.to_string_lossy().into_owned(),
                        is_dir,
                    }),
                    cx,
                );
                cx.stop_propagation();
            },
        )
        .child(chevron_el)
        .child(node_icon_el)
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .when(plan.italic_dim, |s| s.italic())
                .child(display_name),
        );

    if let (Some(color), Some(label)) = (badge_color, badge_label) {
        // Right-aligned single-letter status badge. Small + semibold to mirror
        // the worktree-decoration treatment used elsewhere in the tree.
        row = row.child(
            div()
                .ml_auto()
                .flex_shrink_0()
                .pl(px(8.0))
                .text_color(color)
                .text_size(px(ctx.typography.t_label_xs))
                .font_weight(ctx.typography.w_semibold)
                .child(label),
        );
    } else if plan.italic_dim {
        // Ignored entry — show a circle-slash glyph at the right edge instead
        // of a letter badge, mirroring the worktree-decoration treatment.
        row = row.child(
            div().ml_auto().flex_shrink_0().pl(px(8.0)).child(
                svg()
                    .path("icons/circle-slash.svg")
                    .size(px(12.0))
                    .text_color(ctx.theme.fg_subtle),
            ),
        );
    }

    row
}
