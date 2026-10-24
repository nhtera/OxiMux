//! The chat's side of its send gate: sends that did not start in this
//! composer (a phone's prompt, another agent's `send_agent`, a batch of child
//! events) queue in the gate, and this view writes them when the chat may take
//! them.
//!
//! The gate cannot send by itself: the send is [`AgentChatView::send_text`]
//! (the bubble, the checkpoint, the remote tee, the guards). So the view
//! mirrors what holds it into the gate after every change
//! ([`AgentChatView::sync_gate_holds`]), the gate's waker nudges the view's
//! relay task, and the view takes one item and sends it the ordinary way.
//!
//! Messages typed here still queue in the composer, which keeps their chips
//! and history; the two queues never hold the same item.

use std::sync::{Arc, Mutex};

use gpui::Context;
use oximux_agents::send_gate::{Holds, SendGate, SendSource};
use oximux_agents::thread::{ChatImage, ThreadEvent};
use uuid::Uuid;

use super::AgentChatView;

/// What the transcript says when held child events go out with a message.
const HELD_EVENTS_NOTE: &str = "Child events that arrived after the Stop went with this message.";

impl AgentChatView {
    /// A chat's gate; its waker is armed each time the chat binds.
    pub(super) fn new_send_gate() -> Arc<Mutex<SendGate>> {
        Arc::new(Mutex::new(SendGate::new()))
    }

    /// Point the gate's waker at a fresh channel for the relay task to read.
    pub(super) fn arm_send_gate(gate: &Arc<Mutex<SendGate>>) -> futures::channel::mpsc::UnboundedReceiver<()> {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        gate.lock().unwrap().set_waker(move || {
            let _ = tx.unbounded_send(());
        });
        rx
    }

    /// Mirror what holds this chat's sends into its gate.
    pub(super) fn sync_gate_holds(&self) {
        self.send_gate.lock().unwrap().set_holds(Holds {
            turn_active: self.thread.turn_active,
            interrupted: self.interrupted,
            disconnected: self.disconnected || self.unbound || self.auth.is_some() || self.connection.is_none(),
            retry_armed: self.retry.is_armed(),
            prompt_pending: self.thread.pending_permission().is_some() || self.thread.pending_question().is_some(),
        });
    }

    /// Send the next item the gate lets go, if any. One per turn: the send
    /// starts a turn, which closes the gate until it ends.
    pub(super) fn flush_send_gate(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.send_gate.lock().unwrap().take_next_if_open() else { return };
        if item.source == SendSource::Remote {
            // Nobody is at this screen for a turn a phone started.
            self.screen_control.begin_remote_turn();
        }
        // Only a prompt typed here names the tab: a spawned child keeps its
        // `↳` label, and a chat a phone started keeps the name it was given.
        self.title_generated = true;
        self.pending_send_uuid = Some(item.uuid);
        self.send_text(item.text, item.images, cx);
        // A send that stopped before writing (nothing to send to) gives the
        // item back to whoever queued it.
        if let Some(uuid) = self.pending_send_uuid.take() {
            self.send_gate.lock().unwrap().report_sent(uuid, false);
        }
    }

    /// Write a prompt to the agent, under the id it will report it by. Child
    /// events the gate held after a Stop go first, in the same line, under
    /// their batch's id (so their receipts settle them).
    pub(super) fn write_prompt(&mut self, text: &str, images: &[ChatImage]) -> anyhow::Result<()> {
        let held = self.send_gate.lock().unwrap().take_held_events().into_iter().next();
        let (line, uuid) = match held {
            Some(events) => {
                if let Some(own) = self.pending_send_uuid.take() {
                    self.send_gate.lock().unwrap().report_sent(own, false);
                }
                self.thread.apply(&ThreadEvent::Notice(HELD_EVENTS_NOTE.into()));
                (format!("{}\n\n{text}", events.text), events.uuid)
            }
            None => (text.to_string(), self.pending_send_uuid.take().unwrap_or_else(Uuid::new_v4)),
        };
        let conn = self.connection.as_ref().ok_or_else(|| anyhow::anyhow!("no agent to send to"))?;
        let sent = conn.send_user_message_tagged(&line, images, &uuid.to_string());
        self.send_gate.lock().unwrap().report_sent(uuid, sent.is_ok());
        sent
    }
}

#[cfg(test)]
#[path = "send_gate_link_tests.rs"]
mod tests;
