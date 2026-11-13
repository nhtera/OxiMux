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
//! - **A label is not a description of the request.** Inline Allow exists so a
//!   person can approve what they can see, and the first version of this rule
//!   only checked the summary's *length* — so `Edit: notes.txt` passed, while
//!   `old_string` and `new_string`, which are the whole of what would happen,
//!   were never shown. The rule now asks whether the line accounts for the
//!   request ([`describes_whole_input`]), is a single line, and fits the row.
//!   This is the row's half of SEC7 — the host's half is the digest it mints
//!   and re-checks.
//! - **Only a plain tool call.** A plan approval, a mode change, an MCP
//!   elicitation and a screen-control grant each carry something the row
//!   cannot render: a plan to read, a mode to understand, a server to
//!   identify, an app whose approval outlives this call. Those open.
//!
//! What a *question* offers follows the same logic from the other side: a
//! single free-text question is answerable in one line, so the row offers
//! Reply. One with options is a pick among them, which belongs on the card
//! that can draw them — and a request carrying *several* questions cannot be
//! answered from a row at all, because one line of free text settles the
//! whole request and the questions after the first were never shown.
//!
//! **A row with no live session answers nothing.** It is passed in as such
//! rather than inferred from an empty pending list: a cockpit row waiting for
//! input looks exactly like a live chat with nothing pending, and offering it
//! Reply produced a button whose every press failed.

use crate::attention::AttentionKind;
use crate::session_registry::{PendingPermissionView, PendingQuestionView};
use crate::thread::{PermissionKind, describes_whole_input, tool_target};

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

/// How long an argument summary may be before the row stops offering to
/// approve it.
///
/// A character count is not a width, which is why the row **wraps** the
/// summary rather than clipping it: the budget then bounds how many lines it
/// takes rather than deciding whether the end is visible at all. Two lines of
/// the 11px secondary size at the 220px minimum is about this many
/// characters, so a summary within it is fully on screen at every rail width
/// and one beyond it opens the card.
pub const SUMMARY_BUDGET: usize = 56;

/// What a live session has waiting on a person.
///
/// `None` in [`row_actions`] means there is no session at all — a history row,
/// or a cockpit agent OxiMux reads out of a PTY. That is a different thing
/// from a live session with nothing pending, and conflating the two is what
/// offered a terminal row a Reply button that could never send.
#[derive(Debug, Clone, Copy)]
pub struct SessionPending<'a> {
    pub permissions: &'a [PendingPermissionView],
    pub question: Option<&'a PendingQuestionView>,
}

/// What `kind` can be answered from the row, given what the session actually
/// has pending.
///
/// The attention kind alone cannot decide this. `NeedsPermission` covers a
/// tool call, a plan approval, an auth prompt *and* a question with options,
/// so the proposals and the question are what tell them apart — which is why
/// this takes them and not just the kind.
pub fn row_actions(kind: Option<AttentionKind>, session: Option<SessionPending<'_>>) -> RowActions {
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
    // No session: nothing to answer and nowhere to send it. Dismiss was
    // handled above, which is the only thing a sessionless row can offer.
    let Some(session) = session else { return RowActions::nothing() };
    // A question first: it is the one that must not be answered with a
    // prompt, and a session can hold both (an `AskUserQuestion` is itself a
    // permission request on the wire, so it appears in both maps).
    if let Some(question) = session.question {
        return question_actions(question);
    }
    if let [only] = session.permissions {
        return permission_actions(only);
    }
    if session.permissions.len() > 1 {
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
    // One line of free text settles the WHOLE request — `response`
    // supersedes the per-question map — so a request carrying several
    // questions would have the ones after the first answered, or left
    // unanswered, without ever being shown here.
    let [only] = question.questions.as_slice() else {
        return RowActions::nothing();
    };
    let summary = Some(only.question.clone()).filter(|text| !text.trim().is_empty());
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
/// the request in full — in which case there is no inline Allow either.
///
/// Four things have to hold, and each was a way of approving something
/// unseen:
///
/// 1. **There is a target at all.** Without one the row reads "Needs
///    approval" and no more, so Allow would approve something unnamed.
/// 2. **The line accounts for the request.** `Edit: notes.txt` is a fine
///    card label and hides the replacement text entirely; the request has to
///    be one whose single line is all of it.
/// 3. **It is one line.** The row does not wrap past two, and a command with
///    an embedded newline would have everything after the break hidden.
/// 4. **It fits.** The tail a cut-off line hides is exactly where a
///    `--force` or a second path goes.
fn summary_of(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    if !describes_whole_input(tool_name, input) {
        return None;
    }
    let target = tool_target(tool_name, input)?;
    let target = target.trim();
    if target.is_empty() || target.contains(['\n', '\r']) {
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
        questions(vec![("Which README?", options)])
    }

    fn questions(each: Vec<(&str, Vec<&str>)>) -> PendingQuestionView {
        PendingQuestionView {
            request_id: "q1".into(),
            tool_use_id: Some("t1".into()),
            questions: each
                .into_iter()
                .enumerate()
                .map(|(ix, (text, options))| AskQuestion {
                    id: format!("q-{ix}"),
                    header: "README".into(),
                    question: text.into(),
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
                })
                .collect(),
        }
    }

    /// A live session holding `permissions` and `question`.
    fn live<'a>(
        permissions: &'a [PendingPermissionView],
        question: Option<&'a PendingQuestionView>,
    ) -> Option<SessionPending<'a>> {
        Some(SessionPending { permissions, question })
    }

    fn asked(kind: AttentionKind, session: Option<SessionPending<'_>>) -> RowActions {
        row_actions(Some(kind), session)
    }

    #[test]
    fn one_tool_proposal_the_row_can_show_offers_allow_and_reject() {
        let pending = permission(PermissionKind::Tool, "Bash", json!({"command": "pnpm db:reset"}));
        let actions = asked(AttentionKind::NeedsPermission, live(std::slice::from_ref(&pending), None));
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
            asked(AttentionKind::NeedsPermission, live(&[a, b], None)).is_empty(),
            "which one Allow answers would be a guess, so the card opens instead"
        );
    }

    #[test]
    fn a_label_that_hides_the_change_offers_no_allow() {
        // The review's finding, and the reason the length check alone was
        // not enough: `Edit: notes.txt` is a fine card label and shows none
        // of what would actually be written.
        let edit = permission(
            PermissionKind::Tool,
            "Edit",
            json!({"file_path": "notes.txt", "old_string": "a", "new_string": "b"}),
        );
        assert!(asked(AttentionKind::NeedsPermission, live(&[edit], None)).is_empty());
        // A patch names a file and not the diff going into it.
        let patch = permission(
            PermissionKind::Tool,
            "apply_patch",
            json!({"changes": [{"path": "a.rs", "kind": {"type": "update"}}]}),
        );
        assert!(asked(AttentionKind::NeedsPermission, live(&[patch], None)).is_empty());
    }

    #[test]
    fn a_multi_line_command_offers_no_allow_because_the_row_shows_one_line() {
        let script = permission(
            PermissionKind::Tool,
            "Bash",
            json!({"command": "git add -A\nrm -rf build/"}),
        );
        assert!(
            asked(AttentionKind::NeedsPermission, live(&[script], None)).is_empty(),
            "everything after the break would be hidden"
        );
    }

    #[test]
    fn an_argument_the_row_cannot_show_in_full_is_not_approvable_from_it() {
        // The tail a cut-off line hides is exactly where a second path or a
        // `--force` goes.
        let long = format!("rm -rf {}", "a/very/deep/path/".repeat(6));
        assert!(long.chars().count() > SUMMARY_BUDGET);
        let pending = permission(PermissionKind::Tool, "Bash", json!({"command": long}));
        assert!(asked(AttentionKind::NeedsPermission, live(&[pending], None)).is_empty());
    }

    #[test]
    fn a_proposal_with_nothing_to_name_is_not_approvable_either() {
        // No target: the row would say "Needs approval" and no more, and
        // Allow would approve something unnamed.
        let pending = permission(PermissionKind::Tool, "Task", json!({"x": 1}));
        assert!(asked(AttentionKind::NeedsPermission, live(&[pending], None)).is_empty());
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
                asked(AttentionKind::NeedsPermission, live(&[pending], None)).is_empty(),
                "{kind:?} belongs on the card"
            );
        }
    }

    #[test]
    fn a_free_text_question_is_answered_from_the_row() {
        let q = question(vec![]);
        let actions = asked(AttentionKind::NeedsPermission, live(&[], Some(&q)));
        assert_eq!(actions.actions, vec![RowAction::Reply]);
        assert_eq!(actions.answer, InlineAnswer::Question { request_id: "q1".into() });
        assert_eq!(actions.summary.as_deref(), Some("Which README?"));
    }

    #[test]
    fn a_question_with_options_opens_instead_of_offering_a_text_box() {
        // A Reply box beside unrendered options would answer with free text
        // instead of the option the person meant to pick.
        let q = question(vec!["The root one", "The crate's"]);
        assert!(asked(AttentionKind::NeedsPermission, live(&[], Some(&q))).is_empty());
    }

    #[test]
    fn several_questions_cannot_be_answered_by_one_line() {
        // One free-text `response` supersedes the whole per-question map, so
        // answering from the row would settle questions the row never showed.
        let q = questions(vec![("Which README?", vec![]), ("Which branch?", vec![])]);
        assert!(asked(AttentionKind::NeedsPermission, live(&[], Some(&q))).is_empty());
    }

    #[test]
    fn a_secret_answer_never_gets_a_row_box() {
        let mut q = question(vec![]);
        q.questions[0].is_secret = true;
        assert!(
            asked(AttentionKind::NeedsPermission, live(&[], Some(&q))).is_empty(),
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
        let actions = asked(AttentionKind::NeedsPermission, live(&[also], Some(&q)));
        assert_eq!(actions.answer, InlineAnswer::Question { request_id: "q1".into() });
    }

    #[test]
    fn waiting_for_input_with_no_request_of_its_own_takes_a_prompt() {
        let actions = asked(AttentionKind::NeedsInput, live(&[], None));
        assert_eq!(actions.actions, vec![RowAction::Reply]);
        assert_eq!(actions.answer, InlineAnswer::Prompt);
    }

    #[test]
    fn a_row_with_no_session_is_offered_nothing_to_send() {
        // The review's finding: a cockpit row parsed as "waiting for input"
        // looks exactly like a live chat with nothing pending, and a Reply
        // offered here had no connection to send through — every press
        // failed with "no live session to answer".
        assert!(asked(AttentionKind::NeedsInput, None).is_empty());
        assert!(asked(AttentionKind::NeedsPermission, None).is_empty());
        // Dismiss is still offered, because it is about the person's own
        // list rather than about reaching the agent.
        assert_eq!(asked(AttentionKind::Failed, None).actions, vec![RowAction::Dismiss]);
    }

    #[test]
    fn a_permission_the_handle_no_longer_holds_offers_nothing() {
        // Answered elsewhere a moment ago, or raised by a process since
        // replaced. Allow here would offer to approve nothing.
        assert!(asked(AttentionKind::NeedsPermission, live(&[], None)).is_empty());
    }

    #[test]
    fn a_failure_and_a_stall_offer_dismiss_and_nothing_to_answer() {
        for kind in [AttentionKind::Failed, AttentionKind::Stalled] {
            let actions = asked(kind, live(&[], None));
            assert_eq!(actions.actions, vec![RowAction::Dismiss], "{kind:?}");
            assert_eq!(actions.answer, InlineAnswer::Nothing);
        }
    }

    #[test]
    fn the_informational_kinds_and_a_quiet_row_offer_nothing() {
        for kind in [AttentionKind::Limited, AttentionKind::FinishedUnread] {
            assert!(asked(kind, live(&[], None)).is_empty(), "{kind:?}");
        }
        assert!(row_actions(None, live(&[], None)).is_empty());
    }

    #[test]
    fn a_dismissible_row_is_not_offered_a_stale_proposal_s_buttons() {
        // A failure outranks nothing here, but the proposal map can still
        // hold an entry from before the process died. Allow on it would
        // dispatch to a connection that is gone.
        let stale = permission(PermissionKind::Tool, "Bash", json!({"command": "ls"}));
        let actions = asked(AttentionKind::Failed, live(&[stale], None));
        assert_eq!(actions.actions, vec![RowAction::Dismiss]);
    }
}
