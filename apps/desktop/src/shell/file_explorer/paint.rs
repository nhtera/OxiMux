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
    Context, Entity, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Styled, div, prelude::FluentBuilder, px, svg,
};
use gpui_component::{
    Icon, IconName, Sizable as _,
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};
use std::path::PathBuf;

/// Style + layout tokens threaded into `paint_row` to avoid exceeding the
/// 7-argument limit imposed by clippy's `too_many_arguments` lint.
pub struct PaintCtx<'a> {
    pub theme: Theme,
    pub density: Density,
    pub typography: &'a Typography,
}

/// Which inline editor (if any) a row should mount. Both variants carry the
/// same `InputState`; they differ only in what Escape cancels — an in-place
/// rename of an existing entry, or naming a not-yet-created file/folder.
pub enum InlineEdit {
    Rename(Entity<InputState>),
    Create(Entity<InputState>),
}

impl InlineEdit {
    fn input(&self) -> &Entity<InputState> {
        match self {
            InlineEdit::Rename(i) | InlineEdit::Create(i) => i,
        }
    }
}

/// Paint one explorer row. Click events dispatch to the entity via
/// `cx.listener` so `toggle_dir` / `open_file` have full entity access.
///
/// `is_loading` — when `true` for a directory, appends "…" after the name to
/// signal that children are being fetched.
///
/// `rename_input` — when `Some`, the label is replaced by an editable
/// `Input` bound to that `InputState`. Driven by `FileExplorer::renaming`.
/// The row also installs a key handler that converts Escape into a
/// cancel (Enter is handled via the input's `PressEnter` subscription
/// inside `rename_ops::start_rename`).
#[allow(clippy::too_many_arguments)]
pub fn paint_row(
    plan: RowPlan,
    ctx: &PaintCtx<'_>,
    path: PathBuf,
    is_dir: bool,
    is_loading: bool,
    inline: Option<InlineEdit>,
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
    let hover_bg = ctx.theme.hover_overlay;
    let selection_bg = ctx.theme.selection;

    // Right-click dispatches the shared `FileTreeContextMenu` action.
    // Captured separately from the left-click closure so each handler
    // owns its own clone of the path (avoids the borrow gymnastics of
    // sharing a single capture across both).
    let ctx_path = path.clone();
    let is_editing = inline.is_some();
    // Escape cancels the right inline editor: a not-yet-created entry's name
    // field discards the create; an existing row's field reverts the rename.
    let is_create = matches!(inline, Some(InlineEdit::Create(_)));
    // Container row. When editing, suppress hover/selection backgrounds
    // and skip both click handlers so the Input owns all pointer events.
    // The Escape key handler is hung on this row (not the Input) because
    // we need to intercept the keystroke whether the input has focus or
    // not — gpui-component's InputState propagates Escape after handling
    // it, so the event still bubbles up to us.
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
        .rounded(px(ctx.density.r_xs))
        .text_size(px(ctx.typography.t_body_sm))
        .text_color(fg);
    if !is_editing {
        row = row
            .when(plan.selected, |s| s.bg(selection_bg))
            .hover(|s| s.bg(hover_bg))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |me, ev: &MouseDownEvent, window, cx| {
                    // Clicking a different row while an inline rename or create
                    // is in flight discards it (user-typed-then-clicked-
                    // elsewhere = "discard"; Enter is the explicit commit).
                    me.cancel_rename(cx);
                    me.cancel_create(cx);
                    // Double-click on a file triggers inline rename. The 1st
                    // click of the double already ran the single-click branch
                    // (opening the reusable preview tab); the 2nd click
                    // promotes to rename. Promoting a preview tab to permanent
                    // is done by double-clicking the tab chip itself, not the
                    // explorer row. Directories keep toggle semantics.
                    if !is_dir && ev.click_count >= 2 {
                        me.start_rename(click_path.clone(), window, cx);
                        return;
                    }
                    // Select the clicked row first (both dirs and files) so the
                    // row-scoped keyboard shortcuts target what was just
                    // clicked, not a stale prior selection.
                    me.selected = Some(click_path.clone());
                    if is_dir {
                        me.toggle_dir(click_path.clone(), cx);
                    } else {
                        // Route through the host callback so the file opens
                        // as an editor tab in the active pane group, not via
                        // macOS `open` (which launched the file outside the
                        // cockpit). Test contexts pass `on_open=None`, in
                        // which case this silently no-ops.
                        me.open_file(click_path.clone(), window, cx);
                        cx.notify();
                    }
                    // Keep keyboard focus in the panel (opening a file focuses
                    // the editor) so the row-scoped shortcuts — Enter,
                    // Cmd+Backspace, Cmd+Alt+C — land here, not in the editor.
                    me.focus_panel(window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |me, ev: &MouseDownEvent, window, cx| {
                    // Right-click on a different row also discards
                    // the in-flight rename — the user is starting a
                    // new context-menu interaction somewhere else.
                    me.cancel_rename(cx);
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
                }),
            );
    } else {
        // Escape → cancel the inline edit. The input itself does
        // `cx.propagate()` after its own Escape handler, so the event bubbles
        // up to this row's listener even though the Input has key-focus.
        //
        // The mouse_down handlers (Left + Right) just swallow the event
        // so the background-click cancel handler in the FileExplorer
        // root doesn't fire when the user clicks within the editing
        // row (chevron / icon / input). The Input owns its own internal
        // click handling for cursor positioning; we never need to act
        // on a click within the editing row at the row level.
        row = row
            .on_key_down(cx.listener(move |me, ev: &KeyDownEvent, _window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    if is_create {
                        me.cancel_create(cx);
                    } else {
                        me.cancel_rename(cx);
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, |_ev: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
            })
            .on_mouse_down(MouseButton::Right, |_ev: &MouseDownEvent, _window, cx| {
                cx.stop_propagation();
            });
    }
    row = row.child(chevron_el).child(node_icon_el);

    // Label vs editable input. The label keeps its italic-dim styling
    // for ignored entries; the input uses the gpui-component default
    // styling so it visually pops as "editable".
    let name_cell: gpui::AnyElement = if let Some(edit) = &inline {
        // Size the inline input to the row's own font instead of the
        // gpui-component default (Medium → ~14px text, 32px tall), which
        // dwarfs the 11px rows. The widget derives its text size as
        // `size * 0.875`, so divide by 0.875 to land exactly on `t_body_sm`;
        // the custom-px size also drops it to the compact `h_6` height.
        let input_px = ctx.typography.t_body_sm / 0.875;
        div()
            .flex_1()
            .overflow_hidden()
            .child(Input::new(edit.input()).with_size(px(input_px)))
            .into_any_element()
    } else {
        div()
            .flex_1()
            .overflow_hidden()
            .when(plan.italic_dim, |s| s.italic())
            .child(display_name)
            .into_any_element()
    };
    row = row.child(name_cell);

    if is_editing {
        // Skip the right-aligned badge / ignored-slash glyph during an inline
        // edit so the input gets the full width of the row.
        return row;
    }

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
