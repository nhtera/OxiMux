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
use serde_json::Value;

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
        // `min_w_0` is load-bearing: the markdown view reports its longest
        // *unwrapped* line as min-content width, and without this a flex ancestor
        // honors that and lets the text overflow the column (clipping at the pane
        // edge) instead of wrapping. Zeroing the min-width forces it to shrink to
        // the column and wrap.
        .min_w_0()
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
        // Rendered by the dedicated question card, not this glyph — the arm just
        // keeps the match exhaustive.
        ToolCallStatus::AwaitingAnswer(_) => ("?", theme.status_warn),
        ToolCallStatus::Completed => ("✓", theme.status_ok),
        ToolCallStatus::Failed(_) => ("✗", theme.status_error),
        ToolCallStatus::Rejected | ToolCallStatus::Canceled => ("⊘", theme.fg_subtle),
    }
}

/// A friendly tool label for the header. MCP tools are journaled as
/// `mcp__<server>__<tool>`; show `<server> · <tool>` instead of the raw id so a
/// history full of `mcp__computer-use__left_click` reads like a chat, not a log.
pub(super) fn tool_display_name(name: &str) -> String {
    match name.strip_prefix("mcp__") {
        Some(rest) => rest.replacen("__", " · ", 1),
        None => name.to_string(),
    }
}

/// A short human target for the tool line — the file it touches, the command it
/// runs, the query it searches, a click coordinate, or a subagent/task brief.
/// `None` when the input carries none. Kept legible for the collapsed header,
/// which is what the user sees without expanding the card.
pub(super) fn tool_target(tc: &ToolCall) -> Option<String> {
    let input = &tc.input;
    // A file the tool touches.
    for key in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A command / pattern / url / query the tool acts on.
    for key in ["command", "pattern", "url", "query"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    // A pointer target, e.g. computer-use clicks: `coordinate: [x, y]`.
    if let Some(coord) = input.get("coordinate").and_then(Value::as_array)
        && coord.len() >= 2
    {
        return Some(format!("({}, {})", round_num(&coord[0]), round_num(&coord[1])));
    }
    // A batch of UI actions → a count, not the whole array.
    if let Some(actions) = input.get("actions").and_then(Value::as_array) {
        let n = actions.len();
        return Some(format!("{n} action{}", if n == 1 { "" } else { "s" }));
    }
    // Typed text / key chord, or a subagent/task brief → its first line.
    for key in ["text", "description", "prompt"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            let first = s.lines().next().unwrap_or(s);
            if !first.trim().is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

/// A JSON number rendered as a whole integer (drops `.0` on coordinates).
fn round_num(v: &Value) -> String {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_f64().map(|f| (f.round() as i64).to_string()))
        .unwrap_or_default()
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

    #[test]
    fn tool_target_covers_mcp_and_agent_shapes() {
        // computer-use click → coordinate tuple
        assert_eq!(
            tool_target(&tc(
                "mcp__computer-use__left_click",
                json!({"coordinate": [948, 810]}),
                ToolCallStatus::InProgress
            )),
            Some("(948, 810)".to_string())
        );
        // computer-use batch → action count
        assert_eq!(
            tool_target(&tc(
                "mcp__computer-use__computer_batch",
                json!({"actions": [{}, {}, {}]}),
                ToolCallStatus::InProgress
            )),
            Some("3 actions".to_string())
        );
        // web fetch → url; search → query
        assert_eq!(
            tool_target(&tc("WebFetch", json!({"url": "https://x.dev"}), ToolCallStatus::InProgress)),
            Some("https://x.dev".to_string())
        );
        assert_eq!(
            tool_target(&tc("WebSearch", json!({"query": "gpui focus"}), ToolCallStatus::InProgress)),
            Some("gpui focus".to_string())
        );
        // Agent → first line of the prompt
        assert_eq!(
            tool_target(&tc(
                "Agent",
                json!({"prompt": "Find the bug\nin detail"}),
                ToolCallStatus::InProgress
            )),
            Some("Find the bug".to_string())
        );
    }

    #[test]
    fn display_name_humanizes_mcp_ids() {
        assert_eq!(tool_display_name("mcp__computer-use__left_click"), "computer-use · left_click");
        assert_eq!(tool_display_name("Bash"), "Bash");
    }
}
