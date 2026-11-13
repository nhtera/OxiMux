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
use oximux_agents::row_actions::{InlineAnswer, RowActions, question_actions, row_actions};
use oximux_agents::session_registry::{PendingPermissionView, SessionHandle};
use oximux_agents::thread::{PermissionDecision, QuestionAnswers};

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
    /// A target with no live session offers nothing to answer: a history row
    /// or a cockpit agent has no registry handle, so there is no proposal to
    /// approve and no connection to send a prompt to. Its Dismiss still comes
    /// from the kind, so the row is not left mute.
    pub fn row_actions(&self, target: &str, attention: Option<&Attention>) -> RowActions {
        let kind = attention.map(|a| a.kind);
        match self.session_of(target) {
            Some(handle) => row_actions(
                kind,
                &handle.outstanding_permissions(),
                handle.pending_question().as_ref(),
            ),
            None => row_actions(kind, &[], None),
        }
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
    /// Which of the two is decided **here, now**, not by what the row was
    /// drawn with — a question answered in the chat between the paint and the
    /// click would otherwise have this text sent as a prompt, which the agent
    /// would read while still waiting on the question.
    ///
    /// Sending also records the look (B15): a session you have just answered
    /// can never sit in "unread", and the person has plainly seen it.
    pub fn reply_from_row(&self, target: &str, text: &str) -> anyhow::Result<bool> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(false);
        }
        let handle = self.live_session(target)?;
        // One read of the question, used for the decision AND the answer: two
        // reads would let a withdrawn question be answered by its successor's
        // id. `None` means nothing is being asked, so the text is a prompt.
        let sent = match handle.pending_question() {
            Some(question) => {
                // Re-decided by the same table the row was drawn with. A
                // question that has appeared since the paint is answered as a
                // question rather than talked over; one that cannot be
                // answered in a single line (options to pick, a secret) is
                // not answerable from here at all.
                let InlineAnswer::Question { request_id } = question_actions(&question).answer
                else {
                    anyhow::bail!(
                        "{target} is waiting on a choice, not on a sentence; \
                         open the agent to answer it"
                    );
                };
                let answers = QuestionAnswers {
                    response: Some(text.to_string()),
                    ..QuestionAnswers::default()
                };
                handle.answer_question(&request_id, &question.questions, &answers)?
            }
            // A proposal still pending is not in the way: a person who types
            // instead of approving is steering, and the registry's own send
            // gate decides whether that lands now or is queued.
            None => {
                handle.send_prompt(text, &[])?;
                true
            }
        };
        if sent {
            self.mark_viewed(target);
        }
        Ok(sent)
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

#[cfg(test)]
#[path = "inline_answer_tests.rs"]
mod tests;
