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
use oximux_core::{AgentAdapter, Project};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::forge::ForgeItem;
use crate::shell::open_url::open_url;
use crate::shell::session_merge::relative_age_compact;
use crate::shell::tasks_view::{TaskKind, TaskRow, TasksView};
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

/// Title-case a forge state word (`OPEN` / `open` → `Open`). ASCII-only input
/// (the forge states), so byte-wise capitalization is safe.
pub(super) fn titlecase_state(state: &str) -> String {
    let mut chars = state.chars();
    match chars.next() {
        Some(first) => {
            let rest = chars.as_str().to_ascii_lowercase();
            format!("{}{rest}", first.to_ascii_uppercase())
        }
        None => String::new(),
    }
}

/// The status pill (`● Open` / `● Closed` / `● Merged`) — a dotted, title-cased
/// chip tinted by [`state_color`]. Shared by the list's STATUS column and the
/// detail header so the two read identically.
pub(super) fn state_pill(
    state: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .flex_none()
        .px(px(6.0))
        .rounded(px(density.r_chip))
        .bg(theme.bg_overlay)
        .text_size(px(typography.t_label_xs))
        .text_color(state_color(state, theme))
        .whitespace_nowrap()
        .child(format!("\u{25cf} {}", titlecase_state(state)))
        .into_any_element()
}

/// The workspace name seeded from an issue/PR. `create_workspace_async` derives
/// the slug + `oximux/<slug>` branch from this, so it carries the number for a
/// recognizable branch like `oximux/issue-42-fix-crash`. The title is trimmed
/// to a short lead (see [`short_issue_title`]) so a sentence-length issue title
/// doesn't yield an unwieldy branch + delete-confirm slug.
pub(super) fn workspace_name_for(kind: TaskKind, item: &ForgeItem) -> String {
    let prefix = match kind {
        TaskKind::Issues => "issue",
        TaskKind::Prs => "pr",
    };
    format!("{prefix} {} {}", item.number, short_issue_title(&item.title))
}

/// Keep only a short lead of an issue/PR title for the workspace name. A full
/// title can run to a sentence; the derived slug also gets a hard cap, but
/// trimming here keeps the human-readable name itself tidy. Breaks on a word
/// boundary within a small budget.
fn short_issue_title(title: &str) -> String {
    const MAX: usize = 28;
    let t = title.trim();
    if t.len() <= MAX {
        return t.to_string();
    }
    let mut end = MAX;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    let head = &t[..end];
    match head.rfind(char::is_whitespace) {
        Some(sp) if sp >= 8 => head[..sp].to_string(),
        _ => head.to_string(),
    }
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
/// Clicking the row (outside the action cluster, which stops propagation) opens
/// the issue/PR in the in-pane detail view via `weak_tasks`.
///
/// `project` is the issue/PR's *own* project (each row may belong to a
/// different one under the aggregate scope); `show_project` adds a small
/// project tag to the context sub-line so the source is legible when the list
/// spans projects.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_task_row(
    item: &ForgeItem,
    kind: TaskKind,
    weak_tasks: WeakEntity<TasksView>,
    weak_root: WeakEntity<WorkspaceRoot>,
    project: Project,
    show_project: bool,
    now: &str,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    // The row dispatches to its own project; capture before the action cluster
    // moves `project`.
    let click_project = project.clone();
    let project_name = project.name.clone();
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
        .text_ellipsis()
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child(item.title.clone());

    // Sub-line: author · optional project tag (aggregate scope) · up to 2 label
    // chips, clipped to the column width.
    let mut sub_row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .overflow_hidden();
    if !item.author.login.is_empty() {
        sub_row = sub_row.child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(item.author.login.clone()),
        );
    }
    if show_project {
        sub_row = sub_row.child(chip(
            project_name,
            theme.fg_subtle,
            theme.bg_panel_alt,
            density,
            typography,
        ));
    }
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
    // Em dash placeholder when nobody's assigned, so the column never reads as
    // an accidentally-blank cell.
    if item.assignees.is_empty() {
        assignees_row = assignees_row.child(
            div()
                .flex_none()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child("\u{2014}".to_string()),
        );
    }
    let col_assignees = div()
        .flex_none()
        .w(px(COL_ASSIGNEES_W))
        .overflow_hidden()
        .child(assignees_row);

    // Column: STATUS — dotted state pill (open/closed/merged)
    let col_status = div()
        .flex_none()
        .w(px(COL_STATUS_W))
        .overflow_hidden()
        .child(state_pill(&item.state, theme, density, typography));

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
            item.url.clone(),
            weak_root,
            project,
            theme,
            density,
            typography,
        ));

    // Clicking anywhere on the row (the action cluster stops propagation) opens
    // the issue/PR in the in-pane detail view.
    let click_item = item.clone();
    div()
        .group("")
        .flex()
        .flex_row()
        .items_center()
        .h(px(TASK_ROW_HEIGHT))
        .w_full()
        .px(px(density.pad_panel))
        .gap(px(density.gap_inline))
        .cursor_pointer()
        // Subtle row highlight on hover — interactive feedback that the whole
        // row opens the issue/PR.
        .hover(|s| s.bg(theme.bg_panel_alt))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _w, cx: &mut App| {
            let row = TaskRow {
                project: click_project.clone(),
                item: click_item.clone(),
            };
            let _ = weak_tasks.update(cx, |tv, cx| tv.open_detail(row, cx));
        })
        .child(col_id)
        .child(col_title)
        .child(col_assignees)
        .child(col_status)
        .child(col_updated)
        .child(actions)
        .into_any_element()
}

/// "Open in browser" action chip. Only `https://` URLs are forwarded
/// (the scheme guard lives in `open_url`). `pub(super)` so the detail view
/// reuses the same chip. Stops propagation so the row's open-detail click
/// doesn't also fire.
pub(super) fn open_action(
    url: String,
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
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _w, cx: &mut App| {
            cx.stop_propagation();
            open_url(&url, cx);
        })
        .child("↗".to_string())
        .into_any_element()
}

/// "Create workspace from this" action chip → `create_workspace_async`. Seeds
/// the new workspace with the issue/PR reference (e.g. `"#42"`) and launches
/// Claude Code with the issue URL pre-filled as its prompt (the agent lands
/// with the URL drafted for review, ready to work the issue — the Tasks
/// equivalent of "start"). `pub(super)` so the detail view reuses the same
/// chip; stops propagation so the row's open-detail click doesn't also fire.
#[allow(clippy::too_many_arguments)]
pub(super) fn create_action(
    name: String,
    linked_issue: String,
    issue_url: String,
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
                cx.stop_propagation();
                let _ = weak_root.update(cx, |root, cx| {
                    root.create_workspace_async(
                        project.clone(),
                        name.clone(),
                        Some(AgentAdapter::ClaudeCode),
                        Some(issue_url.clone()),
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

#[cfg(test)]
mod tests {
    use super::{short_issue_title, titlecase_state};

    #[test]
    fn titlecase_state_normalizes_forge_states() {
        assert_eq!(titlecase_state("OPEN"), "Open");
        assert_eq!(titlecase_state("closed"), "Closed");
        assert_eq!(titlecase_state("Merged"), "Merged");
        assert_eq!(titlecase_state(""), "");
    }

    #[test]
    fn short_issue_title_keeps_short_titles_whole() {
        assert_eq!(short_issue_title("Fix crash"), "Fix crash");
        assert_eq!(short_issue_title("  trimmed  "), "trimmed");
    }

    #[test]
    fn short_issue_title_breaks_on_word_boundary() {
        // Long title → cut to the last word boundary within the 28-char budget.
        assert_eq!(
            short_issue_title("fix crash in parser that is triggered at startup"),
            "fix crash in parser that is"
        );
    }

    #[test]
    fn short_issue_title_hard_cut_when_first_space_is_too_early() {
        // Only whitespace is near the start (< 8) → hard cut at the budget;
        // derive_slug later turns the interior space into a dash.
        let out = short_issue_title("ab fix-crash-in-parser-that-happens-at-startup");
        assert!(out.len() <= 28);
        assert!(out.starts_with("ab fix-crash"));
    }
}
