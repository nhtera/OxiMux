//! Fullscreen tool-payload sheet: a dimmed backdrop + a large centered panel
//! showing a single tool call's full payload at full height — a big diff
//! virtualized via `uniform_list`, or a long shell/read/fetch/search body in a
//! scroll region with the inline row/char caps lifted. Opened from a tool
//! card's expand icon; dismissed by the backdrop, the ✕, or Escape (Escape is
//! wired in `mod.rs`'s `DismissOverlay` handler). Reuses the image-lightbox
//! stacking approach (an absolute overlay above the transcript) rather than a
//! new modal system.
//!
//! The panel reads the tool call live from the thread each render, so a still-
//! running tool's output grows in the sheet.

use gpui::{
    AnyElement, ClipboardItem, Context, InteractiveElement, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement, Styled, div, px, uniform_list,
};
use oximux_agents::thread::ToolCall;
use oximux_core::{DiffLine, DiffLineKind};
use oximux_settings::{Density, Theme, Typography};
use std::sync::Arc;

use super::bubble;
use super::diff_card;
use super::tool_bodies;
use super::AgentChatView;

/// A tool card qualifies for the expand-to-sheet affordance when its payload is
/// substantial enough to be cramped inline: any Edit/Write/MultiEdit diff, or a
/// tool whose textual result runs past this many characters (a long shell run,
/// a big file read, a fetched page).
const SUBSTANTIAL_CHARS: usize = 600;

/// Whether this tool call should offer the fullscreen expand affordance.
pub(super) fn is_sheet_expandable(tc: &ToolCall) -> bool {
    if diff_card::build_edit_diff(tc).is_some() {
        return true;
    }
    tc.result.as_deref().map(|r| r.chars().count()).unwrap_or(0) > SUBSTANTIAL_CHARS
}

/// The full payload as plain text for the sheet's copy button: the reconstructed
/// diff for an edit (prefix + line), otherwise the command (if any) followed by
/// the tool's result, falling back to the raw input. `diff` is the pre-built
/// edit diff (computed once by the caller) so this doesn't recompute it.
fn payload_text(tc: &ToolCall, diff: Option<&[DiffLine]>) -> String {
    if let Some(lines) = diff {
        return lines
            .iter()
            .map(|l| format!("{}{}", diff_prefix(l.kind), l.content))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(cmd) = tc.input.get("command").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty())
    {
        parts.push(format!("$ {cmd}"));
    }
    if let Some(result) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(result.to_string());
    }
    if parts.is_empty() {
        parts.push(tc.input.to_string());
    }
    parts.join("\n")
}

fn diff_prefix(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Added => "+",
        DiffLineKind::Removed => "-",
        DiffLineKind::Context => " ",
        DiffLineKind::NoNewlineHint => "\\",
    }
}

/// The fullscreen sheet for one tool call: dimmed backdrop (click-to-dismiss) +
/// a centered panel with a title/status header, a copy button, a ✕, and the
/// full-height body.
pub(super) fn render_tool_sheet(
    tc: &ToolCall,
    copied: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    // Build the (potentially expensive Myers) diff ONCE per frame and reuse it
    // for both the copy payload and the body — the header and body would
    // otherwise each recompute it. `render_tool_sheet` runs on every view
    // repaint (streaming tokens, composer keystrokes), so a single build for a
    // large diff matters.
    let diff = diff_card::build_edit_diff(tc);
    let payload = payload_text(tc, diff.as_deref());
    let header = sheet_header(tc, payload, copied, theme, density, typo, cx);
    let body = sheet_body(tc, diff, theme, density, typo, cx);

    let panel = div()
        .w(px(920.0))
        .max_w(gpui::relative(0.92))
        .h(gpui::relative(0.86))
        .flex()
        .flex_col()
        .min_h_0()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.border_input)
        .bg(theme.bg_panel)
        .overflow_hidden()
        // Clicks inside the panel must not bubble to the backdrop's close.
        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
        .child(header)
        .child(body);

    div()
        .id("tool-sheet")
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .p(px(24.0))
        .bg(gpui::black().opacity(0.66))
        // A click on the bare backdrop closes the sheet.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _e, window, cx| this.close_tool_sheet(window, cx)),
        )
        .child(panel)
        .into_any_element()
}

/// The panel header: status glyph + tool label (+ target), a spacer, then the
/// copy and close controls.
fn sheet_header(
    tc: &ToolCall,
    payload: String,
    copied: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let (glyph, glyph_color) = bubble::status_glyph(&tc.status, theme);
    let mut label = bubble::tool_display_name(&tc.name);
    if let Some(target) = bubble::tool_target(tc) {
        label.push(' ');
        label.push_str(&bubble::elide(&target, 120));
    }

    let copy = super::tool_card::pill_button(
        format!("tool-sheet-copy-{}", tc.id),
        "Copy",
        theme.status_info,
        density,
        typo,
        cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(payload.clone()));
            this.flash_sheet_copied(cx);
        }),
    );

    let copy_label = if copied {
        div()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_ok)
            .child(SharedString::from("Copied ✓"))
            .into_any_element()
    } else {
        copy
    };

    let close = div()
        .id("tool-sheet-close")
        .size(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .text_color(theme.fg_muted)
        .cursor_pointer()
        .hover(|s| s.bg(theme.bg_panel_alt).text_color(theme.fg_base))
        .on_click(cx.listener(|this, _e, window, cx| this.close_tool_sheet(window, cx)))
        .child(SharedString::from("✕"));

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .flex_none()
        .px(px(density.pad_panel))
        .py(px(10.0))
        .border_b_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .text_color(glyph_color)
                .child(SharedString::from(glyph.to_string())),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(label)),
        )
        .child(copy_label)
        .child(close)
        .into_any_element()
}

/// The full-height body: a virtualized diff for Edit/Write/MultiEdit, otherwise
/// the reused inline body renderer at full caps inside a vertical scroll region.
fn sheet_body(
    tc: &ToolCall,
    diff: Option<Vec<DiffLine>>,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    if let Some(lines) = diff {
        // An empty diff (e.g. a Write creating an empty file) would render a blank
        // scroll region — show an explicit note instead.
        if lines.is_empty() {
            return empty_note("No changes to show.", theme, density, typo);
        }
        return virtualized_diff(&tc.id, lines, theme, density, typo, cx);
    }

    // Text bodies (shell/read/search/fetch/websearch/agent/mcp/generic) reuse the
    // inline renderer with the caps lifted (`full = true`), scrolled vertically.
    let body = tool_bodies::render_tool_body(tc, true, theme, density, typo)
        .unwrap_or_else(|| tool_bodies::render_generic_input(tc, true, theme, density, typo));
    div()
        .id("tool-sheet-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scroll()
        .p(px(density.pad_panel))
        .child(body)
        .into_any_element()
}

/// A centered muted note filling the body area (empty diff / nothing to show).
fn empty_note(text: &'static str, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .w_full()
        .items_center()
        .justify_center()
        .p(px(density.pad_panel))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(text))
        .into_any_element()
}

/// A virtualized diff list: each line is a fixed-height (`h_row`) row so
/// `uniform_list` can position by index and paint only the visible window — a
/// 1k-line diff scrolls smoothly. Long lines clip at the panel edge (the Copy
/// button carries the full payload). Rows carry the add/remove/context tint.
fn virtualized_diff(
    tool_id: &str,
    lines: Vec<DiffLine>,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let count = lines.len();
    let lines = Arc::new(lines);
    let typo = typo.clone();
    let list = uniform_list(
        SharedString::from(format!("tool-sheet-diff-{tool_id}")),
        count,
        cx.processor(move |_me: &mut AgentChatView, range: std::ops::Range<usize>, _window, _cx| {
            range
                .map(|i| diff_row(&lines[i], theme, density, &typo))
                .collect::<Vec<AnyElement>>()
        }),
    )
    .flex_1()
    .min_h_0()
    .w_full();

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .p(px(density.pad_panel))
        .child(list)
        .into_any_element()
}

/// One fixed-height diff row: a `+`/`-`/` ` gutter and the line content, tinted
/// by kind. Fixed height keeps `uniform_list`'s row math correct; `overflow_hidden`
/// clips an over-long line rather than wrapping (which would break uniformity).
fn diff_row(line: &DiffLine, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let (bg, fg) = match line.kind {
        DiffLineKind::Added => (theme.status_added.opacity(0.14), theme.status_added),
        DiffLineKind::Removed => (theme.status_removed.opacity(0.14), theme.status_removed),
        DiffLineKind::Context => (theme.bg_base, theme.fg_muted),
        DiffLineKind::NoNewlineHint => (theme.bg_base, theme.fg_subtle),
    };
    div()
        .flex()
        .flex_row()
        .w_full()
        .h(px(density.h_row))
        .items_center()
        .overflow_hidden()
        .bg(bg)
        .px(px(density.pad_row))
        .text_size(px(typo.t_body_sm))
        .child(
            div()
                .w(px(12.0))
                .flex_none()
                .text_color(fg)
                .child(SharedString::from(diff_prefix(line.kind))),
        )
        .child(
            div()
                .flex_1()
                .text_color(fg)
                .child(SharedString::from(line.content.clone())),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edits_and_long_output_are_expandable() {
        // Any Edit/Write diff qualifies, however small.
        let edit = ToolCall::new("t", "Edit", json!({"old_string": "a", "new_string": "b"}));
        assert!(is_sheet_expandable(&edit));
        let write = ToolCall::new("t", "Write", json!({"file_path": "x", "content": "one\ntwo"}));
        assert!(is_sheet_expandable(&write));

        // A long textual result qualifies even without a diff.
        let mut big = ToolCall::new("t", "Bash", json!({"command": "seq 1000"}));
        big.result = Some("x".repeat(SUBSTANTIAL_CHARS + 1));
        assert!(is_sheet_expandable(&big));
    }

    #[test]
    fn short_bodies_are_not_expandable() {
        // No diff, short/absent result → no expand affordance.
        let short = ToolCall::new("t", "Bash", json!({"command": "ls"}));
        assert!(!is_sheet_expandable(&short));
        let mut small = ToolCall::new("t", "Read", json!({"file_path": "a.rs"}));
        small.result = Some("fn main() {}".into());
        assert!(!is_sheet_expandable(&small));
    }

    /// Helper mirroring the render path: build the diff once, then the payload.
    fn payload_of(tc: &ToolCall) -> String {
        payload_text(tc, diff_card::build_edit_diff(tc).as_deref())
    }

    #[test]
    fn payload_reconstructs_diff_with_prefixes() {
        let edit = ToolCall::new("t", "Edit", json!({"old_string": "a\nb", "new_string": "a\nc"}));
        let text = payload_of(&edit);
        // Context " a", removed "-b", added "+c" all present.
        assert!(text.lines().any(|l| l == " a"), "context line: {text:?}");
        assert!(text.lines().any(|l| l == "-b"), "removed line: {text:?}");
        assert!(text.lines().any(|l| l == "+c"), "added line: {text:?}");
    }

    #[test]
    fn payload_prefers_command_plus_output_then_falls_back() {
        // Shell: "$ cmd" then the result.
        let mut bash = ToolCall::new("t", "Bash", json!({"command": "echo hi"}));
        bash.result = Some("hi".into());
        assert_eq!(payload_of(&bash), "$ echo hi\nhi");

        // No command / no result → the raw input, never an empty string.
        let bare = ToolCall::new("t", "Weird", json!({"opt": true}));
        assert!(payload_of(&bare).contains("opt"));
    }
}
