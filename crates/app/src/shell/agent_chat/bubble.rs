//! Pure (cx-free) renderers for chat transcript pieces.
//!
//! Each function builds the *static* content of one bubble kind — user text,
//! the assistant's markdown body, a thinking block body, or the one-line
//! tool-call placeholder. The interactive shell (the thinking disclosure
//! toggle, the scroll container) lives in the view; keeping these pure means
//! the view file stays small and this file needs no `Context`.

use gpui::{AnyElement, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use oximux_agents::thread::{ToolCall, ToolCallStatus};
use oximux_settings::{Density, Theme, Typography};

/// A small role caption above a message ("You" / "Claude").
pub(super) fn role_caption(label: &str, color: Hsla, typo: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typo.t_label_xs))
        .text_color(color)
        .child(SharedString::from(label.to_string()))
}

/// Max width of a user bubble — a prompt longer than this wraps into a column
/// rather than stretching the full measure, keeping the right-aligned shape.
const USER_BUBBLE_MAX_W: f32 = 520.0;

/// The user's prompt as a right-aligned, filled bubble (Claude-Desktop style):
/// visually distinct from the assistant's plain left-aligned text, so the thread
/// reads as an asymmetric back-and-forth. `bg_overlay` (the lightest surface)
/// reads as "my message" lifted off the panel. No `nowrap`/`truncate` (which
/// render blank in a flex column) — the width cap lets the text wrap naturally.
pub(super) fn user_body(
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .justify_end()
        .w_full()
        .child(
            div()
                .max_w(px(USER_BUBBLE_MAX_W))
                .rounded(px(density.r_card))
                .bg(theme.bg_overlay)
                .px(px(density.pad_panel))
                .py(px(6.0))
                .text_size(px(typo.t_body_md))
                .text_color(theme.fg_base)
                .child(SharedString::from(text.to_string())),
        )
        .into_any_element()
}

/// The max width for the user's attached-image thumbnail strip — matches the
/// text bubble so images and prose share the same right edge.
pub(super) const USER_IMAGES_MAX_W: f32 = USER_BUBBLE_MAX_W;

/// The assistant's visible reply, rendered as GitHub-flavored markdown. `key`
/// discriminates the renderer's per-bubble state so two bubbles never share it.
pub(super) fn assistant_body(key: usize, body: &str, typo: &Typography) -> AnyElement {
    // Dark-only app: pin the markdown renderer to the dark highlight theme (its
    // own default is a light code theme, which reads washed-out on the panel).
    let style = TextViewStyle {
        is_dark: true,
        highlight_theme: HighlightTheme::default_dark(),
        ..Default::default()
    };
    div()
        .w_full()
        .text_size(px(typo.t_body_md))
        .child(
            TextView::markdown(("chat-assistant-md", key), body.to_string())
                .style(style)
                .selectable(true),
        )
        .into_any_element()
}

/// The extended-thinking body shown when its disclosure is expanded. Muted +
/// left-ruled so it reads as secondary to the reply.
pub(super) fn thinking_body(
    text: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    div()
        .w_full()
        .border_l_2()
        .border_color(theme.border_inactive)
        .pl(px(density.pad_panel))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// Status → (glyph, color). `WaitingForConfirmation` reads as a pause because
/// the tool is gated on the user's Allow/Reject decision.
pub(super) fn status_glyph(status: &ToolCallStatus, theme: Theme) -> (&'static str, Hsla) {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::InProgress => ("▸", theme.fg_subtle),
        ToolCallStatus::WaitingForConfirmation(_) => ("⏸", theme.status_warn),
        ToolCallStatus::Completed => ("✓", theme.status_ok),
        ToolCallStatus::Failed(_) => ("✗", theme.status_error),
        ToolCallStatus::Rejected | ToolCallStatus::Canceled => ("⊘", theme.fg_subtle),
    }
}

/// A short human target for the tool line — the file it touches, the command it
/// runs, or the pattern it searches. `None` when the input carries none.
pub(super) fn tool_target(tc: &ToolCall) -> Option<String> {
    let input = &tc.input;
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        return Some(cmd.to_string());
    }
    if let Some(p) = input.get("pattern").and_then(|v| v.as_str()) {
        return Some(p.to_string());
    }
    None
}

/// Cap a label to `max` chars, appending an ellipsis when cut. Keeps the tool
/// header one row regardless of a giant command/path.
pub(super) fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tc(name: &str, input: serde_json::Value, status: ToolCallStatus) -> ToolCall {
        let mut t = ToolCall::new("id", name, input);
        t.status = status;
        t
    }

    #[test]
    fn tool_target_prefers_file_then_command_then_pattern() {
        assert_eq!(
            tool_target(&tc("Edit", json!({"file_path": "a.rs"}), ToolCallStatus::InProgress)),
            Some("a.rs".to_string())
        );
        assert_eq!(
            tool_target(&tc("Bash", json!({"command": "ls -la"}), ToolCallStatus::InProgress)),
            Some("ls -la".to_string())
        );
        assert_eq!(
            tool_target(&tc("Grep", json!({"pattern": "foo"}), ToolCallStatus::InProgress)),
            Some("foo".to_string())
        );
        assert_eq!(
            tool_target(&tc("Task", json!({"x": 1}), ToolCallStatus::InProgress)),
            None
        );
    }

    #[test]
    fn elide_caps_long_strings() {
        assert_eq!(elide("short", 80), "short");
        let long = "x".repeat(100);
        let out = elide(&long, 80);
        assert_eq!(out.chars().count(), 81);
        assert!(out.ends_with('…'));
    }
}
