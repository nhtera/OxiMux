//! Transcript snapshot harness — the fidelity floor under the agent mappers.
//!
//! Written as the safety net for a five-way `Assembler` extraction, which was
//! then declined on measurement: the mappers turned out not to be five copies
//! of one lifecycle (Claude's decoder is stateless, Codex is item-lifecycle
//! native, and the one real duplicate already shares `snapshot_diff`). The
//! harness outlived the plan that motivated it, because the evidence behind
//! that plan — nine rounds of render-fidelity work — was never about the
//! assembler. It was about mapper changes altering what the user sees without
//! failing anything.
//!
//! What is pinned is the folded [`ChatThread`], not the event stream. That is
//! deliberate and still load-bearing: event coalescing remains a legitimate
//! future change that would rewrite the stream without moving a pixel, and a
//! pinned `Vec<ThreadEvent>` would report it as a regression. The fold is where
//! "rendered output is unchanged" is actually decidable, because it is what
//! every render path reads.
//!
//! Behind the `test-support` feature so none of this ships: `oximux-agents`
//! turns it on in dev-dependencies so both crates pin transcripts identically
//! rather than each growing its own harness.
//!
//! Regenerate after an *intended* change with
//! `UPDATE_TRANSCRIPT_SNAPSHOTS=1 cargo test`, then read the diff before
//! committing it — a snapshot accepted without reading is worse than no
//! snapshot, because it looks like coverage.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::entry::{ChatImage, ThreadEntry};
use super::event::{ThreadEvent, TurnUsage};
use super::state::ChatThread;

/// Environment variable that rewrites snapshots instead of asserting them.
pub const UPDATE_VAR: &str = "UPDATE_TRANSCRIPT_SNAPSHOTS";

/// The render-visible state of a folded transcript.
///
/// Deliberately a curated projection rather than all of `ChatThread`: fields
/// the fold does not own (ephemeral slash-command descriptions, the plan
/// panel's live cache) would add churn without adding safety.
#[derive(Serialize)]
struct TranscriptSnapshot<'a> {
    session_id: &'a Option<String>,
    model: &'a Option<String>,
    permission_mode: &'a Option<String>,
    turn_active: bool,
    compacting: bool,
    title: &'a Option<String>,
    last_error: &'a Option<String>,
    last_summary: &'a Option<String>,
    /// The settled turn's token/cost breakdown. Included because it renders in
    /// the transcript footer — and because leaving it out made this harness
    /// vacuous: a mutant that dropped `usage` from every `TurnEnded` passed all
    /// thirteen snapshots. A projection is only a safety net over the fields it
    /// actually projects.
    usage: &'a Option<TurnUsage>,
    entries: Vec<ThreadEntry>,
}

/// Fold `events` and render the result as stable pretty JSON.
pub fn transcript_snapshot(events: &[ThreadEvent]) -> String {
    let mut thread = ChatThread::default();
    for ev in events {
        thread.apply(ev);
    }
    snapshot_of(&thread)
}

/// Render an already-folded thread.
pub fn snapshot_of(thread: &ChatThread) -> String {
    let snap = TranscriptSnapshot {
        session_id: &thread.session_id,
        model: &thread.model,
        permission_mode: &thread.permission_mode,
        turn_active: thread.turn_active,
        compacting: thread.compacting,
        title: &thread.title,
        last_error: &thread.last_error,
        last_summary: &thread.last_summary,
        usage: &thread.usage,
        entries: thread.entries.iter().cloned().map(redact_entry).collect(),
    };
    let mut text = serde_json::to_string_pretty(&snap).expect("transcript snapshot serializes");
    text.push('\n');
    text
}

/// Replace image payloads with a length+hash marker.
///
/// A screenshot's base64 runs to hundreds of kilobytes. Left inline it would
/// make the snapshot unreadable, and an unreadable snapshot gets regenerated
/// blindly — which is the failure mode this harness exists to prevent. The
/// marker still changes whenever the bytes change, so fidelity is kept where it
/// matters and only reviewability is traded away.
fn redact_entry(entry: ThreadEntry) -> ThreadEntry {
    fn redact(images: &mut Vec<ChatImage>) {
        for img in images {
            img.data = format!("<{} bytes, fnv={:016x}>", img.data.len(), fnv1a(&img.data));
        }
    }
    let mut entry = entry;
    match &mut entry {
        ThreadEntry::User { images, .. } => redact(images),
        ThreadEntry::ToolCall(call) => {
            redact(&mut call.images);
            // A tool call's `input`/`structured` are free-form `Value`s, and
            // their key order is a build-configuration detail — see
            // [`canonical`].
            call.input = canonical(std::mem::take(&mut call.input));
            call.structured = call.structured.take().map(canonical);
        }
        ThreadEntry::Assistant(_)
        | ThreadEntry::ContextCompaction { .. }
        | ThreadEntry::TurnDiff { .. } => {}
    }
    entry
}

/// Sort every object key, recursively.
///
/// `serde_json` maps preserve *insertion* order when anything in the build
/// graph turns on `preserve_order` (something in this workspace does — the
/// lockfile shows `serde_json` pulling `indexmap`) and sort keys when nothing
/// does. Cargo unifies features per build, so the same fixture serialized
/// differently depending on whether `agent-core` was built alone or as part of
/// the workspace, and the snapshot failed only in the full run.
///
/// Sorting here makes the snapshot a property of the transcript rather than of
/// the build graph. Found the honest way: the workspace suite failed while the
/// crate suite passed.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// FNV-1a. Inline rather than a dependency: `agent-core` is deliberately
/// dep-minimal and mobile-portable, and this is a change detector, not a
/// security primitive.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Assert the folded transcript matches the snapshot at `path`.
///
/// Writes the file (creating parent directories) when [`UPDATE_VAR`] is set, so
/// a new fixture is pinned by running the suite once.
pub fn assert_transcript_snapshot(path: impl AsRef<Path>, events: &[ThreadEvent]) {
    assert_snapshot_text(path, &transcript_snapshot(events));
}

/// [`assert_transcript_snapshot`] for a caller that folded the thread itself
/// (an agent whose replay helper drives extra state into it).
pub fn assert_thread_snapshot(path: impl AsRef<Path>, thread: &ChatThread) {
    assert_snapshot_text(path, &snapshot_of(thread));
}

fn assert_snapshot_text(path: impl AsRef<Path>, actual: &str) {
    let path = path.as_ref();
    if std::env::var_os(UPDATE_VAR).is_some() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("snapshot directory");
        }
        std::fs::write(path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing transcript snapshot {}: {err}\n\
             Create it with `{UPDATE_VAR}=1 cargo test`, then READ the generated \
             file before committing it.",
            path.display()
        )
    });
    if expected == actual {
        return;
    }
    panic!(
        "transcript changed for {}\n\n{}\n\n\
         If this change is intended, regenerate with `{UPDATE_VAR}=1 cargo test` \
         and name the change in the commit. If it is not, a mapper change \
         altered what the user sees.",
        path.display(),
        first_difference(&expected, actual)
    );
}

/// The first differing line with a little context — enough to see what moved
/// without printing two full transcripts into the test output.
///
/// Reports a difference `str::lines()` cannot see *as such*, rather than
/// rendering a window for it. `lines()` discards `\r` and the trailing newline,
/// so two strings that differ only in those compare equal line by line: the
/// scan finds nothing, and a naive fallback prints the tail of each side —
/// two halves that are character-identical, under a message saying the
/// transcript changed. That reads like a bug in the fold, and the natural
/// response is to regenerate the snapshot, which "fixes" it by committing the
/// wrong bytes and silently destroys the byte-exactness this harness exists to
/// provide. It has happened once already (CRLF on the Windows runner), so the
/// invisible cases are named here instead of being drawn.
fn first_difference(expected: &str, actual: &str) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();

    if let Some(at) = exp.iter().zip(&act).position(|(a, b)| a != b) {
        return window_around(at, &exp, &act);
    }

    // Every line the two share is equal, so the difference is either past the
    // end of the shorter side or in bytes `lines()` threw away.
    if exp.len() != act.len() {
        return format!(
            "{}\n\n(expected has {} lines, actual has {} — they agree up to line {})",
            window_around(exp.len().min(act.len()), &exp, &act),
            exp.len(),
            act.len(),
            exp.len().min(act.len())
        );
    }

    format!(
        "every line is equal, so the difference is in bytes `lines()` discards:\n\
         {}\n\
         This is a line-ending or trailing-newline difference, NOT a change to \
         the transcript. Do NOT regenerate with `{UPDATE_VAR}=1` — that would \
         commit the wrong bytes. Fix it where it comes from: a checkout that \
         rewrote the file (see the `text eol=lf` rules in `.gitattributes`) or \
         an editor that added a final newline.",
        invisible_differences(expected, actual)
    )
}

/// The concrete evidence for a difference no rendered line can show.
fn invisible_differences(expected: &str, actual: &str) -> String {
    let describe = |s: &str| {
        let endings = match (s.contains("\r\n"), s.contains('\n')) {
            (true, _) => "CRLF",
            (false, true) => "LF",
            (false, false) => "no line breaks",
        };
        let trailing = if s.ends_with('\n') { "yes" } else { "no" };
        format!("{endings}, trailing newline: {trailing}, {} bytes", s.len())
    };
    format!(
        "  expected: {}\n  actual:   {}",
        describe(expected),
        describe(actual)
    )
}

fn window_around(at: usize, exp: &[&str], act: &[&str]) -> String {
    let from = at.saturating_sub(3);
    let window = |lines: &[&str]| {
        lines
            .iter()
            .enumerate()
            .skip(from)
            .take(7)
            .map(|(i, l)| format!("  {:>4} | {l}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "first difference at line {}\n--- expected ---\n{}\n--- actual ---\n{}",
        at + 1,
        window(exp),
        window(act)
    )
}

#[cfg(test)]
mod tests {
    use super::first_difference;

    /// The failure that actually shipped: a CRLF checkout of an LF snapshot.
    ///
    /// The old reporter answered this with "first difference at line 4" and two
    /// windows that printed identically, because `lines()` strips `\r`.
    #[test]
    fn a_crlf_checkout_is_named_rather_than_drawn() {
        let report = first_difference("a\r\nb\r\nc\r\n", "a\nb\nc\n");

        assert!(report.contains("every line is equal"), "{report}");
        assert!(report.contains("expected: CRLF"), "{report}");
        assert!(report.contains("actual:   LF"), "{report}");
        // The whole point: do not send the reader to the regenerate command.
        assert!(report.contains("Do NOT regenerate"), "{report}");
        assert!(!report.contains("first difference at line"), "{report}");
    }

    #[test]
    fn a_trailing_newline_difference_is_named_too() {
        let report = first_difference("a\nb\n", "a\nb");

        assert!(report.contains("every line is equal"), "{report}");
        assert!(report.contains("trailing newline: yes"), "{report}");
        assert!(report.contains("trailing newline: no"), "{report}");
    }

    /// Negative control. A reporter that called everything invisible would pass
    /// both tests above, so a real content change must still be drawn.
    #[test]
    fn a_real_content_change_is_still_drawn_with_a_window() {
        let report = first_difference("a\nb\nc\n", "a\nB\nc\n");

        assert!(report.contains("first difference at line 2"), "{report}");
        assert!(report.contains("     2 | b"), "{report}");
        assert!(report.contains("     2 | B"), "{report}");
        assert!(!report.contains("every line is equal"), "{report}");
    }

    /// A truncated side shares every line it has, so it reaches the same branch
    /// as the invisible cases — but it is a genuine content difference and must
    /// not be reported as line endings.
    #[test]
    fn a_shorter_actual_reports_the_line_counts_not_line_endings() {
        let report = first_difference("a\nb\nc\n", "a\nb\n");

        assert!(report.contains("expected has 3 lines, actual has 2"), "{report}");
        assert!(!report.contains("every line is equal"), "{report}");
    }
}
