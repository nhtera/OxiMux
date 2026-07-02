//! Legible per-tool body renderers for the transcript, so common tools read
//! like a chat instead of a raw-JSON log dump. Only Edit/Write/MultiEdit have a
//! diff (see `diff_card`); this covers Bash/Read/Grep/Glob. Everything else
//! falls back to the generic raw-input + result blocks in `tool_card`.
//!
//! Read-only and built on demand (the caller only invokes these when a card is
//! expanded), and every output is capped by chars then rows so a chatty tool
//! can't blow up layout.

use gpui::{AnyElement, Hsla, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::ToolCall;
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

use super::bubble;

/// Rows shown before truncation, and a char cap applied first (UTF-8-safe via
/// `bubble::elide`, which counts/takes by `char`, never slicing mid-codepoint).
const MAX_ROWS: usize = 200;
const MAX_CHARS: usize = 8000;

/// Dispatch a tool call to its bespoke body, or `None` to fall back to generic.
pub(super) fn render_tool_body(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> Option<AnyElement> {
    match tc.name.as_str() {
        "Bash" => Some(render_bash(tc, theme, density, typo)),
        "Read" => Some(render_read(tc, theme, density, typo)),
        "Grep" => Some(render_matches(tc, "match", theme, density, typo)),
        "Glob" => Some(render_matches(tc, "file", theme, density, typo)),
        _ => None,
    }
}

fn input_str<'a>(tc: &'a ToolCall, key: &str) -> &'a str {
    tc.input.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Bash → a terminal-style card: the `$`-prefixed command, then its output.
fn render_bash(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let cmd = input_str(tc, "command");
    let prompted = cmd
        .lines()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("$ {l}") } else { format!("  {l}") })
        .collect::<Vec<_>>()
        .join("\n");

    let mut col = div().flex().flex_col().gap(px(4.0)).w_full();
    col = col.child(code_block(&prompted, theme.status_info, theme, density, typo));
    if let Some(out) = tc.result.as_deref().filter(|s| !s.trim().is_empty()) {
        col = col.child(code_block(out, theme.fg_muted, theme, density, typo));
    }
    col.into_any_element()
}

/// Read → the returned file slice as a code block (the path is already in the
/// card header).
fn render_read(tc: &ToolCall, theme: Theme, density: Density, typo: &Typography) -> AnyElement {
    let body = tc.result.as_deref().unwrap_or("");
    code_block(body, theme.fg_muted, theme, density, typo)
}

/// Grep/Glob → a count header + the match/file list (`noun` = "match"/"file").
fn render_matches(
    tc: &ToolCall,
    noun: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let result = tc.result.as_deref().unwrap_or("");
    let n = result.lines().filter(|l| !l.trim().is_empty()).count();
    let header = format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .w_full()
        .child(caption(header, theme, typo))
        .child(code_block(result, theme.fg_muted, theme, density, typo))
        .into_any_element()
}

/// A bordered output block. Each line is wrapped in its OWN flex-row — a bare
/// line of text placed directly in a flex-col renders blank (a known gpui trap)
/// — and the block is char-capped then row-capped with a "… N more" footer.
fn code_block(
    text: &str,
    color: Hsla,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let capped = bubble::elide(text, MAX_CHARS);
    let lines: Vec<&str> = capped.lines().collect();
    let total = lines.len();

    let mut block = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(density.r_xs))
        .bg(theme.bg_base)
        .px(px(density.pad_row))
        .py(px(4.0))
        .text_size(px(typo.t_body_sm))
        .text_color(color);
    for line in lines.iter().take(MAX_ROWS) {
        // Preserve blank lines with a space so the row keeps its height.
        let shown = if line.is_empty() { " ".to_string() } else { (*line).to_string() };
        block = block.child(div().flex().flex_row().w_full().child(SharedString::from(shown)));
    }
    if total > MAX_ROWS {
        block = block.child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .text_color(theme.fg_subtle)
                .child(SharedString::from(format!("… {} more lines", total - MAX_ROWS))),
        );
    }
    block.into_any_element()
}

fn caption(label: String, theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(label))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatch_covers_known_tools_only() {
        let mk = |name: &str| {
            let mut tc = ToolCall::new("t", name, json!({"command": "ls", "file_path": "a", "pattern": "x"}));
            tc.result = Some("line one\nline two".into());
            tc
        };
        for t in ["Bash", "Read", "Grep", "Glob"] {
            assert!(
                render_tool_body(&mk(t), Theme::default(), Density::default(), &Typography::default()).is_some(),
                "{t} should render a bespoke body"
            );
        }
        for t in ["Edit", "Write", "MultiEdit", "WebFetch", "SomethingNew"] {
            assert!(
                render_tool_body(&mk(t), Theme::default(), Density::default(), &Typography::default()).is_none(),
                "{t} should fall back to the generic card"
            );
        }
    }

    #[test]
    fn renderers_survive_empty_and_oversized_inputs() {
        let theme = Theme::default();
        let density = Density::default();
        let typo = Typography::default();
        // Empty: no input fields, no result — must not panic.
        let empty = ToolCall::new("t", "Bash", json!({}));
        let _ = render_tool_body(&empty, theme, density, &typo);
        let _ = render_tool_body(&ToolCall::new("t", "Read", json!({})), theme, density, &typo);
        // Oversized: 10k lines must build (capped) without panic.
        let mut big = ToolCall::new("t", "Bash", json!({"command": "seq 10000"}));
        big.result = Some((0..10_000).map(|i| i.to_string()).collect::<Vec<_>>().join("\n"));
        let _ = render_tool_body(&big, theme, density, &typo);
    }
}
