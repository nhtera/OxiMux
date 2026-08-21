//! Stateless renderer for the Background Tasks drawer: the Running / Finished
//! list of subagents + background bash commands the current turn spawned, folded
//! onto [`oximux_agents::thread::ChatThread::background_tasks`].
//!
//! Kept stateless (like `plan_panel`) — the toggle state lives on the chat view;
//! this module only turns `&[BackgroundTask]` into an element. "View output" is a
//! pure `open -R` side-effect (reveal the task's output file / transcript in
//! Finder), so its click closure needs no view handle.

use gpui::{
    AnyElement, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString, Styled,
    div, px,
};
use oximux_agents::thread::{BackgroundTask, TaskStatus};
use oximux_settings::{Density, Theme, Typography};

/// The status glyph + resolved color for a task, using the shared status palette.
fn status_badge(status: TaskStatus, theme: &Theme) -> (&'static str, gpui::Hsla) {
    match status {
        TaskStatus::Running => ("◐", theme.status_info),
        TaskStatus::Completed => ("●", theme.status_ok),
        TaskStatus::Failed => ("✕", theme.status_error),
    }
}

/// One task row: a kind tag, the description (running) or summary (finished), a
/// status glyph, an activity readout, and — for a finished task with an output —
/// a "View output" affordance that reveals it in Finder.
fn render_row(task: &BackgroundTask, theme: &Theme, density: &Density, typo: &Typography) -> AnyElement {
    let (glyph, glyph_color) = status_badge(task.status, theme);
    // Running shows the live description; finished prefers the completion summary.
    let primary = if task.is_running() {
        task.description.clone()
    } else {
        task.summary.clone().unwrap_or_else(|| task.description.clone())
    };
    // Activity: the last tool + a step count, when the task has run any.
    let activity = match (&task.last_tool, task.tool_uses) {
        (Some(tool), n) if n > 0 => Some(format!("{tool} · {n} step{}", if n == 1 { "" } else { "s" })),
        (None, n) if n > 0 => Some(format!("{n} step{}", if n == 1 { "" } else { "s" })),
        _ => None,
    };

    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .gap(px(density.gap_inline))
        .py(px(density.gap_inline * 0.5))
        // Status glyph.
        .child(
            div()
                .flex_none()
                .text_size(px(typo.t_body_sm))
                .text_color(glyph_color)
                .child(SharedString::from(glyph)),
        )
        // Kind tag (subagent type / "bash").
        .child(
            div()
                .flex_none()
                .px(px(density.gap_inline * 0.5))
                .rounded(px(density.r_xs))
                .bg(theme.bg_panel_alt)
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(task.kind_label().to_string())),
        )
        // Primary text (description / summary) — clips to one line inside a
        // flex_row so a long line truncates instead of rendering blank.
        .child(
            div().flex_1().min_w_0().truncate().child(
                div()
                    .text_size(px(typo.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(SharedString::from(primary)),
            ),
        );

    if let Some(activity) = activity {
        row = row.child(
            div()
                .flex_none()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(activity)),
        );
    }

    // "View output" reveals the task's output file / transcript in Finder — a
    // pure side-effect, so no view handle is needed.
    if task.has_transcript() {
        let output = task.output_file.clone().unwrap_or_default();
        row = row.child(
            div()
                .id(SharedString::from(format!("bg-task-view-{}", task.task_id)))
                .flex_none()
                .px(px(density.gap_inline * 0.5))
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .cursor_pointer()
                .hover(|s| s.text_color(theme.fg_base))
                .on_mouse_down(MouseButton::Left, move |_, _window, cx| {
                    // `reveal_path` reveals AND selects the file (its .jsonl /
                    // stdout) in the file manager rather than opening it in an
                    // editor.
                    cx.reveal_path(std::path::Path::new(&output));
                })
                .child(SharedString::from("View output")),
        );
    }

    row.into_any_element()
}

/// A muted section header ("Running · N" / "Finished · N").
fn section_header(label: &str, count: usize, theme: &Theme, density: &Density, typo: &Typography) -> AnyElement {
    div()
        .pt(px(density.gap_inline))
        .pb(px(density.gap_inline * 0.5))
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(format!("{label} · {count}")))
        .into_any_element()
}

/// Render the drawer body: a Running section then a Finished section, each shown
/// only when non-empty. Caller frames + gates it behind the toggle.
pub(super) fn render_drawer(
    tasks: &[BackgroundTask],
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let mut col = div().flex().flex_col().w_full();

    let running: Vec<&BackgroundTask> = tasks.iter().filter(|t| t.is_running()).collect();
    let finished: Vec<&BackgroundTask> = tasks.iter().filter(|t| !t.is_running()).collect();

    if !running.is_empty() {
        col = col.child(section_header("Running", running.len(), &theme, &density, typo));
        for t in running {
            col = col.child(render_row(t, &theme, &density, typo));
        }
    }
    if !finished.is_empty() {
        col = col.child(section_header("Finished", finished.len(), &theme, &density, typo));
        for t in finished {
            col = col.child(render_row(t, &theme, &density, typo));
        }
    }
    col.into_any_element()
}
