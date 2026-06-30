//! One issue/PR table row + column-width constants shared with the header.
//!
//! Data is pre-fetched into [`ForgeItem`]s before the `uniform_list` closure
//! builds these rows, so nothing here touches the network or does per-frame
//! work. Row actions (create workspace, open in browser) dispatch through plain
//! event closures capturing a `WeakEntity<WorkspaceRoot>` + the active project.
//!
//! Column layout (5 columns, single flex row):
//!   ID (fixed) · TITLE/CONTEXT (flex) · ASSIGNEES (fixed) · STATUS (fixed) · UPDATED (fixed)
//! The header row in `table.rs` uses the same `COL_*` constants so columns align.
//!
//! Kept whole despite running slightly over the file-size soft cap: every item
//! here is row-layout-scoped (column cells + the two action builders), and the
//! cells share enough local context that splitting them would add more import
//! surface than it removes.

use gpui::{
    AnyElement, App, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, WeakEntity, Window, div, px,
};
use oximux_core::Project;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::ForgeItem;
use crate::shell::open_url::open_url;
use crate::shell::session_merge::relative_age_compact;
use crate::shell::tasks_view::TaskKind;
use crate::workspace_root::WorkspaceRoot;

// ---------------------------------------------------------------------------
// Shared column widths — used by BOTH the header row (in mod.rs) and each
// data row here, so columns always align without any runtime coordination.
// ---------------------------------------------------------------------------

/// Fixed width for the `#ID` column (e.g. `#1234`).
pub(super) const COL_ID_W: f32 = 56.0;
/// Fixed width for the `ASSIGNEES` column.
pub(super) const COL_ASSIGNEES_W: f32 = 120.0;
/// Fixed width for the `STATUS` column (state chip).
pub(super) const COL_STATUS_W: f32 = 80.0;
/// Fixed width for the `UPDATED` column (relative age, e.g. `3d`).
pub(super) const COL_UPDATED_W: f32 = 90.0;
/// Fixed-width right-edge action cluster revealed on row hover.
pub(super) const COL_ACTIONS_W: f32 = 100.0;

/// Fixed row height for the virtualized list (single text line + padding).
pub(super) const TASK_ROW_HEIGHT: f32 = 36.0;

/// Color for a GitHub state string. `OPEN` reads as live (ok), `MERGED` as
/// informational, anything else (`CLOSED`) as muted. Case-insensitive so it
/// tolerates `open`/`OPEN` variants across `gh` versions.
pub(super) fn state_color(state: &str, theme: Theme) -> Hsla {
    match state.to_ascii_uppercase().as_str() {
        "OPEN" => theme.status_ok,
        "MERGED" => theme.status_info,
        _ => theme.fg_muted,
    }
}

/// The workspace name seeded from an issue/PR. `create_workspace_async` derives
/// the slug + `oximux/<slug>` branch from this, so it carries the number for a
/// recognizable branch like `oximux/issue-42-fix-crash`.
pub(super) fn workspace_name_for(kind: TaskKind, item: &ForgeItem) -> String {
    let prefix = match kind {
        TaskKind::Issues => "issue",
        TaskKind::Prs => "pr",
    };
    format!("{prefix} {} {}", item.number, item.title)
}

/// A small static rounded pill (status or label chip).
fn chip(text: String, fg: Hsla, bg: Hsla, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(bg)
        .text_size(px(typography.t_label_xs))
        .text_color(fg)
        .child(text)
        .into_any_element()
}

/// Build one issue/PR table row element.
///
/// Layout: five fixed-basis cells in a `flex_row`, matching the header columns:
///   `#ID` | `TITLE` (flex) | `ASSIGNEES` | `STATUS` | `UPDATED`
/// A right-edge action cluster (`↗`, `+ Workspace`) is appended and hidden
/// behind a hover-visible opacity trick (always rendered, zero-opacity at rest).
pub(super) fn render_task_row(
    item: &ForgeItem,
    kind: TaskKind,
    weak_root: WeakEntity<WorkspaceRoot>,
    project: Project,
    now: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    // Column: #ID
    let col_id = div()
        .flex_none()
        .w(px(COL_ID_W))
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .overflow_hidden()
        .whitespace_nowrap()
        .child(format!("#{}", item.number));

    // Column: TITLE/CONTEXT (flex) — title line + optional label sub-line
    let title_text = div()
        .flex_none()
        .w_full()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child(item.title.clone());

    // Sub-line: up to 2 label chips, clipped to the column width
    let mut sub_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .overflow_hidden();
    for label in item.labels.iter().take(2) {
        sub_row = sub_row.child(chip(
            label.name.clone(),
            theme.fg_muted,
            theme.bg_overlay,
            density,
            typography,
        ));
    }

    // The title is `whitespace_nowrap`; a nowrap text node placed DIRECTLY in a
    // flex-col renders blank (the nowrap node forces its full content width and
    // the column collapses it). Wrap it in a flex-row so it clips instead.
    let title_row = div()
        .flex()
        .flex_row()
        .w_full()
        .min_w_0()
        .child(title_text);

    let col_title = div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(title_row)
        .child(sub_row);

    // Column: ASSIGNEES — up to 2 `@login` entries, fixed width
    let mut assignees_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .overflow_hidden();
    for assignee in item.assignees.iter().take(2) {
        assignees_row = assignees_row.child(
            div()
                .flex_none()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .whitespace_nowrap()
                .child(format!("@{}", assignee.login)),
        );
    }
    let col_assignees = div()
        .flex_none()
        .w(px(COL_ASSIGNEES_W))
        .overflow_hidden()
        .child(assignees_row);

    // Column: STATUS — state chip (open/closed/merged)
    let state_chip = chip(
        item.state.to_ascii_lowercase(),
        state_color(&item.state, theme),
        theme.bg_overlay,
        density,
        typography,
    );
    let col_status = div()
        .flex_none()
        .w(px(COL_STATUS_W))
        .overflow_hidden()
        .child(state_chip);

    // Column: UPDATED — relative age ("3d", "2h", "now"). Falls back to a dash
    // when the source reported no timestamp (a forge listing that omits it) or
    // it's unparseable, so the column never renders an empty cell.
    let updated_label = {
        let rel = relative_age_compact(&item.updated_at, now);
        if rel.is_empty() {
            "\u{2014}".to_string()
        } else {
            rel
        }
    };
    let col_updated = div()
        .flex_none()
        .w(px(COL_UPDATED_W))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(updated_label);

    // Right-edge action cluster — always laid out (reserves its column), but
    // transparent at rest and faded in on row hover via the row's `.group("")`.
    let actions = div()
        .flex_none()
        .w(px(COL_ACTIONS_W))
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .opacity(0.0)
        .group_hover("", |s| s.opacity(1.0))
        .gap(px(density.gap_inline))
        .child(open_action(item.url.clone(), theme, density, typography))
        .child(create_action(
            workspace_name_for(kind, item),
            format!("#{}", item.number),
            weak_root,
            project,
            theme,
            density,
            typography,
        ));

    div()
        .group("")
        .flex()
        .flex_row()
        .items_center()
        .h(px(TASK_ROW_HEIGHT))
        .w_full()
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .child(col_id)
        .child(col_title)
        .child(col_assignees)
        .child(col_status)
        .child(col_updated)
        .child(actions)
        .into_any_element()
}

/// "Open in browser" action chip. Only `https://` URLs are forwarded
/// (the scheme guard lives in `open_url`).
fn open_action(url: String, theme: Theme, density: Density, typography: &Typography) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_overlay)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _w, _cx: &mut App| {
            open_url(&url);
        })
        .child("↗".to_string())
        .into_any_element()
}

/// "Create workspace from this" action chip → `create_workspace_async`, seeded
/// with the issue/PR reference (e.g. `"#42"`) and activating the new workspace.
fn create_action(
    name: String,
    linked_issue: String,
    weak_root: WeakEntity<WorkspaceRoot>,
    project: Project,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(5.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_overlay)
        .text_size(px(typography.t_label_xs))
        .text_color(theme.fg_base)
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                let _ = weak_root.update(cx, |root, cx| {
                    root.create_workspace_async(
                        project.clone(),
                        name.clone(),
                        None,
                        Some(linked_issue.clone()),
                        true,
                        window,
                        cx,
                    );
                });
            },
        )
        .child("+ Workspace".to_string())
        .into_any_element()
}
