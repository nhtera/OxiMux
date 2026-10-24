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
use oximux_agents::thread::{ChatImage, PermissionDecision, PermissionKind, ThreadEvent, ToolCallStatus, Transport};
use uuid::Uuid;

use super::AgentChatView;

/// Why a plan approval was refused in a turn child events started.
const PLAN_REFUSED: &str = "Leaving plan mode needs you: this turn was started by child events, not by you.";

/// What the transcript says when held child events go out with a message.
const HELD_EVENTS_NOTE: &str = "Child events that arrived after the Stop went with this message.";

/// What the transcript says when a Stop takes back a line the user steered.
const STEER_TAKEN_BACK: &str = "The Stop took back a message the agent had not started on; it is queued again below.";

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
        // The process these were tracked against is gone with the binding.
        gate.take_unstarted();
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
        let mut gate = self.send_gate.lock().unwrap();
        // A steered line not yet started may still run as a turn of its own:
        // until it starts, the turn is not over.
        let awaiting_steer = gate.awaiting_steer();
        gate.set_holds(Holds {
            turn_active: self.thread.turn_active || awaiting_steer,
            interrupted: self.interrupted,
            disconnected: self.disconnected || self.unbound || self.auth.is_some() || self.connection.is_none(),
            retry_armed: self.retry.is_armed(),
            prompt_pending: self.thread.pending_permission().is_some() || self.thread.pending_question().is_some(),
            turn_started: self.turn_has_output(),
            // Child events are steered only into an agent that reports what it
            // started on: without that, a Stop could not take them back.
            steer_supported: self.reports_receipts()
                && self.connection.as_ref().is_some_and(|c| c.capabilities().supports_steer),
        });
    }

    /// Whether the agent has said anything since the latest prompt.
    fn turn_has_output(&self) -> bool {
        self.thread
            .entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e, ThreadEntry::User { .. }))
            .any(|e| matches!(e, ThreadEntry::Assistant(_) | ThreadEntry::ToolCall(_)))
    }

    /// Whether this agent reports, per line, that it started on it (Claude).
    fn reports_receipts(&self) -> bool {
        self.backend.transport == Transport::StreamJson
    }

    /// Whether a steered line has not started yet (see `sync_gate_holds`).
    pub(super) fn awaiting_steer(&self) -> bool {
        self.send_gate.lock().unwrap().awaiting_steer()
    }

    /// What the view checks on each event before folding it: the spawn
    /// posture, receipts for its own lines, and a plan approval asked for in a
    /// turn child events started (returned, to refuse once it is folded).
    pub(super) fn before_apply(&mut self, event: &ThreadEvent) -> Option<String> {
        self.guard_spawn_posture(event);
        self.note_receipt(event);
        match event {
            ThreadEvent::PermissionRequested { request_id, kind: PermissionKind::Plan, .. }
                if self.send_gate.lock().unwrap().turn_started_by_events() =>
            {
                Some(request_id.clone())
            }
            _ => None,
        }
    }

    /// A turn child events started may not raise the session's permission
    /// mode: leaving plan mode is refused there and left to the user. Other
    /// prompts in such a turn go to the user as usual.
    pub(super) fn after_apply(&mut self, refuse_plan: Option<String>) {
        let Some(request_id) = refuse_plan else { return };
        let Some((tool_id, _)) = self.thread.pending_permission().map(|(t, r)| (t.to_string(), r.request_id.clone()))
            .filter(|(_, r)| *r == request_id)
        else {
            return;
        };
        if let Some(conn) = &self.connection {
            let _ = conn.resolve_permission(&request_id, PermissionDecision::Deny { message: PLAN_REFUSED.into() });
        }
        self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
        self.thread.apply(&ThreadEvent::Notice(PLAN_REFUSED.into()));
    }

    /// Receipts for this chat's lines. A steered line the agent starts after
    /// the turn it was meant for has ended runs as a turn of its own: the chat
    /// shows that turn as running (a Stop can end it). For an agent with no
    /// receipts, its first frame after a line stands in for acceptance and the
    /// turn's end for delivery, so the host can settle what it sent.
    fn note_receipt(&mut self, event: &ThreadEvent) {
        let mut gate = self.send_gate.lock().unwrap();
        let mut stand_in = None;
        match event {
            ThreadEvent::UserMessageAccepted { uuid } => {
                if let Some(person) = gate.note_accepted(uuid)
                    && !self.thread.turn_active
                {
                    gate.begin_turn(if person { SendSource::User } else { SendSource::ParentEvent });
                    self.thread.turn_active = true;
                }
            }
            ThreadEvent::UserMessagesDelivered { uuids } => uuids.iter().for_each(|u| {
                gate.note_accepted(u);
            }),
            _ if self.reports_receipts() => {}
            ThreadEvent::TurnEnded { .. } => {
                stand_in = gate.unreported_delivered().map(|u| ThreadEvent::UserMessagesDelivered { uuids: vec![u.to_string()] });
            }
            _ => stand_in = gate.unreported_accepted().map(|u| ThreadEvent::UserMessageAccepted { uuid: u.to_string() }),
        }
        drop(gate);
        if let (Some(receipt), Some(binding)) = (stand_in, &self.remote) {
            binding.ingest(receipt);
        }
    }

    /// The Stop button (and a Stop from a phone or the CLI): take back the
    /// lines written into this turn that the agent has not started on, then
    /// interrupt, so none of them starts a turn after the Stop. Off the UI
    /// thread (each reply is awaited briefly). Child events taken back return
    /// to the host, held until the user's next message; a line the user
    /// steered goes back into the composer. With nothing to take back this is
    /// the plain interrupt.
    pub(super) fn interrupt_turn(&mut self, cx: &mut Context<Self>) {
        // Held until the user's next message, across any respawn in between.
        self.send_gate.lock().unwrap().stop_by_user();
        let Some(conn) = self.connection.clone() else { return };
        let unstarted = self.send_gate.lock().unwrap().take_unstarted();
        if unstarted.is_empty() {
            let _ = conn.cancel();
            return;
        }
        let ids: Vec<String> = unstarted.iter().map(|(u, _)| u.to_string()).collect();
        let stopping = cx.background_spawn(async move { conn.stop_turn(&ids) });
        cx.spawn(async move |this, cx| {
            let taken = match stopping.await {
                Ok(taken) => taken,
                Err(err) => {
                    tracing::warn!(%err, "stopping the turn failed");
                    return;
                }
            };
            let _ = this.update(cx, |view, cx| {
                let mut typed = Vec::new();
                let gate = view.send_gate.lock().unwrap();
                for (uuid, text) in unstarted.into_iter().filter(|(u, _)| taken.contains(&u.to_string())) {
                    match text {
                        Some(text) => typed.push(text),
                        None => gate.report_sent(uuid, false),
                    }
                }
                drop(gate);
                if !typed.is_empty() {
                    view.thread.apply(&ThreadEvent::Notice(STEER_TAKEN_BACK.into()));
                    view.composer.update(cx, |c, cx| c.seed_queued(typed, cx));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Send the next item the gate lets go, if any. One per turn: the send
    /// starts a turn, which closes the gate until it ends.
    pub(super) fn flush_send_gate(&mut self, cx: &mut Context<Self>) {
        if self.send_gate.lock().unwrap().take_stop_request() {
            self.stop_turn(cx);
            return;
        }
        if self.send_gate.lock().unwrap().ready() == Ready::Steer {
            self.steer_from_gate(cx);
            return;
        }
        let Some(item) = self.send_gate.lock().unwrap().take_next_if_open() else { return };
        if item.text.trim().is_empty() && item.images.is_empty() {
            // Nothing to send; dropping it keeps it from holding up the rest.
            self.sync_gate_holds();
            return;
        }
        if item.source.is_remote() {
            // Nobody is at this screen for a turn a phone, another agent or
            // child events started (their text may be a child's output).
            self.screen_control.begin_remote_turn();
        }
        // Only a prompt typed here names the tab: a spawned child keeps its
        // `↳` label, and a chat a phone started keeps the name it was given.
        let title_generated = std::mem::replace(&mut self.title_generated, true);
        self.pending_send_uuid = Some(item.uuid);
        let kept = item.clone();
        self.send_text(item.text, item.images, cx);
        // A send that stopped before writing (nothing to send to right now)
        // puts the item back first in line, so a phone's prompt is never lost
        // after its sender was told it was taken; the turn it would have
        // started is undone.
        if self.pending_send_uuid.take().is_some() {
            self.title_generated = title_generated;
            self.screen_control.end_remote_turn();
            self.send_gate.lock().unwrap().requeue_front(kept);
            self.sync_gate_holds();
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
            gate.note_steered(item.uuid, None);
            drop(gate);
            // Shown where the agent takes it in, like a steer typed here.
            self.thread.push_user_message(&item.text);
            self.follow_bottom();
            cx.notify();
        }
    }

    /// Hand a message to the turn that is already streaming (a queued chip's
    /// Send now on an agent that takes mid-turn lines). The agent picks it up
    /// at the next tool boundary and changes course.
    ///
    /// Deliberately not routed through [`Self::send_text`]. That path is the
    /// *start* of a turn: it binds an unbound draft, respawns an interrupted
    /// child, generates the tab title and takes the pre-turn checkpoint. None of
    /// that applies to a message going into a turn that is already running — and
    /// the checkpoint especially must not: it anchors rewind to the repo as it
    /// stood *before* a turn, and taking one now would capture a tree the running
    /// turn's own tools are halfway through editing.
    pub(super) fn steer_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() || !self.thread.turn_active {
            return;
        }
        let Some(conn) = self.connection.clone() else { return };
        // Tracked when the agent reports it, so a Stop can take it back (and
        // return it to the composer) instead of letting it run afterwards.
        let uuid = Uuid::new_v4();
        let sent = if self.reports_receipts() { conn.steer_tagged(&text, &uuid.to_string()) } else { conn.steer(&text) };
        // The bubble goes in at the point the user sent it, which is also where
        // the agent will act on it — the turn's remaining output lands after.
        match sent {
            Ok(()) => {
                if self.reports_receipts() {
                    self.send_gate.lock().unwrap().note_steered(uuid, Some(text.clone()));
                }
                self.thread.push_user_message(&text);
                self.note_chat_prompt_sent();
            }
            Err(e) => self.thread.last_error = Some(format!("Steer failed: {e}")),
        }
        self.follow_bottom();
        self.sync_composer(cx);
        cx.notify();
    }

    /// Write a prompt to the agent, under the id it will report it by. Child
    /// events the gate held after a Stop go first, in the same line, under
    /// their batch's id (so their receipts settle them).
    pub(super) fn write_prompt(&mut self, text: &str, images: &[ChatImage]) -> anyhow::Result<()> {
        // Only after a Stop: events that merely waited for a busy turn go on
        // their own when it ends. The host merges events into one batch, so
        // there is normally one; any other waits for the next turn.
        let held = {
            let mut gate = self.send_gate.lock().unwrap();
            if gate.stopped_by_user() {
                let mut held = gate.take_held_events().into_iter();
                let first = held.next();
                for rest in held.rev() {
                    gate.requeue_front(rest);
                }
                first
            } else {
                None
            }
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
        let mut gate = self.send_gate.lock().unwrap();
        gate.report_sent(uuid, sent.is_ok());
        if sent.is_ok() && !self.reports_receipts() {
            gate.note_unreported(uuid);
        }
        drop(gate);
        sent
    }
}

#[cfg(test)]
#[path = "send_gate_link_tests.rs"]
mod tests;
