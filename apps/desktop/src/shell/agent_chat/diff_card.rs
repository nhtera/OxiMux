//! Read-only inline diff for Edit/Write tool cards.
//!
//! An Edit tool payload carries `old_string`/`new_string` (not a unified diff),
//! so we synthesize a line-level diff with `similar` and render add/remove/
//! context rows directly. This is deliberately NOT the git-coupled `DiffView`
//! (which needs a `Repository`): a tool card diffs two in-memory strings and
//! only displays them, so it needs neither hunk line numbers nor staging.

use super::apply_patch;
use crate::shell::diff_view::syntax;
use gpui::{
    AnyElement, HighlightStyle, Hsla, IntoElement, ParentElement, SharedString, StyledText, Styled,
    div, px,
};
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
    // Codex names its file edits `apply_patch` and carries them as a `changes`
    // array of per-file patches — a different payload shape to Claude's Edit,
    // but the same card.
    if tc.name == "apply_patch" {
        return apply_patch::diff(tc);
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

/// The file this card's diff belongs to, for syntax highlighting — `None` when
/// the payload names no single file, in which case rows render plain rather
/// than guessed at.
///
/// A multi-file `apply_patch` deliberately yields `None`: one card's rows can
/// span several languages, and highlighting them all as the first file's would
/// be worse than not highlighting at all.
pub(super) fn diff_path(tc: &ToolCall) -> Option<&str> {
    if let Some(d) = acp_diff(tc) {
        return d.get("path").and_then(Value::as_str);
    }
    if tc.name == "apply_patch" {
        return apply_patch::single_path(tc);
    }
    tc.input.get("file_path").and_then(Value::as_str)
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
///
/// `path` names the file the rows came from, which decides the grammar used to
/// colour their tokens; `None` renders plain. Highlighting reuses DiffView's
/// tokenizer, so an edit reads the same in a tool card as in the diff pane.
pub(super) fn render_diff(
    lines: &[DiffLine],
    path: Option<&str>,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    // Resolved once per card, not once per row: the grammar lookup is a
    // syntax-set search, and the rows all share a file.
    let style = path.map(|p| RowStyle {
        lang: syntax::detect_language(std::path::Path::new(p)),
        path: p,
        light: theme.is_light(),
    });
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
                    div().flex_1().text_color(fg).child(row_text(&line.content, style.as_ref())),
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

/// The grammar a card's rows are coloured with, plus the path that selected it
/// (which doubles as the highlight cache's key).
struct RowStyle<'a> {
    lang: syntax::Language,
    path: &'a str,
    /// Which palette to tokenise against. Resolved per card with the rest of
    /// the style, so it reaches the row cache without widening `row_text`.
    light: bool,
}

/// One row's highlight runs, ready to hand to `StyledText`.
type Runs = Vec<(std::ops::Range<usize>, HighlightStyle)>;

/// Memoized row highlights, keyed by the file path and the row's exact text.
///
/// The transcript is not virtualized, so every visible entry — including an
/// expanded diff card — rebuilds its elements on each repaint, and streaming
/// drives repaints up to `NOTIFY_INTERVAL`-fast. Without this, a card the user
/// expanded once would re-tokenize all of its rows tens of times a second for
/// the rest of the session, while the rows never change. Syntect's per-line
/// tokenize is deterministic, so a hit is always exact.
///
/// Thread-local because rendering only ever happens on the foreground thread.
const HL_CACHE_MAX: usize = 4096;
thread_local! {
    static HL_CACHE: std::cell::RefCell<std::collections::HashMap<(bool, String, String), Runs>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A diff row's text, syntax-coloured when a grammar matched the file.
///
/// Falls back to the row's flat colour whenever highlighting produces nothing —
/// no grammar, an empty row, or token ranges that don't line up with the
/// content — so a bad match degrades to plain text and can never blank a row.
/// Untokenized spans inherit the row's colour from the parent element.
fn row_text(content: &str, style: Option<&RowStyle<'_>>) -> AnyElement {
    let text = SharedString::from(content.to_string());
    let Some(style) = style else {
        return text.into_any_element();
    };
    let runs = cached_runs(content, style);
    if runs.is_empty() {
        return text.into_any_element();
    }
    StyledText::new(text).with_highlights(runs).into_any_element()
}

/// This row's highlight runs, tokenizing only on a cache miss.
fn cached_runs(content: &str, style: &RowStyle<'_>) -> Runs {
    let light = style.light;
    // The palette is part of the key, not just the value: the cached runs
    // carry baked r/g/b, so a switch has to miss rather than hand back the
    // previous theme's colours.
    let key = (light, style.path.to_string(), content.to_string());
    HL_CACHE.with(|c| {
        if let Some(hit) = c.borrow().get(&key) {
            return hit.clone();
        }
        let runs = highlight_runs(content, style.lang, light);
        let mut cache = c.borrow_mut();
        // A whole session's diffs must not accumulate forever. Rows are cheap
        // to recompute, so drop everything rather than track recency — the
        // working set (one open card) refills on the next frame.
        if cache.len() >= HL_CACHE_MAX {
            cache.clear();
        }
        cache.insert(key, runs.clone());
        runs
    })
}

/// Tokenize one row into colour runs, skipping any token whose byte range
/// doesn't land on a char boundary of this row.
fn highlight_runs(content: &str, lang: syntax::Language, light: bool) -> Runs {
    syntax::highlight_line(content, lang, light)
        .into_iter()
        .filter_map(|t| {
            let (start, end) = (t.start.min(content.len()), t.end.min(content.len()));
            if start >= end || !content.is_char_boundary(start) || !content.is_char_boundary(end) {
                return None;
            }
            let color = Hsla::from(gpui::Rgba {
                r: t.r as f32 / 255.0,
                g: t.g as f32 / 255.0,
                b: t.b as f32 / 255.0,
                a: 1.0,
            });
            Some((start..end, HighlightStyle { color: Some(color), ..Default::default() }))
        })
        .collect()
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
    fn diff_path_names_the_file_to_highlight_as() {
        // Claude's Edit/Write carry it directly.
        let edit = ToolCall::new("id", "Edit", json!({"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}));
        assert_eq!(diff_path(&edit), Some("src/a.rs"));
        // An ACP edit's normalized payload names its own path.
        let acp = ToolCall::new(
            "id",
            "Edit",
            json!({"__acp_diff__": {"path": "src/b.rs", "old_text": "a", "new_text": "b"}}),
        );
        assert_eq!(diff_path(&acp), Some("src/b.rs"));
        // A single-file patch → its path.
        let one = ToolCall::new(
            "id",
            "apply_patch",
            json!({"changes": [{"path": "src/c.rs", "kind": {"type": "add"}, "diff": "x"}]}),
        );
        assert_eq!(diff_path(&one), Some("src/c.rs"));
        // A multi-file patch spans languages → no single grammar, so plain rows
        // rather than colouring everything as the first file.
        let many = ToolCall::new(
            "id",
            "apply_patch",
            json!({"changes": [
                {"path": "a.rs", "kind": {"type": "add"}, "diff": "x"},
                {"path": "b.py", "kind": {"type": "add"}, "diff": "y"},
            ]}),
        );
        assert_eq!(diff_path(&many), None);
        // No path at all → plain.
        let bare = ToolCall::new("id", "Write", json!({"content": "x"}));
        assert_eq!(diff_path(&bare), None);
    }

    #[test]
    fn cached_runs_match_a_fresh_tokenize_and_key_on_the_file() {
        let rs = RowStyle {
            lang: syntax::detect_language(std::path::Path::new("a.rs")),
            path: "a.rs",
            light: false,
        };
        let line = "let x = \"hi\";";
        let fresh = highlight_runs(line, rs.lang, false);
        // First call populates, second hits — both must equal a fresh tokenize,
        // or the cache would silently paint a row wrong.
        assert_eq!(cached_runs(line, &rs), fresh);
        assert_eq!(cached_runs(line, &rs), fresh);
        assert!(!fresh.is_empty(), "rust tokenizes, so this test is not vacuous");

        // Same text in a different language must not collide on the cache key.
        let py = RowStyle {
            lang: syntax::detect_language(std::path::Path::new("a.py")),
            path: "a.py",
            light: false,
        };
        assert_eq!(cached_runs(line, &py), highlight_runs(line, py.lang, false));

        // …and neither must the same text in the same file under the other
        // palette: the runs carry baked colours, so a hit here would repaint
        // the card in the theme the user just left.
        let lit = RowStyle { light: true, ..rs };
        assert_ne!(cached_runs(line, &lit), fresh, "palette is part of the key");

        // Growth only happens on a miss, so that is where the bound is enforced:
        // a full cache plus one new row evicts rather than growing forever.
        HL_CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            cache.clear();
            for i in 0..HL_CACHE_MAX {
                cache.insert((false, "f.rs".into(), format!("line{i}")), Vec::new());
            }
        });
        let fresh_row = "let evicting = 1;";
        assert_eq!(
            cached_runs(fresh_row, &rs),
            highlight_runs(fresh_row, rs.lang, false),
            "a miss against a full cache still returns correct runs"
        );
        HL_CACHE.with(|c| assert!(c.borrow().len() <= HL_CACHE_MAX, "cache stays bounded"));
    }

    #[test]
    fn row_highlighting_degrades_to_plain_text() {
        // A grammar that matches produces token runs...
        let rust = syntax::detect_language(std::path::Path::new("a.rs"));
        assert!(!syntax::highlight_line("let x = 1;", rust, false).is_empty(), "rust tokenizes");
        // ...while an unknown extension yields none, which `row_text` renders
        // as flat text rather than a blank row.
        let unknown = syntax::detect_language(std::path::Path::new("a.zzzz"));
        assert!(syntax::highlight_line("let x = 1;", unknown, false).is_empty());
        // An empty row is safe in either language.
        assert!(syntax::highlight_line("", rust, false).is_empty());
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
