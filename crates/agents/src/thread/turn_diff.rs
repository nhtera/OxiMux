//! Per-file change stats for the turn-end "N files changed" card.
//!
//! Two sources, because the backends differ in what they report:
//!
//! - **Codex** sends the turn's whole unified diff (`turn/diff/updated`), which
//!   also covers files written by shell commands. [`stats_from_unified_diff`]
//!   counts it.
//! - **Everyone else** reports no turn diff, so [`stats_from_turn_entries`]
//!   sums the turn's own edit cards. That misses a file written by a bare shell
//!   command — those never produce an edit card — which is the price of having
//!   no wire diff, not a bug in the counting.
//!
//! Pure and gpui-free: the fold calls these, and they are unit-testable without
//! a thread or a backend.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::entry::ThreadEntry;
use super::tool_call::{ToolCall, ToolCallStatus};

/// One file a turn changed, and by how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFileChange {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

/// Count a unified diff into per-file stats.
///
/// Walks LINE BY LINE, starting a new file only on a line that BEGINS with
/// `diff --git ` — the same line-anchored rule the sibling parser in
/// `oximux_git::parse_unified_diff` uses. Splitting on the substring anywhere
/// would let a diff that merely CONTAINS the text (a `+diff --git a/x b/x` line
/// in an edited README, or in this repo's own plan docs) fabricate a phantom
/// file and inflate the card's count.
///
/// The path comes from the `+++ b/…` header, falling back to the `diff --git`
/// line so a deletion (whose `+++` is `/dev/null`) still names its file.
/// `+++`/`---`/`@@` headers are not counted as content — only body lines are.
pub fn stats_from_unified_diff(diff: &str) -> Vec<TurnFileChange> {
    let mut out: Vec<TurnFileChange> = Vec::new();
    let mut current: Option<TurnFileChange> = None;
    // Header lines only qualify while they precede the first hunk: past `@@`,
    // a `+++ `/`--- ` line is body content (an edited diff/patch file), not a
    // header, and must count rather than rename the file.
    let mut in_hunk = false;
    for raw in diff.lines() {
        // CRLF tolerance, matching the sibling parser.
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(done) = current.take().filter(|f| !f.path.is_empty()) {
                out.push(done);
            }
            current = Some(TurnFileChange { path: git_header_path(rest), added: 0, removed: 0 });
            in_hunk = false;
            continue;
        }
        let Some(file) = current.as_mut() else {
            // Preamble before the first `diff --git` — nothing to attribute it to.
            continue;
        };
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            // The post-image names the file; `/dev/null` means it was deleted, in
            // which case the `diff --git` fallback already named it.
            if let Some(p) = line.strip_prefix("+++ ")
                && let Some(real) = strip_diff_prefix(p)
            {
                file.path = real.to_string();
            }
            if line.starts_with("+++ ") || line.starts_with("--- ") {
                continue;
            }
        }
        if line.starts_with('+') {
            file.added += 1;
        } else if line.starts_with('-') {
            file.removed += 1;
        }
    }
    if let Some(done) = current.filter(|f| !f.path.is_empty()) {
        out.push(done);
    }
    out
}

/// The `b/…` path from a `diff --git a/x b/x` header line, else the `a/…` one
/// (a deletion's post-image is `/dev/null`). Empty when neither parses.
fn git_header_path(header: &str) -> String {
    let mut parts = header.split_whitespace();
    let a = parts.next().unwrap_or_default();
    let b = parts.next().unwrap_or_default();
    strip_diff_prefix(b)
        .or_else(|| strip_diff_prefix(a))
        .unwrap_or_default()
        .to_string()
}

/// Strip a diff path's `a/`/`b/` prefix. `None` for `/dev/null` and for an empty
/// path, so a caller can fall back rather than showing the placeholder.
fn strip_diff_prefix(p: &str) -> Option<&str> {
    let p = p.split('\t').next().unwrap_or(p).trim();
    if p.is_empty() || p == "/dev/null" {
        return None;
    }
    Some(p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p))
}

/// Sum the edit cards of the turn that ends at the tail of `entries`.
///
/// The turn is everything after the last `User` prompt. Only settled cards count
/// — an in-flight or failed edit did not change the file. Repeated edits to one
/// path merge into a single row, since the card counts FILES.
pub fn stats_from_turn_entries(entries: &[ThreadEntry]) -> Vec<TurnFileChange> {
    let start = entries
        .iter()
        .rposition(|e| matches!(e, ThreadEntry::User { .. }))
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut out: Vec<TurnFileChange> = Vec::new();
    for entry in &entries[start..] {
        let ThreadEntry::ToolCall(tc) = entry else { continue };
        if !matches!(tc.status, ToolCallStatus::Completed) {
            continue;
        }
        for change in edit_card_stats(tc) {
            match out.iter_mut().find(|f| f.path == change.path) {
                Some(existing) => {
                    existing.added += change.added;
                    existing.removed += change.removed;
                }
                None => out.push(change),
            }
        }
    }
    out
}

/// The files + counts one edit-family tool card changed. Empty for every other
/// tool, which is what keeps a read/search/command card out of the summary.
fn edit_card_stats(tc: &ToolCall) -> Vec<TurnFileChange> {
    let path = || {
        tc.input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match tc.name.as_str() {
        // A new file is all-added; its `content` is the whole body.
        "Write" => {
            let content = tc.input.get("content").and_then(Value::as_str).unwrap_or_default();
            vec![TurnFileChange { path: path(), added: line_count(content), removed: 0 }]
        }
        "Edit" => vec![TurnFileChange {
            path: path(),
            added: field_lines(&tc.input, "new_string"),
            removed: field_lines(&tc.input, "old_string"),
        }],
        // MultiEdit applies several edits to ONE file — sum them into one row.
        "MultiEdit" => {
            let (mut added, mut removed) = (0, 0);
            for e in tc.input.get("edits").and_then(Value::as_array).unwrap_or(&Vec::new()) {
                added += field_lines(e, "new_string");
                removed += field_lines(e, "old_string");
            }
            vec![TurnFileChange { path: path(), added, removed }]
        }
        // A Codex patch carries a real per-file unified diff already.
        "apply_patch" => tc
            .input
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|c| {
                        let p = c.get("path").and_then(Value::as_str)?.to_string();
                        let d = c.get("diff").and_then(Value::as_str).unwrap_or_default();
                        let (added, removed) = count_body_lines(d);
                        Some(TurnFileChange { path: p, added, removed })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
    .into_iter()
    // A card with no path names no file — drop it rather than show a blank row.
    .filter(|f| !f.path.is_empty())
    .collect()
}

/// `+`/`-` body lines in a bare unified-diff fragment (no `diff --git` header).
fn count_body_lines(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn field_lines(v: &Value, key: &str) -> usize {
    line_count(v.get(key).and_then(Value::as_str).unwrap_or_default())
}

/// Lines in a body, counting a trailing newline as terminating the last line
/// rather than starting an empty one. Empty text is zero lines.
fn line_count(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The real `turn/diff/updated` payload captured from codex-cli 0.144.3 for a
    /// turn that created one file and appended to another.
    const CAPTURED: &str = "\
diff --git a/notes.txt b/notes.txt
new file mode 100644
index 0000000000000000..1bf1b6700c0ad94e
--- /dev/null
+++ b/notes.txt
@@ -0,0 +1,2 @@
+First note
+Second note
diff --git a/scratch-verify.txt b/scratch-verify.txt
index ce013625030ba8db..13f7029cea893004
--- a/scratch-verify.txt
+++ b/scratch-verify.txt
@@ -1 +1,2 @@
 hello
+verified
";

    #[test]
    fn counts_a_real_captured_turn_diff() {
        let files = stats_from_unified_diff(CAPTURED);
        assert_eq!(
            files,
            vec![
                TurnFileChange { path: "notes.txt".into(), added: 2, removed: 0 },
                TurnFileChange { path: "scratch-verify.txt".into(), added: 1, removed: 0 },
            ]
        );
    }

    #[test]
    fn diff_headers_are_not_counted_as_content() {
        // `--- a/x` must not read as a removed line, nor `+++ b/x` as an added
        // one — the classic off-by-two in a naive +/- counter.
        let files = stats_from_unified_diff(
            "diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n context\n",
        );
        assert_eq!(files, vec![TurnFileChange { path: "x.rs".into(), added: 1, removed: 1 }]);
    }

    #[test]
    fn a_deleted_file_still_names_itself() {
        // The post-image is /dev/null, so the name comes from the git header.
        let files = stats_from_unified_diff(
            "diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\n--- a/gone.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n",
        );
        assert_eq!(files, vec![TurnFileChange { path: "gone.txt".into(), added: 0, removed: 2 }]);
    }

    #[test]
    fn a_diff_whose_content_mentions_diff_git_counts_one_file() {
        // Editing a doc that SHOWS a git diff (this repo's own plan files do) must
        // not fabricate a second, phantom file from the body text. Only a line
        // that BEGINS a section starts one.
        let files = stats_from_unified_diff(
            "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,3 @@\n ctx\n+diff --git a/x b/x\n+--- a/x\n",
        );
        assert_eq!(
            files,
            vec![TurnFileChange { path: "README.md".into(), added: 2, removed: 0 }],
            "body text that looks like a diff header is content, not a new file"
        );
    }

    #[test]
    fn header_lines_only_count_as_headers_before_the_first_hunk() {
        // `--- a/x` inside a HUNK is an added/removed line of an edited patch
        // file, not this section's header.
        let files = stats_from_unified_diff(
            "diff --git a/p.patch b/p.patch\n--- a/p.patch\n+++ b/p.patch\n@@ -1,2 +1,2 @@\n--- old header\n+++ new header\n",
        );
        assert_eq!(
            files,
            vec![TurnFileChange { path: "p.patch".into(), added: 1, removed: 1 }],
            "in-hunk header-looking lines are content"
        );
    }

    #[test]
    fn multiple_sections_each_count_independently() {
        let files = stats_from_unified_diff(CAPTURED);
        assert_eq!(files.len(), 2, "the captured turn changed exactly two files");
    }

    #[test]
    fn a_no_newline_marker_is_not_counted() {
        let files = stats_from_unified_diff(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n",
        );
        assert_eq!(files, vec![TurnFileChange { path: "a.txt".into(), added: 1, removed: 1 }]);
    }

    #[test]
    fn crlf_input_parses_like_lf() {
        let files = stats_from_unified_diff(
            "diff --git a/a.txt b/a.txt\r\n--- a/a.txt\r\n+++ b/a.txt\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n",
        );
        assert_eq!(files, vec![TurnFileChange { path: "a.txt".into(), added: 1, removed: 1 }]);
    }

    #[test]
    fn an_empty_or_headerless_diff_yields_no_files() {
        assert!(stats_from_unified_diff("").is_empty());
        assert!(stats_from_unified_diff("not a diff at all\n+x\n").is_empty());
    }

    fn completed(name: &str, input: Value) -> ThreadEntry {
        let mut tc = ToolCall::new("t", name, input);
        tc.status = ToolCallStatus::Completed;
        ThreadEntry::ToolCall(tc)
    }

    fn user() -> ThreadEntry {
        ThreadEntry::User { text: "go".into(), images: vec![], checkpoint: None }
    }

    #[test]
    fn aggregates_the_edit_family_of_one_turn() {
        let entries = vec![
            user(),
            completed("Write", json!({"file_path": "new.rs", "content": "a\nb\nc"})),
            completed("Edit", json!({"file_path": "old.rs", "old_string": "x", "new_string": "y\nz"})),
            completed("Bash", json!({"command": "ls"})),
            completed("Read", json!({"file_path": "other.rs"})),
        ];
        let files = stats_from_turn_entries(&entries);
        assert_eq!(
            files,
            vec![
                // A Write is a whole new file: all added.
                TurnFileChange { path: "new.rs".into(), added: 3, removed: 0 },
                TurnFileChange { path: "old.rs".into(), added: 2, removed: 1 },
            ],
            "only the edit family counts — a Bash/Read card changed no file"
        );
    }

    #[test]
    fn repeated_edits_to_one_file_are_one_row() {
        let entries = vec![
            user(),
            completed("Edit", json!({"file_path": "a.rs", "old_string": "1", "new_string": "2"})),
            completed("Edit", json!({"file_path": "a.rs", "old_string": "3", "new_string": "4"})),
        ];
        let files = stats_from_turn_entries(&entries);
        assert_eq!(files, vec![TurnFileChange { path: "a.rs".into(), added: 2, removed: 2 }]);
    }

    #[test]
    fn multi_edit_sums_its_edits_into_one_file() {
        let entries = vec![
            user(),
            completed(
                "MultiEdit",
                json!({"file_path": "m.rs", "edits": [
                    {"old_string": "a", "new_string": "b\nc"},
                    {"old_string": "d\ne", "new_string": "f"}]}),
            ),
        ];
        assert_eq!(
            stats_from_turn_entries(&entries),
            vec![TurnFileChange { path: "m.rs".into(), added: 3, removed: 3 }]
        );
    }

    #[test]
    fn apply_patch_counts_each_files_own_diff() {
        let entries = vec![
            user(),
            completed(
                "apply_patch",
                json!({"changes": [
                    {"path": "p.rs", "diff": "@@ -1 +1,2 @@\n ctx\n+added"},
                    {"path": "q.rs", "diff": "@@ -1,2 +1 @@\n-gone\n ctx"}]}),
            ),
        ];
        assert_eq!(
            stats_from_turn_entries(&entries),
            vec![
                TurnFileChange { path: "p.rs".into(), added: 1, removed: 0 },
                TurnFileChange { path: "q.rs".into(), added: 0, removed: 1 },
            ]
        );
    }

    #[test]
    fn only_this_turns_settled_edits_count() {
        let entries = vec![
            user(),
            completed("Edit", json!({"file_path": "prev.rs", "old_string": "a", "new_string": "b"})),
            // A new prompt starts a new turn — the previous turn's edit is history.
            user(),
            completed("Edit", json!({"file_path": "now.rs", "old_string": "a", "new_string": "b"})),
            // An unsettled edit hasn't changed anything yet.
            ThreadEntry::ToolCall(ToolCall::new(
                "pending",
                "Edit",
                json!({"file_path": "pending.rs", "old_string": "a", "new_string": "b"}),
            )),
        ];
        let files = stats_from_turn_entries(&entries);
        assert_eq!(files.len(), 1, "got {files:?}");
        assert_eq!(files[0].path, "now.rs");
    }

    #[test]
    fn a_conversational_turn_changes_nothing() {
        let entries = vec![user(), completed("Read", json!({"file_path": "a.rs"}))];
        assert!(stats_from_turn_entries(&entries).is_empty(), "no card for a turn with no edits");
    }
}
