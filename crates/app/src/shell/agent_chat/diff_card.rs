//! Read-only inline diff for Edit/Write tool cards.
//!
//! An Edit tool payload carries `old_string`/`new_string` (not a unified diff),
//! so we synthesize a line-level diff with `similar` and render add/remove/
//! context rows directly. This is deliberately NOT the git-coupled `DiffView`
//! (which needs a `Repository`): a tool card diffs two in-memory strings and
//! only displays them, so it needs neither hunk line numbers nor staging.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::ToolCall;
use oximux_core::{DiffLine, DiffLineKind};
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

/// Cap on rendered diff rows so a huge edit can't blow up a card; the overflow
/// is summarized as a trailing note.
const MAX_DIFF_ROWS: usize = 200;

/// Build a line-level diff for an Edit/Write/MultiEdit tool from its input, or
/// `None` for any other tool (or a payload missing the expected fields). Pure
/// so it is unit-testable without a render.
pub(super) fn build_edit_diff(tc: &ToolCall) -> Option<Vec<DiffLine>> {
    // ACP edits arrive as a normalized `__acp_diff__` payload (on the input at
    // start, or the structured result if the diff only lands at completion) —
    // rendered like an Edit: a line-level diff of old→new, all-additions when
    // there's no prior content (a new-file write). Provider-neutral: the view
    // never learns which backend produced the edit.
    if let Some(d) = acp_diff(tc) {
        let old = d.get("old_text").and_then(Value::as_str).unwrap_or("");
        let new = d.get("new_text").and_then(Value::as_str).unwrap_or("");
        return Some(if old.is_empty() {
            new.lines()
                .map(|l| DiffLine { kind: DiffLineKind::Added, content: l.to_string() })
                .collect()
        } else {
            lines_from_change(old, new)
        });
    }
    let obj = tc.input.as_object()?;
    match tc.name.as_str() {
        "Edit" => {
            let old = obj.get("old_string")?.as_str()?;
            let new = obj.get("new_string")?.as_str()?;
            Some(lines_from_change(old, new))
        }
        "Write" => {
            // A whole-file write: every line is an addition.
            let content = obj.get("content")?.as_str()?;
            Some(
                content
                    .lines()
                    .map(|l| DiffLine { kind: DiffLineKind::Added, content: l.to_string() })
                    .collect(),
            )
        }
        "MultiEdit" => {
            let edits = obj.get("edits")?.as_array()?;
            let mut out = Vec::new();
            for e in edits {
                let old = e.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
                let new = e.get("new_string").and_then(|v| v.as_str()).unwrap_or("");
                out.extend(lines_from_change(old, new));
            }
            (!out.is_empty()).then_some(out)
        }
        _ => None,
    }
}

/// The normalized ACP diff payload (`{path, old_text, new_text}`) for this tool
/// call, from the input (edit start) or the structured result (late diff), if any.
fn acp_diff(tc: &ToolCall) -> Option<&Value> {
    tc.input
        .get("__acp_diff__")
        .or_else(|| tc.structured.as_ref().and_then(|s| s.get("__acp_diff__")))
}

/// Line-level diff of two strings → `DiffLine`s (trailing newline stripped for
/// display).
fn lines_from_change(old: &str, new: &str) -> Vec<DiffLine> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .map(|c| {
            let kind = match c.tag() {
                ChangeTag::Delete => DiffLineKind::Removed,
                ChangeTag::Insert => DiffLineKind::Added,
                ChangeTag::Equal => DiffLineKind::Context,
            };
            DiffLine { kind, content: c.value().trim_end_matches('\n').to_string() }
        })
        .collect()
}

/// Render a diff line stream as a bordered code block: added rows green-tinted,
/// removed red-tinted, context muted. Rows are capped; a trailing note reports
/// any overflow.
pub(super) fn render_diff(
    lines: &[DiffLine],
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let mut col = div()
        .flex()
        .flex_col()
        .w_full()
        .rounded(px(density.r_xs))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_base)
        .text_size(px(typo.t_body_sm));

    for line in lines.iter().take(MAX_DIFF_ROWS) {
        let (bg, fg, prefix) = match line.kind {
            DiffLineKind::Added => (theme.status_added.opacity(0.14), theme.status_added, "+"),
            DiffLineKind::Removed => {
                (theme.status_removed.opacity(0.14), theme.status_removed, "-")
            }
            DiffLineKind::Context => (theme.bg_base, theme.fg_muted, " "),
            DiffLineKind::NoNewlineHint => (theme.bg_base, theme.fg_subtle, "\\"),
        };
        // Wrap each line in a flex_row: a nowrap/plain text line placed directly
        // in a flex column can render blank (a known GPUI quirk).
        col = col.child(
            div()
                .flex()
                .flex_row()
                .w_full()
                .bg(bg)
                .px(px(density.pad_row))
                .child(
                    div()
                        .w(px(10.0))
                        .flex_none()
                        .text_color(fg)
                        .child(SharedString::from(prefix)),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(fg)
                        .child(SharedString::from(line.content.clone())),
                ),
        );
    }

    if lines.len() > MAX_DIFF_ROWS {
        col = col.child(
            div()
                .w_full()
                .px(px(density.pad_row))
                .py(px(2.0))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(format!(
                    "… {} more lines",
                    lines.len() - MAX_DIFF_ROWS
                ))),
        );
    }
    col.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::thread::ToolCall;
    use serde_json::json;

    #[test]
    fn edit_diff_has_removed_then_added() {
        let tc = ToolCall::new("id", "Edit", json!({"old_string": "a\nb", "new_string": "a\nc"}));
        let lines = build_edit_diff(&tc).expect("edit diff");
        // context "a", removed "b", added "c".
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed && l.content == "b"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "c"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Context && l.content == "a"));
    }

    #[test]
    fn write_is_all_added() {
        let tc = ToolCall::new("id", "Write", json!({"file_path": "x", "content": "one\ntwo"}));
        let lines = build_edit_diff(&tc).expect("write diff");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Added));
    }

    #[test]
    fn non_edit_tool_has_no_diff() {
        let tc = ToolCall::new("id", "Bash", json!({"command": "ls"}));
        assert!(build_edit_diff(&tc).is_none());
    }

    #[test]
    fn acp_diff_payload_on_input_builds_a_diff() {
        // An ACP edit's normalized `__acp_diff__` payload renders like an Edit.
        let tc = ToolCall::new(
            "id",
            "Edit",
            json!({"file_path": "a.rs", "__acp_diff__": {"old_text": "a\nb", "new_text": "a\nc"}}),
        );
        let lines = build_edit_diff(&tc).expect("acp diff");
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed && l.content == "b"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "c"));
    }

    #[test]
    fn acp_diff_without_old_text_is_all_added() {
        // A whole-file write (no prior content) is all additions.
        let tc = ToolCall::new(
            "id",
            "Write",
            json!({"__acp_diff__": {"old_text": null, "new_text": "one\ntwo"}}),
        );
        let lines = build_edit_diff(&tc).expect("acp write diff");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Added));
    }

    #[test]
    fn acp_diff_on_structured_result_builds_a_diff() {
        // A diff that only arrived at completion lands in the structured slot.
        let mut tc = ToolCall::new("id", "Modify x", json!({"file_path": "x"}));
        tc.structured = Some(json!({"__acp_diff__": {"old_text": "x", "new_text": "y"}}));
        let lines = build_edit_diff(&tc).expect("structured acp diff");
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "y"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed && l.content == "x"));
    }
}
