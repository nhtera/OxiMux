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

use gpui::{AppContext as _, Context};
use oximux_agents::send_gate::{Holds, Ready, SendGate, SendSource};
use oximux_agents::thread::ThreadEntry;
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

    /// The chat let go of its session: child events still waiting here go
    /// back to the host (which hands them out again, under a new id, when the
    /// chat binds again), so a batch can never be written twice.
    pub(super) fn release_gate_events(&self) {
        let mut gate = self.send_gate.lock().unwrap();
        gate.clear_waker();
        let events: Vec<u64> = gate.queued().filter(|q| q.source == SendSource::ParentEvent).map(|q| q.id).collect();
        for id in events {
            gate.remove(id);
        }
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
            // The agent has answered something since the prompt.
            turn_started: !matches!(self.thread.entries.last(), Some(ThreadEntry::User { .. }) | None),
            steer_supported: self.connection.as_ref().is_some_and(|c| c.capabilities().supports_steer),
        });
    }

    /// The agent started on (or finished) lines this chat wrote: a Stop no
    /// longer needs to take them back.
    pub(super) fn note_receipt(&self, event: &ThreadEvent) {
        let mut gate = self.send_gate.lock().unwrap();
        match event {
            ThreadEvent::UserMessageAccepted { uuid } => gate.note_accepted(uuid),
            ThreadEvent::UserMessagesDelivered { uuids } => uuids.iter().for_each(|u| gate.note_accepted(u)),
            _ => {}
        }
    }

    /// The Stop button: take back the child events written into this turn that
    /// the agent has not started on, then interrupt, so none of them starts a
    /// turn after the Stop. Off the UI thread (each reply is awaited briefly);
    /// what was taken back returns to the host, held until the user's next
    /// message. With nothing to take back this is the plain interrupt.
    pub(super) fn interrupt_turn(&mut self, cx: &mut Context<Self>) {
        // Held until the user's next message, across any respawn in between.
        self.send_gate.lock().unwrap().stop_by_user();
        let Some(conn) = self.connection.clone() else { return };
        let unstarted: Vec<String> =
            self.send_gate.lock().unwrap().take_unstarted().iter().map(Uuid::to_string).collect();
        if unstarted.is_empty() {
            let _ = conn.cancel();
            return;
        }
        let gate = self.send_gate.clone();
        cx.background_spawn(async move {
            match conn.stop_turn(&unstarted) {
                Ok(taken) => {
                    let gate = gate.lock().unwrap();
                    for uuid in taken.iter().filter_map(|u| Uuid::parse_str(u).ok()) {
                        gate.report_sent(uuid, false);
                    }
                }
                Err(err) => tracing::warn!(%err, "stopping the turn failed"),
            }
        })
        .detach();
    }

    /// Send the next item the gate lets go, if any. One per turn: the send
    /// starts a turn, which closes the gate until it ends.
    pub(super) fn flush_send_gate(&mut self, cx: &mut Context<Self>) {
        if self.send_gate.lock().unwrap().ready() == Ready::Steer {
            self.steer_from_gate(cx);
            return;
        }
        let Some(item) = self.send_gate.lock().unwrap().take_next_if_open() else { return };
        if item.source.is_remote() {
            // Nobody is at this screen for a turn a phone, another agent or
            // child events started (their text may be a child's output).
            self.screen_control.begin_remote_turn();
        }
        // Only a prompt typed here names the tab: a spawned child keeps its
        // `↳` label, and a chat a phone started keeps the name it was given.
        self.title_generated = true;
        self.pending_send_uuid = Some(item.uuid);
        let kept = item.clone();
        self.send_text(item.text, item.images, cx);
        // A send that stopped before writing (nothing to send to right now)
        // puts the item back first in line, so a phone's prompt is never lost
        // after its sender was told it was taken.
        if self.pending_send_uuid.take().is_some() {
            self.send_gate.lock().unwrap().requeue_front(kept);
        }
    }

    /// Write the gate's steerable item (child events) into the running turn.
    /// The agent folds it in at its next tool boundary, or runs it next.
    fn steer_from_gate(&mut self, cx: &mut Context<Self>) {
        let (Some(conn), Some(item)) = (self.connection.clone(), self.send_gate.lock().unwrap().take_steerable())
        else {
            return;
        };
        // Child output goes into this turn: treat it as one nobody watches.
        self.screen_control.begin_remote_turn();
        let sent = conn.steer_tagged(&item.text, &item.uuid.to_string());
        let mut gate = self.send_gate.lock().unwrap();
        gate.report_sent(item.uuid, sent.is_ok());
        if sent.is_ok() {
            gate.note_steered(item.uuid);
            drop(gate);
            // Shown where the agent takes it in, like a steer typed here.
            self.thread.push_user_message(&item.text);
            self.follow_bottom();
            cx.notify();
        }
    }

    /// Write a prompt to the agent, under the id it will report it by. Child
    /// events the gate held after a Stop go first, in the same line, under
    /// their batch's id (so their receipts settle them).
    pub(super) fn write_prompt(&mut self, text: &str, images: &[ChatImage]) -> anyhow::Result<()> {
        // Only after a Stop: events that merely waited for a busy turn go on
        // their own when it ends.
        let held = {
            let mut gate = self.send_gate.lock().unwrap();
            if gate.stopped_by_user() { gate.take_held_events().into_iter().next() } else { None }
        };
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
