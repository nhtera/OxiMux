//! What a row can answer without opening the session.
//!
//! F5's action table, as one function. The agents row asks it, and the phone
//! (P7) and the parent banner (P5) ask the same one: three surfaces offering
//! different buttons for the same pending request is how a person learns not
//! to trust any of them.
//!
//! Three rules decide everything here:
//!
//! - **One pending request, or none of this.** Two proposals and a single pair
//!   of buttons is a guess about which one Allow answers. The chat card is
//!   where several are told apart, so the row says Open instead.
//! - **A truncated argument is not a shown argument.** Inline Allow exists so
//!   a person can approve what they can see; a row shows one short line, so
//!   when the summary does not fit in it the row has not shown the arguments
//!   and must not offer to approve them. This is the row's half of SEC7 — the
//!   host's half is the digest it mints and re-checks.
//! - **Only a plain tool call.** A plan approval, a mode change, an MCP
//!   elicitation and a screen-control grant each carry something the row
//!   cannot render: a plan to read, a mode to understand, a server to
//!   identify, an app whose approval outlives this call. Those open.
//!
//! What a *question* offers follows the same logic from the other side: a
//! free-text question is answerable in one line, so the row offers Reply; one
//! with options is a pick among them, which belongs on the card that can draw
//! them.

use crate::attention::AttentionKind;
use crate::session_registry::{PendingPermissionView, PendingQuestionView};
use crate::thread::{PermissionKind, tool_target};

/// One inline control on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// Approve the one pending proposal.
    Allow,
    /// Refuse it.
    Reject,
    /// Answer in free text — a question's `response`, or a new prompt.
    Reply,
    /// This has been dealt with: stop asking.
    Dismiss,
}

/// What an inline answer is sent through, named precisely enough that the
/// dispatcher cannot send it anywhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineAnswer {
    /// Allow / Reject this proposal, echoing the digest it was shown with.
    /// Both travel together because the digest is only meaningful as the
    /// answer to *this* request.
    Permission { request_id: String, args_digest: String },
    /// Free text answers this question through `answer_question`'s `response`
    /// field. Never a `SendV2`: a prompt sent beside an unanswered question
    /// leaves the agent still waiting on it.
    Question { request_id: String },
    /// Free text is a new prompt: the session is waiting on a person, not on
    /// a request of its own.
    Prompt,
    /// Nothing is pending. Dismiss may still be offered — it is about the
    /// person's own list, not about the agent.
    Nothing,
}

/// What a row offers, and what answering it goes through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowActions {
    pub actions: Vec<RowAction>,
    pub answer: InlineAnswer,
    /// The one line the row shows above the buttons: what is actually being
    /// approved. `None` when there is nothing to describe.
    ///
    /// Minted here rather than read off the attention, because a health-derived
    /// permission carries **no reason at all** (`health::derive` returns
    /// `None` for a prompt) — the words have to come from the proposal.
    pub summary: Option<String>,
}

impl RowActions {
    /// A row that offers nothing: no alert, nothing pending it can answer, or
    /// no host to ask.
    pub fn nothing() -> Self {
        Self { actions: Vec::new(), answer: InlineAnswer::Nothing, summary: None }
    }

    fn with(actions: Vec<RowAction>, answer: InlineAnswer, summary: Option<String>) -> Self {
        Self { actions, answer, summary }
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn offers(&self, action: RowAction) -> bool {
        self.actions.contains(&action)
    }
}

/// How much of an argument summary a row can show before it is cut.
///
/// Measured against the 250px rail at its default width (inner ~226px) at the
/// 11px secondary size, which is the width the mock is drawn at and the
/// narrowest the rail goes is 220px. A summary longer than this is elided on
/// screen, and an elided argument is not one the person has seen.
pub const SUMMARY_BUDGET: usize = 56;

/// What `kind` can be answered from the row, given what the session actually
/// has pending.
///
/// The attention kind alone cannot decide this. `NeedsPermission` covers a
/// tool call, a plan approval, an auth prompt *and* a question with options,
/// so the proposals and the question are what tell them apart — which is why
/// this takes all three and not just the kind.
pub fn row_actions(
    kind: Option<AttentionKind>,
    permissions: &[PendingPermissionView],
    question: Option<&PendingQuestionView>,
) -> RowActions {
    let Some(kind) = kind else { return RowActions::nothing() };
    // A dismissible kind is asked of the evaluator, never listed again here:
    // a second list of kinds is how the row and the menu came to disagree
    // about what could be dismissed.
    if kind.is_dismissible() {
        return RowActions::with(vec![RowAction::Dismiss], InlineAnswer::Nothing, None);
    }
    if !kind.is_needs_you() {
        // Limited retries itself and an unread finish is read by reading it.
        return RowActions::nothing();
    }
    // A question first: it is the one that must not be answered with a
    // prompt, and a session can hold both (an `AskUserQuestion` is itself a
    // permission request on the wire, so it appears in both maps).
    if let Some(question) = question {
        return question_actions(question);
    }
    if let [only] = permissions {
        return permission_actions(only);
    }
    if permissions.len() > 1 {
        // Which one a single Allow answers is a guess.
        return RowActions::nothing();
    }
    // Waiting on a person with no request of its own: a reply is a new prompt.
    match kind {
        AttentionKind::NeedsInput => {
            RowActions::with(vec![RowAction::Reply], InlineAnswer::Prompt, None)
        }
        // A permission the handle no longer holds — answered elsewhere a
        // moment ago, or raised by a process that has been replaced. Offering
        // Allow for a proposal nobody has would be offering to approve
        // nothing.
        _ => RowActions::nothing(),
    }
}

/// A question's row: free text in, or open the card.
///
/// Public because the dispatch asks it again at the moment the reply is sent:
/// a question raised between the paint and the click must be answered as a
/// question rather than talked over by a prompt, and a question that cannot
/// be answered in one line must not be answered from the row at all.
pub fn question_actions(question: &PendingQuestionView) -> RowActions {
    // Any options at all and this is a pick, not a sentence. Rendering 2–4
    // labelled options with descriptions (and possibly multi-select) is what
    // the `QuestionCard` is for; a Reply box beside them would quietly answer
    // with free text instead of the option the person meant to choose.
    if question.questions.iter().any(|q| !q.options.is_empty()) {
        return RowActions::nothing();
    }
    // A secret answer never gets a row box: the row has no masking and the
    // redaction that keeps it out of the transcript lives on the chat.
    if question.questions.iter().any(|q| q.is_secret) {
        return RowActions::nothing();
    }
    let summary = question
        .questions
        .first()
        .map(|q| q.question.clone())
        .filter(|text| !text.trim().is_empty());
    RowActions::with(
        vec![RowAction::Reply],
        InlineAnswer::Question { request_id: question.request_id.clone() },
        summary,
    )
}

/// One proposal's row: Allow / Reject when the row can show what it is, else
/// nothing and the card opens.
fn permission_actions(pending: &PendingPermissionView) -> RowActions {
    // Only a plain tool call. Every other kind carries something a row cannot
    // put in front of someone.
    if pending.kind != PermissionKind::Tool {
        return RowActions::nothing();
    }
    let Some(summary) = summary_of(&pending.tool_name, &pending.input) else {
        return RowActions::nothing();
    };
    RowActions::with(
        vec![RowAction::Allow, RowAction::Reject],
        InlineAnswer::Permission {
            request_id: pending.request_id.clone(),
            args_digest: pending.args_digest.clone(),
        },
        Some(summary),
    )
}

/// The line the row shows for a proposal, or `None` when the row cannot show
/// it in full.
///
/// Both halves matter. With no target the row would read "Needs approval" and
/// nothing else, so Allow would approve something unnamed; over the budget it
/// would read a cut-off command, and the tail it hides is exactly where a
/// `--force` or a second path goes.
fn summary_of(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let target = tool_target(tool_name, input)?;
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    let line = format!("{tool_name}: {target}");
    (line.chars().count() <= SUMMARY_BUDGET).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::{AskQuestion, QuestionKind, QuestionOption};
    use serde_json::json;

    fn permission(kind: PermissionKind, tool: &str, input: serde_json::Value) -> PendingPermissionView {
        PendingPermissionView {
            request_id: "r1".into(),
            tool_name: tool.into(),
            kind,
            args_digest: crate::session_registry::args_digest(tool, kind, &input),
            input,
        }
    }

    fn question(options: Vec<&str>) -> PendingQuestionView {
        PendingQuestionView {
            request_id: "q1".into(),
            tool_use_id: Some("t1".into()),
            questions: vec![AskQuestion {
                id: "q-0".into(),
                header: "README".into(),
                question: "Which README?".into(),
                options: options
                    .into_iter()
                    .map(|label| QuestionOption {
                        label: label.into(),
                        description: String::new(),
                    })
                    .collect(),
                kind: QuestionKind::SingleSelect,
                other_allowed: true,
                is_secret: false,
            }],
        }
    }

    #[test]
    fn one_tool_proposal_the_row_can_show_offers_allow_and_reject() {
        let pending = permission(PermissionKind::Tool, "Bash", json!({"command": "pnpm db:reset"}));
        let actions =
            row_actions(Some(AttentionKind::NeedsPermission), std::slice::from_ref(&pending), None);
        assert_eq!(actions.actions, vec![RowAction::Allow, RowAction::Reject]);
        assert_eq!(actions.summary.as_deref(), Some("Bash: pnpm db:reset"));
        assert_eq!(
            actions.answer,
            InlineAnswer::Permission {
                request_id: "r1".into(),
                args_digest: pending.args_digest.clone(),
            },
            "the digest travels with the request it was minted for"
        );
    }

    #[test]
    fn two_proposals_offer_nothing_because_one_pair_of_buttons_is_a_guess() {
        let a = permission(PermissionKind::Tool, "Bash", json!({"command": "ls"}));
        let mut b = permission(PermissionKind::Tool, "Bash", json!({"command": "rm -rf build/"}));
        b.request_id = "r2".into();
        assert!(
            row_actions(Some(AttentionKind::NeedsPermission), &[a, b], None).is_empty(),
            "which one Allow answers would be a guess, so the card opens instead"
        );
    }

    #[test]
    fn an_argument_the_row_cannot_show_in_full_is_not_approvable_from_it() {
        // The tail a cut-off line hides is exactly where a second path or a
        // `--force` goes, so the row must not offer to approve what it has
        // elided.
        let long = format!("rm -rf {}", "a/very/deep/path/".repeat(6));
        assert!(long.chars().count() > SUMMARY_BUDGET);
        let pending = permission(PermissionKind::Tool, "Bash", json!({"command": long}));
        assert!(row_actions(Some(AttentionKind::NeedsPermission), &[pending], None).is_empty());
    }

    #[test]
    fn a_proposal_with_nothing_to_name_is_not_approvable_either() {
        // No target: the row would say "Needs approval" and no more, and
        // Allow would approve something unnamed.
        let pending = permission(PermissionKind::Tool, "Task", json!({"x": 1}));
        assert!(row_actions(Some(AttentionKind::NeedsPermission), &[pending], None).is_empty());
    }

    #[test]
    fn only_a_plain_tool_call_is_answerable_from_the_row() {
        // Each of these carries something the row cannot put in front of
        // someone: a plan to read, a mode to understand, a server to
        // identify, an app whose approval outlives this one call.
        for kind in [
            PermissionKind::Plan,
            PermissionKind::Mode,
            PermissionKind::Mcp,
            PermissionKind::Screen,
            PermissionKind::Other,
        ] {
            let pending = permission(kind, "Bash", json!({"command": "ls"}));
            assert!(
                row_actions(Some(AttentionKind::NeedsPermission), &[pending], None).is_empty(),
                "{kind:?} belongs on the card"
            );
        }
    }

    #[test]
    fn a_free_text_question_is_answered_from_the_row() {
        let q = question(vec![]);
        let actions = row_actions(Some(AttentionKind::NeedsPermission), &[], Some(&q));
        assert_eq!(actions.actions, vec![RowAction::Reply]);
        assert_eq!(actions.answer, InlineAnswer::Question { request_id: "q1".into() });
        assert_eq!(actions.summary.as_deref(), Some("Which README?"));
    }

    #[test]
    fn a_question_with_options_opens_instead_of_offering_a_text_box() {
        // A Reply box beside unrendered options would answer with free text
        // instead of the option the person meant to pick.
        let q = question(vec!["The root one", "The crate's"]);
        assert!(row_actions(Some(AttentionKind::NeedsPermission), &[], Some(&q)).is_empty());
    }

    #[test]
    fn a_secret_answer_never_gets_a_row_box() {
        let mut q = question(vec![]);
        q.questions[0].is_secret = true;
        assert!(
            row_actions(Some(AttentionKind::NeedsPermission), &[], Some(&q)).is_empty(),
            "the row has no masking and no redaction"
        );
    }

    #[test]
    fn a_question_beats_a_proposal_when_a_session_holds_both() {
        // An `AskUserQuestion` IS a permission request on the wire, so it is
        // in both maps. Answering it with Allow would approve the question
        // tool without answering the question.
        let q = question(vec![]);
        let also = permission(PermissionKind::Tool, "AskUserQuestion", json!({"questions": []}));
        let actions = row_actions(Some(AttentionKind::NeedsPermission), &[also], Some(&q));
        assert_eq!(actions.answer, InlineAnswer::Question { request_id: "q1".into() });
    }

    #[test]
    fn waiting_for_input_with_no_request_of_its_own_takes_a_prompt() {
        let actions = row_actions(Some(AttentionKind::NeedsInput), &[], None);
        assert_eq!(actions.actions, vec![RowAction::Reply]);
        assert_eq!(actions.answer, InlineAnswer::Prompt);
    }

    #[test]
    fn a_permission_the_handle_no_longer_holds_offers_nothing() {
        // Answered elsewhere a moment ago, or raised by a process since
        // replaced. Allow here would offer to approve nothing.
        assert!(row_actions(Some(AttentionKind::NeedsPermission), &[], None).is_empty());
    }

    #[test]
    fn a_failure_and_a_stall_offer_dismiss_and_nothing_to_answer() {
        for kind in [AttentionKind::Failed, AttentionKind::Stalled] {
            let actions = row_actions(Some(kind), &[], None);
            assert_eq!(actions.actions, vec![RowAction::Dismiss], "{kind:?}");
            assert_eq!(actions.answer, InlineAnswer::Nothing);
        }
    }

    #[test]
    fn the_informational_kinds_and_a_quiet_row_offer_nothing() {
        for kind in [AttentionKind::Limited, AttentionKind::FinishedUnread] {
            assert!(row_actions(Some(kind), &[], None).is_empty(), "{kind:?}");
        }
        assert!(row_actions(None, &[], None).is_empty());
    }

    #[test]
    fn a_dismissible_row_is_not_offered_a_stale_proposal_s_buttons() {
        // A failure outranks nothing here, but the proposal map can still
        // hold an entry from before the process died. Allow on it would
        // dispatch to a connection that is gone.
        let stale = permission(PermissionKind::Tool, "Bash", json!({"command": "ls"}));
        let actions = row_actions(Some(AttentionKind::Failed), &[stale], None);
        assert_eq!(actions.actions, vec![RowAction::Dismiss]);
    }
}
