//! Codex `apply_patch` payloads → renderable diff rows.
//!
//! Codex reports a file edit as an `apply_patch` tool call carrying a `changes`
//! array of per-file patches — a different shape to Claude's `Edit` (which
//! sends old/new strings) and to ACP's normalized diff, but the same card. This
//! module turns that payload into the shared `DiffLine` stream `diff_card`
//! renders, so all three providers converge on one visual.

use oximux_core::{DiffLine, DiffLineKind};
use oximux_agents::thread::ToolCall;
use serde_json::Value;

/// Build display rows for a Codex `apply_patch`, or `None` when the payload
/// carries nothing renderable (so the card falls back to the generic view).
///
/// Each change's `kind` decides how to read its `diff`: an `add` sends the new
/// file's raw content, a `delete` the removed content, and an `update` a
/// unified-diff hunk. A change we can't read contributes no rows rather than
/// failing the whole card.
pub(super) fn diff(tc: &ToolCall) -> Option<Vec<DiffLine>> {
    let changes = changes(tc)?;
    let multi = changes.len() > 1;
    let mut out = Vec::new();
    for change in changes {
        let body = change.get("diff").and_then(Value::as_str).unwrap_or_default();
        let kind = change
            .get("kind")
            .and_then(|k| k.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("update");
        // A single file's path already sits in the card header; only a
        // multi-file patch needs per-file captions to stay readable.
        if multi
            && let Some(path) = change.get("path").and_then(Value::as_str)
        {
            out.push(DiffLine { kind: DiffLineKind::Context, content: format!("▸ {path}") });
        }
        let whole_file =
            |kind: DiffLineKind| body.lines().map(move |l| DiffLine { kind, content: l.to_string() });
        match kind {
            "add" => out.extend(whole_file(DiffLineKind::Added)),
            "delete" => out.extend(whole_file(DiffLineKind::Removed)),
            _ => out.extend(unified_diff_lines(body)),
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The lone file this patch touches, or `None` when it spans several — a
/// multi-file patch has no single language to highlight its rows as.
pub(super) fn single_path(tc: &ToolCall) -> Option<&str> {
    match changes(tc)? {
        [only] => only.get("path").and_then(Value::as_str),
        _ => None,
    }
}

/// The per-file patches of an `apply_patch` call, from the input (present from
/// `item/started`) or, at completion, the structured result. The input wraps
/// them as `{changes: […]}` while the structured slot carries the bare array,
/// so both shapes are accepted.
fn changes(tc: &ToolCall) -> Option<&[Value]> {
    fn read(v: &Value) -> Option<&[Value]> {
        v.get("changes")
            .unwrap_or(v)
            .as_array()
            .map(Vec::as_slice)
            .filter(|a| !a.is_empty())
    }
    read(&tc.input).or_else(|| tc.structured.as_ref().and_then(read))
}

/// Parse unified-diff text into display rows. Lenient by design: a line whose
/// prefix we don't recognize is kept verbatim as context rather than dropped,
/// so an unfamiliar patch dialect can never silently swallow content. Hunk
/// headers survive as context rows to separate hunks visually.
fn unified_diff_lines(text: &str) -> Vec<DiffLine> {
    // `---`/`+++` are file headers only in the preamble before the first hunk;
    // once inside a hunk an identical prefix is patch content (a removed line
    // that itself starts with dashes). With no hunk header at all, every line
    // is content.
    let mut in_hunk = !text.lines().any(|l| l.starts_with("@@"));
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk && is_patch_header(line) {
            continue;
        }
        let (kind, content) = match line.as_bytes().first() {
            Some(b'+') => (DiffLineKind::Added, &line[1..]),
            Some(b'-') => (DiffLineKind::Removed, &line[1..]),
            Some(b'\\') => (DiffLineKind::NoNewlineHint, line[1..].trim_start()),
            Some(b' ') => (DiffLineKind::Context, &line[1..]),
            // Hunk headers, blank separators and anything unrecognized.
            _ => (DiffLineKind::Context, line),
        };
        out.push(DiffLine { kind, content: content.to_string() });
    }
    out
}

/// Whether a preamble line is patch metadata rather than file content.
fn is_patch_header(line: &str) -> bool {
    ["--- ", "+++ ", "*** ", "diff --git ", "index ", "Index: "]
        .iter()
        .any(|p| line.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An `apply_patch` input for one change of the given kind.
    fn patch(kind: &str, path: &str, body: &str) -> ToolCall {
        ToolCall::new(
            "id",
            "apply_patch",
            json!({"changes": [{"path": path, "kind": {"type": kind}, "diff": body}]}),
        )
    }

    #[test]
    fn add_kind_is_all_added() {
        // An `add` sends the new file's raw content, not a diff.
        let lines = diff(&patch("add", "a.rs", "one\ntwo")).expect("add diff");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Added));
        assert_eq!(lines[0].content, "one");
    }

    #[test]
    fn delete_kind_is_all_removed() {
        let lines = diff(&patch("delete", "a.rs", "one\ntwo")).expect("delete diff");
        assert!(lines.iter().all(|l| l.kind == DiffLineKind::Removed));
    }

    #[test]
    fn update_kind_parses_unified_hunk() {
        let tc = patch("update", "a.rs", "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma");
        let lines = diff(&tc).expect("update diff");
        // Markers are stripped from the body text; the hunk header survives as
        // a context row.
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed && l.content == "beta"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "BETA"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Context && l.content == "alpha"));
        assert!(lines.iter().any(|l| l.content.starts_with("@@")));
    }

    #[test]
    fn multi_file_captions_each_path() {
        let tc = ToolCall::new(
            "id",
            "apply_patch",
            json!({"changes": [
                {"path": "a.rs", "kind": {"type": "add"}, "diff": "one"},
                {"path": "b.rs", "kind": {"type": "add"}, "diff": "two"},
            ]}),
        );
        let lines = diff(&tc).expect("multi diff");
        assert!(lines.iter().any(|l| l.content == "▸ a.rs"));
        assert!(lines.iter().any(|l| l.content == "▸ b.rs"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "one"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "two"));
    }

    #[test]
    fn single_file_has_no_caption_row() {
        // The lone path is already in the card header — repeating it as a row
        // would be noise.
        let lines = diff(&patch("add", "a.rs", "one")).expect("add diff");
        assert!(lines.iter().all(|l| !l.content.starts_with('▸')));
    }

    #[test]
    fn missing_diff_field_is_none_not_a_panic() {
        // A `delete` may omit the body entirely: no rows to show, so the card
        // falls back to the generic view rather than rendering an empty diff.
        let tc = ToolCall::new(
            "id",
            "apply_patch",
            json!({"changes": [{"path": "a.rs", "kind": {"type": "delete"}}]}),
        );
        assert!(diff(&tc).is_none());
    }

    #[test]
    fn falls_back_to_structured_changes() {
        // At completion the authoritative changes land on `structured` as a
        // bare array.
        let mut tc = ToolCall::new("id", "apply_patch", json!({}));
        tc.structured = Some(json!([{"path": "a.rs", "kind": {"type": "add"}, "diff": "one"}]));
        let lines = diff(&tc).expect("structured patch");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffLineKind::Added);
    }

    #[test]
    fn renders_a_live_codex_payload() {
        // Captured verbatim off `codex app-server` 0.144.3: one turn that adds
        // a file and updates another arrives as a SINGLE fileChange item whose
        // `changes` mixes both kinds, an `add` carrying raw content while an
        // `update` carries a hunk. Pinned so a wire-shape drift fails loudly
        // here rather than silently falling back to a generic card.
        let tc = ToolCall::new(
            "exec-2ab554a4",
            "apply_patch",
            json!({"changes": [
                {
                    "path": "/tmp/oximux-codex-probe/added.txt",
                    "kind": {"type": "add"},
                    "diff": "one\ntwo\n"
                },
                {
                    "path": "/tmp/oximux-codex-probe/sample.txt",
                    "kind": {"type": "update", "move_path": null},
                    "diff": "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
                }
            ]}),
        );
        let lines = diff(&tc).expect("live codex patch");
        let row = |c: &str| lines.iter().find(|l| l.content == c).map(|l| l.kind);
        // Both files captioned, since one item carries two paths.
        assert_eq!(row("▸ /tmp/oximux-codex-probe/added.txt"), Some(DiffLineKind::Context));
        assert_eq!(row("▸ /tmp/oximux-codex-probe/sample.txt"), Some(DiffLineKind::Context));
        // The added file's raw content, with no trailing blank row from the
        // payload's trailing newline.
        assert_eq!(row("one"), Some(DiffLineKind::Added));
        assert_eq!(row("two"), Some(DiffLineKind::Added));
        // The updated file's hunk, markers stripped.
        assert_eq!(row("beta"), Some(DiffLineKind::Removed));
        assert_eq!(row("BETA"), Some(DiffLineKind::Added));
        assert_eq!(row("alpha"), Some(DiffLineKind::Context));
        assert_eq!(row("gamma"), Some(DiffLineKind::Context));
        assert!(!lines.iter().any(|l| l.content.is_empty()));
    }

    #[test]
    fn unified_diff_keeps_dashed_content_inside_a_hunk() {
        // `---` is a file header only before the first hunk; inside one it is a
        // removed line whose text starts with dashes, and dropping it would
        // silently lose patch content.
        let lines = unified_diff_lines("--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n---- rule\n+++ new");
        assert!(!lines.iter().any(|l| l.content.contains("a/x.rs")));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed && l.content == "--- rule"));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added && l.content == "++ new"));
    }

    #[test]
    fn unified_diff_without_hunk_header_keeps_every_line() {
        // With no `@@` there is no preamble, so nothing is header-stripped.
        let lines = unified_diff_lines("-old\n+new");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].kind, DiffLineKind::Removed);
        assert_eq!(lines[1].kind, DiffLineKind::Added);
    }
}
