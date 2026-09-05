//! Transcript snapshot harness — the safety net for the assembler extraction.
//!
//! The assembler moves lifecycle decisions (id minting, turn/item boundaries,
//! close dedupe, text accumulation, coalescing) out of five agent mappers and
//! into one place. The thing that must not change while that happens is what
//! the user sees, and *that* is the folded [`ChatThread`] — not the event
//! stream. Coalescing deliberately alters the event stream: it merges deltas,
//! so a pinned `Vec<ThreadEvent>` would report a false regression on the first
//! migration. The fold is where "rendered output is unchanged" is actually
//! decidable, because it is what every render path reads.
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
        ThreadEntry::ToolCall(call) => redact(&mut call.images),
        ThreadEntry::Assistant(_)
        | ThreadEntry::ContextCompaction { .. }
        | ThreadEntry::TurnDiff { .. } => {}
    }
    entry
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
         and name the change in the phase log. If it is not, the assembler \
         altered what the user sees.",
        path.display(),
        first_difference(&expected, actual)
    );
}

/// The first differing line with a little context — enough to see what moved
/// without printing two full transcripts into the test output.
fn first_difference(expected: &str, actual: &str) -> String {
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    let at = exp.iter().zip(&act).position(|(a, b)| a != b).unwrap_or(exp.len().min(act.len()));
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
        window(&exp),
        window(&act)
    )
}
