//! Acting on a row's inline Allow / Reject / Reply / Dismiss.
//!
//! Each of the four goes through the host, which owns both the decision path
//! and the record of what a person has seen. Nothing is decided here: the
//! action and the answer were settled by the one table when the row was
//! painted, and the host re-validates them at the dispatch.
//!
//! **A refusal is shown, not swallowed.** The host refuses an approval whose
//! arguments changed, one whose process has been replaced, and a reply while
//! a choice is waiting — all of which are things the person needs to be told,
//! because each means "the thing you were looking at is not there any more"
//! and the next step is to open the agent. A silently dropped click would
//! read as a broken button.

use gpui::Context;
use oximux_agents::row_actions::{InlineAnswer, RowAction};

use crate::shell::agents_dashboard::row_attention::attention_target;
use crate::shell::left_rail::rail_agent_row::RailAgentTarget;
use crate::shell::toast::ToastKind;
use crate::workspace_root::WorkspaceRoot;

impl WorkspaceRoot {
    /// Do what a row's inline control asked.
    ///
    /// `answer` is the one the row was painted with — see the comment at its
    /// call site: the digest inside it is older than this click, which is the
    /// entire guarantee.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn act_on_row_action(
        &mut self,
        action: RowAction,
        answer: &InlineAnswer,
        target: &str,
        row_target: &RailAgentTarget,
        db_id: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            // Reply does not answer anything by itself: the first click opens
            // the box, and Enter in it sends. A pill that sent on click would
            // have nothing to send.
            RowAction::Reply => {
                self.toggle_row_reply(target, window, cx);
                return;
            }
            // A dismissal is a look, which is why it goes through the same
            // call as opening the row: the person has seen this and dealt
            // with it. The evaluator decides what that clears.
            RowAction::Dismiss => {
                if let Some(host) = self.host(cx) {
                    host.mark_viewed(&attention_target(db_id, row_target));
                }
                cx.notify();
                return;
            }
            RowAction::Allow | RowAction::Reject => {}
        }
        let InlineAnswer::Permission { request_id, args_digest } = answer else {
            // A verdict on a row that had nothing to answer. Not reachable
            // through the table — Allow is only ever offered beside a
            // proposal — so there is nothing useful to say to the person.
            return;
        };
        let Some(host) = self.host(cx) else { return };
        let decided = match action {
            RowAction::Allow => host.allow_from_row(target, request_id, args_digest),
            _ => host.reject_from_row(target, request_id),
        };
        match decided {
            // `false` is "already answered": a second click, or the chat got
            // there first. Not a failure, and nothing to say about it.
            Ok(_) => {}
            Err(err) => self.push_toast(ToastKind::Error, err.to_string(), cx),
        }
        cx.notify();
    }

    /// Open this row's Reply box, or close it if it is already the open one.
    ///
    /// Focus moves into the box on open, because the person clicked Reply and
    /// means to type — and *only* then, so a row painting itself never takes
    /// focus from a composer someone is writing in.
    pub(crate) fn toggle_row_reply(
        &mut self,
        target: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let rail = self.left_rail.clone();
        rail.update(cx, |rail, cx| rail.toggle_row_reply(target, window, cx));
        cx.notify();
    }

    /// Send what is in the open Reply box.
    ///
    /// The host decides whether that text answers a question or starts a
    /// turn, re-read at this moment rather than at the paint. On success the
    /// box closes; on a refusal it stays open with the text still in it, so
    /// the person can read the reason and copy what they wrote.
    pub(crate) fn send_row_reply(
        &mut self,
        target: &str,
        text: &str,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host(cx) else { return };
        match host.reply_from_row(target, text) {
            Ok(true) => {
                let rail = self.left_rail.clone();
                rail.update(cx, |rail, cx| rail.close_row_reply(window, cx));
            }
            // Nothing to send: an empty box. Left open, since closing it
            // would look like the reply went somewhere.
            Ok(false) => {}
            Err(err) => self.push_toast(ToastKind::Error, err.to_string(), cx),
        }
        cx.notify();
    }

    /// Collapse the open Reply box without sending it.
    pub(crate) fn close_row_reply(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let rail = self.left_rail.clone();
        rail.update(cx, |rail, cx| rail.close_row_reply(window, cx));
        cx.notify();
    }

    /// The process-wide host, if it is running.
    fn host(&self, cx: &gpui::App) -> Option<std::sync::Arc<oximux_remote_host::HostCore>> {
        cx.try_global::<crate::remote_control::RemoteControl>().and_then(|rc| rc.host_core())
    }
}
