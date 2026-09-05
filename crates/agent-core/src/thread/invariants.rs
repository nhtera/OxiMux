//! Transcript invariants every agent must satisfy, checked against the fold.
//!
//! Nine rounds of render-fidelity work went into the agent mappers, and the
//! recurring cause was not that any one mapper was wrong — it was that each
//! decides lifecycle for itself, so the same rule holds in four mappers and
//! quietly fails in the fifth. Writing the rules down once, and running them
//! against every agent's captured fixtures, turns that class from "someone
//! notices in the UI months later" into a test failure.
//!
//! These are properties of the **folded transcript**, deliberately not of the
//! event stream. A mapper is free to emit whatever sequence its dialect implies;
//! what it may not do is leave the user looking at something incoherent.
//!
//! Behind `test-support`, like [`super::snapshot`] — this is a test oracle, not
//! shipping code.

use super::entry::ThreadEntry;
use super::state::ChatThread;
use super::tool_call::ToolCallStatus;

/// A transcript state no agent should ever produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// A tool row is still rendering as in-flight after the turn settled.
    ///
    /// This is the "terminalize before the terminal event" rule. A spinner that
    /// never resolves is the single most visible transcript bug: the turn is
    /// over, the agent has moved on, and one card spins forever with no way for
    /// the user to tell whether it succeeded.
    TransientRowAfterTurnSettled { index: usize, id: String, name: String, status: String },
    /// Two rows claim the same tool-call id.
    ///
    /// Rows are deduped by identity. Two rows for one id means a second
    /// `ToolCallStarted` opened a fresh card instead of updating the open one,
    /// so the user sees the same call twice, usually with one stuck pending.
    DuplicateToolCallId { first: usize, second: usize, id: String },
    /// A compaction divider with no summary text.
    ///
    /// The divider renders as a centered rule with its summary; with an empty
    /// summary the user gets a bare line across the transcript and no
    /// indication that history was truncated behind it.
    EmptyCompactionDivider { index: usize },
    /// A turn-diff card listing no files.
    ///
    /// The card exists to say what a turn changed, and is only meant to be
    /// pushed when a turn changed something. With no files it renders as an
    /// empty card claiming a turn edited nothing — worse than absent, because
    /// it asserts something false.
    EmptyTurnDiffCard { index: usize },
    /// An assistant row folded to nothing at all.
    ///
    /// An empty bubble renders as a blank gap. It usually means text was
    /// accumulated under one key and settled under another, so the visible text
    /// went to a row that no longer exists.
    EmptyAssistantRow { index: usize },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TransientRowAfterTurnSettled { index, id, name, status } => write!(
                f,
                "entry {index}: tool `{name}` (id {id}) is still {status} after the turn settled"
            ),
            Self::DuplicateToolCallId { first, second, id } => {
                write!(f, "entries {first} and {second} share tool-call id {id}")
            }
            Self::EmptyAssistantRow { index } => {
                write!(f, "entry {index}: assistant row has neither text nor thinking")
            }
            Self::EmptyCompactionDivider { index } => {
                write!(f, "entry {index}: compaction divider carries no summary")
            }
            Self::EmptyTurnDiffCard { index } => {
                write!(f, "entry {index}: turn-diff card lists no files")
            }
        }
    }
}

/// Whether a status still renders as in-flight.
///
/// `WaitingForConfirmation` and `AwaitingAnswer` are deliberately included: both
/// render as a card awaiting the *user*, and a settled turn means nobody is
/// going to answer them. Leaving one open after the turn ends is the same
/// dead-end as a spinner.
fn is_transient(status: &ToolCallStatus) -> bool {
    matches!(
        status,
        ToolCallStatus::Pending
            | ToolCallStatus::InProgress
            | ToolCallStatus::WaitingForConfirmation(_)
            | ToolCallStatus::AwaitingAnswer(_)
    )
}

fn status_name(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in-progress",
        ToolCallStatus::WaitingForConfirmation(_) => "awaiting confirmation",
        ToolCallStatus::AwaitingAnswer(_) => "awaiting an answer",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed(_) => "failed",
        ToolCallStatus::Rejected => "rejected",
        ToolCallStatus::Canceled => "canceled",
    }
}

/// Check a folded transcript, returning every violation found.
///
/// `turn_settled` says whether a turn actually ran and finished — the
/// transient-row rule only applies once nothing more is coming.
///
/// Derive it from having *observed a `TurnEnded`*, never from
/// `!thread.turn_active`. An idle thread also has `turn_active == false`, so a
/// capture that never opens a turn (a bare permission request, say) would be
/// read as "settled" and every legitimately-open card reported as a violation.
/// That mistake produced this checker's first failure, against a one-line
/// fixture holding nothing but a `can_use_tool` request.
pub fn check(thread: &ChatThread, turn_settled: bool) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen_ids: Vec<(usize, &str)> = Vec::new();

    for (index, entry) in thread.entries.iter().enumerate() {
        match entry {
            ThreadEntry::ToolCall(call) => {
                if turn_settled && is_transient(&call.status) {
                    out.push(Violation::TransientRowAfterTurnSettled {
                        index,
                        id: call.id.clone(),
                        name: call.name.clone(),
                        status: status_name(&call.status).to_string(),
                    });
                }
                // An empty id is a legitimate "this dialect has no join key"
                // marker, not an identity — several would collide spuriously.
                if !call.id.is_empty()
                    && let Some((first, _)) = seen_ids.iter().find(|(_, id)| *id == call.id)
                {
                    out.push(Violation::DuplicateToolCallId {
                        first: *first,
                        second: index,
                        id: call.id.clone(),
                    });
                }
                seen_ids.push((index, &call.id));
            }
            ThreadEntry::Assistant(msg) if msg.is_empty() => {
                out.push(Violation::EmptyAssistantRow { index });
            }
            ThreadEntry::ContextCompaction { summary } if summary.trim().is_empty() => {
                out.push(Violation::EmptyCompactionDivider { index });
            }
            ThreadEntry::TurnDiff { files, .. } if files.is_empty() => {
                out.push(Violation::EmptyTurnDiffCard { index });
            }
            _ => {}
        }
    }
    out
}

/// Panic with every violation, naming `label` (the fixture) so a failure says
/// which capture broke rather than only which rule.
pub fn assert_holds(label: &str, thread: &ChatThread, turn_settled: bool) {
    let violations = check(thread, turn_settled);
    assert!(
        violations.is_empty(),
        "{label}: {} transcript invariant violation(s)\n{}",
        violations.len(),
        violations.iter().map(|v| format!("  - {v}")).collect::<Vec<_>>().join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::entry::AssistantMessage;
    use crate::thread::tool_call::ToolCall;
    use serde_json::json;

    fn call(id: &str, status: ToolCallStatus) -> ThreadEntry {
        let mut c = ToolCall::new(id, "Bash", json!({}));
        c.status = status;
        ThreadEntry::ToolCall(c)
    }

    fn thread(entries: Vec<ThreadEntry>) -> ChatThread {
        // Built by mutation rather than a struct literal: `ChatThread` keeps
        // its streaming cursors private, and this checker only reads `entries`.
        let mut t = ChatThread::default();
        t.entries = entries;
        t
    }

    #[test]
    fn a_settled_turn_with_only_terminal_rows_is_clean() {
        let t = thread(vec![
            call("a", ToolCallStatus::Completed),
            call("b", ToolCallStatus::Failed("boom".into())),
            call("c", ToolCallStatus::Rejected),
            call("d", ToolCallStatus::Canceled),
        ]);
        assert_eq!(check(&t, true), Vec::new());
    }

    #[test]
    fn a_row_still_in_flight_after_the_turn_settled_is_a_violation() {
        for status in [ToolCallStatus::Pending, ToolCallStatus::InProgress] {
            let t = thread(vec![call("a", status)]);
            assert_eq!(check(&t, true).len(), 1, "a spinner must not survive the turn");
        }
    }

    /// The same rows are fine while the turn is still running — that is just a
    /// tool that has not finished yet.
    #[test]
    fn an_in_flight_row_during_a_live_turn_is_not_a_violation() {
        let t = thread(vec![call("a", ToolCallStatus::InProgress)]);
        assert_eq!(check(&t, false), Vec::new());
    }

    #[test]
    fn two_rows_sharing_a_tool_call_id_are_a_violation() {
        let t = thread(vec![
            call("dup", ToolCallStatus::Completed),
            call("dup", ToolCallStatus::Completed),
        ]);
        assert_eq!(
            check(&t, true),
            vec![Violation::DuplicateToolCallId { first: 0, second: 1, id: "dup".into() }]
        );
    }

    /// A dialect with no join key leaves the id empty. Several such rows are
    /// normal and must not read as one row duplicated — treating "unknown" as
    /// an identity is how a real duplicate gets buried in false positives.
    #[test]
    fn rows_without_an_id_are_not_duplicates_of_each_other() {
        let t = thread(vec![
            call("", ToolCallStatus::Completed),
            call("", ToolCallStatus::Completed),
            call("", ToolCallStatus::Completed),
        ]);
        assert_eq!(check(&t, true), Vec::new());
    }

    #[test]
    fn an_assistant_row_with_no_text_and_no_thinking_is_a_violation() {
        let t = thread(vec![ThreadEntry::Assistant(AssistantMessage::default())]);
        assert_eq!(check(&t, true), vec![Violation::EmptyAssistantRow { index: 0 }]);
    }

    #[test]
    fn an_assistant_row_holding_only_thinking_still_renders() {
        let msg = AssistantMessage { text: String::new(), thinking: "hmm".into() };
        let t = thread(vec![ThreadEntry::Assistant(msg)]);
        assert_eq!(check(&t, true), Vec::new());
    }

    #[test]
    fn a_compaction_divider_with_no_summary_is_a_violation() {
        let t = thread(vec![ThreadEntry::ContextCompaction { summary: "   ".into() }]);
        assert_eq!(check(&t, true), vec![Violation::EmptyCompactionDivider { index: 0 }]);
    }

    #[test]
    fn a_compaction_divider_with_a_summary_is_fine() {
        let t = thread(vec![ThreadEntry::ContextCompaction { summary: "older messages".into() }]);
        assert_eq!(check(&t, true), Vec::new());
    }

    #[test]
    fn a_turn_diff_card_listing_no_files_is_a_violation() {
        let t = thread(vec![ThreadEntry::TurnDiff { files: Vec::new(), diff: None }]);
        assert_eq!(check(&t, true), vec![Violation::EmptyTurnDiffCard { index: 0 }]);
    }

    /// Every violation is reported, not just the first — a mapper that breaks
    /// one rule usually breaks it everywhere, and fixing them one test-run at a
    /// time is how a refactor stalls.
    #[test]
    fn all_violations_are_reported_together() {
        let t = thread(vec![
            call("x", ToolCallStatus::InProgress),
            call("x", ToolCallStatus::Completed),
            ThreadEntry::Assistant(AssistantMessage::default()),
        ]);
        assert_eq!(check(&t, true).len(), 3);
    }
}
