//! Answering a session from somewhere other than its chat.
//!
//! The host owns this for the same reason it owns the attention: a surface
//! that worked out for itself what a row could answer, and then sent the
//! answer by its own route, would be a second definition of both. The row
//! asks [`HostCore::row_actions`] what to draw and calls one of the three
//! methods below; the phone (P7) will call the same three.
//!
//! **What is offered is decided twice, deliberately.** Once when the row is
//! painted, and again inside the dispatch — because a frame is a long time.
//! The paint is a snapshot: by the time a click lands, the proposal may have
//! been answered in the chat, the process may have been replaced, or the
//! question may have been withdrawn. So the dispatch re-reads the handle and
//! refuses anything that is no longer the thing the person was shown, rather
//! than trusting what the click carried. This is the "re-validate at flush"
//! rule B13 set for the banner, applied to the answer instead of the alert.
//!
//! The approval itself is checked by
//! [`SessionHandle::resolve_permission`](oximux_agents::session_registry::SessionHandle::resolve_permission),
//! which refuses a digest that does not match the arguments it still holds.
//! Nothing here re-implements that; the digest is carried through untouched,
//! because a surface that recomputed it would be hashing what it rendered
//! rather than what the agent asked for.

use oximux_agents::attention::Attention;
use oximux_agents::row_actions::{
    InlineAnswer, RowActions, SessionPending, question_actions, row_actions,
};
use oximux_agents::session_registry::{PendingPermissionView, SessionHandle};
use oximux_agents::thread::{PermissionDecision, QuestionAnswer, QuestionAnswers};

use crate::HostCore;

/// The message a denial carries. The agent sees this, so it says who refused
/// rather than reading as a tool failure.
const REJECTED: &str = "Denied by user";

impl HostCore {
    /// What `target`'s row can answer inline, given what it is asking for.
    ///
    /// `attention` is the row's own evaluated attention — passed in rather
    /// than re-read, so the buttons and the accent bar on one row are
    /// answering the same moment's question.
    ///
    /// A target with no live session is passed through as having none, not as
    /// a session with an empty pending list: a cockpit row parsed as
    /// "waiting for input" looks exactly like a live chat with nothing
    /// pending, and offering it Reply produced a button whose every press
    /// failed. Its Dismiss still comes from the kind, so the row is not mute.
    pub fn row_actions(&self, target: &str, attention: Option<&Attention>) -> RowActions {
        let kind = attention.map(|a| a.kind);
        let Some(handle) = self.session_of(target) else { return row_actions(kind, None) };
        let permissions = handle.outstanding_permissions();
        let question = handle.pending_question();
        row_actions(kind, Some(SessionPending { permissions: &permissions, question: question.as_ref() }))
    }

    /// Approve the proposal the row was showing.
    ///
    /// `args_digest` is the one minted with that proposal and echoed back
    /// unchanged. Carrying it is the whole point of this method:
    /// [`SessionHandle::resolve_permission`] only checks a decision that has
    /// one, so an approval sent from here without it would be accepted
    /// against arguments nobody was shown.
    ///
    /// Nothing here re-checks it. That check lives in `resolve_permission`,
    /// which holds the proposal and the gate under one lock; a copy of the
    /// comparison out here would be a second definition of the rule, and a
    /// test could not tell which one refused.
    ///
    /// `Ok(false)` means it was already answered — a second click, or the
    /// chat got there first — which is not a failure and needs no message.
    ///
    /// [`SessionHandle::resolve_permission`]: oximux_agents::session_registry::SessionHandle::resolve_permission
    pub fn allow_from_row(
        &self,
        target: &str,
        request_id: &str,
        args_digest: &str,
    ) -> anyhow::Result<bool> {
        let (handle, pending) = self.proposal(target, request_id)?;
        // The input comes from the handle, not from the click: the transport
        // requires the proposal echoed back, and echoing what a surface held
        // would send a stale copy. If the proposal changed between this read
        // and the dispatch, the digest below is what refuses it.
        handle.resolve_permission(
            request_id,
            PermissionDecision::allow(pending.input)
                .with_args_digest(Some(args_digest.to_string())),
        )
    }

    /// Refuse the proposal the row was showing.
    ///
    /// A denial carries no digest: there is nothing to protect. Approving the
    /// wrong request does something; refusing the wrong one is at worst a
    /// refusal the agent reports, and demanding a match would turn "the
    /// arguments changed under you" into a row you cannot say no from.
    pub fn reject_from_row(&self, target: &str, request_id: &str) -> anyhow::Result<bool> {
        self.live_session(target)?
            .resolve_permission(request_id, PermissionDecision::deny(REJECTED))
    }

    /// Answer in free text: a question's `response`, or a new prompt.
    ///
    /// `answer` is what the row was painted for, and the send happens only
    /// where that is still what is waiting. This is the digest rule applied
    /// to a question: an answer has to name what it answers, or a reply
    /// typed for one question lands on whichever question replaced it — and
    /// the person never saw that one. The same holds the other way: text
    /// typed at a session merely waiting for input must not be dropped into
    /// a question raised since, because it was not written as that answer.
    ///
    /// Sending also records the look (B15): a session you have just answered
    /// can never sit in "unread", and the person has plainly seen it.
    pub fn reply_from_row(
        &self,
        target: &str,
        answer: &InlineAnswer,
        text: &str,
    ) -> anyhow::Result<bool> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(false);
        }
        let handle = self.live_session(target)?;
        // One read of the question, used for the check AND the answer: two
        // reads would let a withdrawn question be answered by its
        // successor's id.
        let pending = handle.pending_question();
        match (answer, pending) {
            // The question the row was showing, still the one waiting.
            (InlineAnswer::Question { request_id }, Some(question))
                if &question.request_id == request_id =>
            {
                // Re-decided by the same table the row was drawn with: a
                // question that has grown options or a second question since
                // the paint is no longer answerable from a row.
                if !matches!(question_actions(&question).answer, InlineAnswer::Question { .. }) {
                    anyhow::bail!(
                        "{target} is waiting on a choice, not on a sentence; \
                         open the agent to answer it"
                    );
                }
                let sent = handle.answer_question(
                    request_id,
                    &question.questions,
                    &free_text(&question.questions, text),
                )?;
                self.viewed_if(sent, target);
                Ok(sent)
            }
            // Painted for a question that is no longer the one waiting.
            (InlineAnswer::Question { request_id }, _) => anyhow::bail!(
                "question {request_id} is no longer the one waiting on an answer; \
                 open the agent to see what it is asking"
            ),
            // Painted as a prompt, and a question has appeared since. Both
            // routes would be wrong — a prompt leaves the question waiting,
            // and answering it uses words written for something else.
            (_, Some(_)) => anyhow::bail!(
                "{target} is asking a question now; \
                 open the agent so you can see what it wants"
            ),
            // Painted as a prompt, and that is still what it is.
            (_, None) => {
                handle.send_prompt(text, &[])?;
                self.viewed_if(true, target);
                Ok(true)
            }
        }
    }

    /// Record the look for a send that happened.
    fn viewed_if(&self, sent: bool, target: &str) {
        if sent {
            self.mark_viewed(target);
        }
    }

    /// The handle and the proposal `request_id` names, or why there is none.
    ///
    /// This is only about *existence*: whether there is still something to
    /// echo back. Whether it is the same something the person was shown is
    /// the digest's job, settled in `resolve_permission`.
    ///
    /// The message is the one a person reads in a toast, so it says what to
    /// do next rather than naming an internal state.
    fn proposal(
        &self,
        target: &str,
        request_id: &str,
    ) -> anyhow::Result<(std::sync::Arc<SessionHandle>, PendingPermissionView)> {
        let handle = self.live_session(target)?;
        let pending = handle.pending_permission(request_id).ok_or_else(|| {
            anyhow::anyhow!(
                "request {request_id} is no longer waiting on an answer; \
                 open the agent to see where it stands"
            )
        })?;
        Ok((handle, pending))
    }

    /// The registry handle `target` names, or a sentence saying there is none.
    fn live_session(&self, target: &str) -> anyhow::Result<std::sync::Arc<SessionHandle>> {
        self.session_of(target)
            .ok_or_else(|| anyhow::anyhow!("{target} has no live session to answer"))
    }
}


/// One line of free text, in both dialects a backend might read it in.
///
/// Claude's `updated_input_json` takes `response` and ignores the map;
/// Codex's `codex_answers_json` reads **only** the per-question map and
/// ignores `response`, so a `response`-only answer reached a Codex session as
/// `{"answers":{}}` — the text dropped, the request settled, and the row's
/// box closing as though it had worked. Filling both is what makes one Reply
/// box mean the same thing on every backend.
///
/// Safe for Claude because `response` still supersedes the map there, and the
/// map holds the same sentence either way. Only called with a single
/// question: [`question_actions`] refuses a request carrying more.
fn free_text(questions: &[oximux_agents::thread::AskQuestion], text: &str) -> QuestionAnswers {
    let mut answers =
        QuestionAnswers { response: Some(text.to_string()), ..QuestionAnswers::default() };
    for q in questions {
        answers.by_question.insert(
            q.id.clone(),
            QuestionAnswer { selected: Vec::new(), custom: Some(text.to_string()) },
        );
    }
    answers
}

#[cfg(test)]
#[path = "inline_answer_tests.rs"]
mod tests;
