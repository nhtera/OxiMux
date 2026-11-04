//! The chat's health facts, published to its session handle.
//!
//! The view owns the fold and the connection, so it is the one that knows
//! whether a turn runs, a prompt waits, a retry is armed or the process died.
//! It gathers those facts into [`HealthInputs`] after every batch and every
//! state change and publishes them; the handle and the host derive the health
//! (`oximux_agents::health`). What the fold does not keep is tracked here:
//! when the agent last showed a sign of life, an unanswered send, how the
//! process ended, and what a restart left behind.

use gpui::Context;
use oximux_agents::health::{
    self, ExitProof, HealthInputs, PromptKind, Process, Restored,
};
use oximux_agents::session_health::now_ms;
use oximux_agents::thread::{TaskStatus, ThreadEntry, ThreadEvent};

use super::AgentChatView;

/// What the view tracks for its health beyond the fold.
#[derive(Debug, Default)]
pub(super) struct HealthTrack {
    /// The last sign of life (any provider frame, or a send).
    last_activity_at: i64,
    /// A send went out and no frame has answered it yet.
    sent_unanswered: bool,
    /// The id that send went out under.
    sent_id: Option<String>,
    /// Restored sends nobody answered, still held and marked: the blob keeps
    /// each (with its own text) until the user sends or drops its card.
    pub(super) unconfirmed_held: Vec<crate::persisted_chat::UnconfirmedSend>,
    /// The agent took the last send (receipt) and has produced nothing since.
    accepted_at: Option<i64>,
    /// How the process ended, once it has.
    exit: Option<ExitProof>,
    /// Sends and process exits so far, for the edge ids.
    turns: u32,
    exits: u32,
    /// Set on restore until the first frame on the new connection.
    restored: Option<Restored>,
    /// The last turn ended in error (its message); cleared by the next send
    /// or a turn that ends well. Only a turn's own result, not a failed fork
    /// or rewind, makes a session read failed.
    failed_turn: Option<String>,
    /// The composer's queue: restored messages held, and messages waiting to drain.
    pub(super) held: u32,
    pub(super) queued: u32,
}

impl HealthTrack {
    /// A restored chat: unconfirmed until its first frame. `was_mid_turn`:
    /// its saved state had a turn or background work running.
    pub(super) fn restored(was_mid_turn: bool, prior_host_gone: bool) -> Self {
        Self { restored: Some(Restored { was_mid_turn, prior_host_gone }), ..Self::default() }
    }

    /// A frame from the agent. Receipts are bookkeeping, not a sign of life,
    /// except that acceptance starts the "accepted, not started" clock.
    pub(super) fn note_event(&mut self, ev: &ThreadEvent, now: i64) {
        if ev.is_receipt() {
            if matches!(ev, ThreadEvent::UserMessageAccepted { .. }) && self.sent_unanswered {
                self.accepted_at.get_or_insert(now);
            }
            return;
        }
        self.last_activity_at = now;
        self.sent_unanswered = false;
        self.accepted_at = None;
        self.restored = None;
        if let ThreadEvent::TurnEnded { is_error, result, .. } = ev {
            self.failed_turn = is_error.then(|| result.clone().unwrap_or_else(|| "Turn failed".into()));
        }
    }

    /// The last turn failed and nothing has been sent since: the queue waits
    /// for the user rather than draining into a failing session.
    pub(super) fn holds_queue(&self) -> bool {
        self.failed_turn.is_some()
    }

    /// The conversation was cleared: its failed turn no longer holds anything.
    pub(super) fn forget_failed_turn(&mut self) {
        self.failed_turn = None;
    }

    /// A send was written to the agent, under `id` when it was tagged.
    pub(super) fn note_sent(&mut self, now: i64, id: Option<String>) {
        self.sent_id = id;
        self.turns += 1;
        self.sent_unanswered = true;
        self.accepted_at = None;
        self.last_activity_at = now;
        self.exit = None;
        self.failed_turn = None;
    }

    /// The process ended. Returns the proof recorded.
    pub(super) fn note_exit(&mut self, stopped: bool, code: Option<i32>) -> ExitProof {
        let proof = match (stopped, code) {
            (true, _) => ExitProof::UserStop,
            (false, Some(code)) => ExitProof::ExitCode(code),
            (false, None) => ExitProof::ConnectionClosed,
        };
        self.exits += 1;
        self.exit = Some(proof);
        self.sent_unanswered = false;
        self.accepted_at = None;
        proof
    }

    /// A new process took over (a respawn): its exit is not the old one's.
    pub(super) fn note_connected(&mut self) {
        self.exit = None;
    }
}

/// The error a dead process leaves on the chat.
pub(super) fn exit_message(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("Agent process exited (code {code})."),
        None => "Agent process exited.".into(),
    }
}

impl AgentChatView {
    /// The facts as they stand.
    pub(super) fn health_inputs(&self) -> HealthInputs {
        let t = &self.health;
        let incarnation = self.remote.as_ref().map(|b| b.incarnation()).unwrap_or_default();
        let process = match (t.exit, &self.connection) {
            (Some(proof), _) => Process::Exited(proof),
            (None, Some(_)) => Process::Live,
            (None, None) => Process::Dormant,
        };
        HealthInputs {
            turn_active: self.thread.turn_active,
            continuations_open: self.thread.continuations_open,
            pending_unanswered_send: t.sent_unanswered,
            accepted_unstarted_since: t.accepted_at,
            background_active: self
                .thread
                .background_tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Running)
                .count() as u32,
            children_active: 0,
            prompt: if self.thread.pending_permission().is_some() {
                Some(PromptKind::Permission)
            } else if self.thread.pending_question().is_some() {
                Some(PromptKind::Question)
            } else {
                None
            },
            prompt_request: self
                .thread
                .pending_permission()
                .map(|(_, req)| req.request_id.clone())
                .or_else(|| self.thread.pending_question().map(|(_, req)| req.request_id.clone())),
            limited_until: self.retry.pending.as_ref().map(|p| p.wake_at_ms),
            limit_reason: self.retry.pending.as_ref().map(|p| p.reason.clone()),
            // The fold's message when it has one: it folds in the stderr
            // diagnostic the raw result lacks.
            last_turn_failure: t
                .failed_turn
                .as_ref()
                .filter(|_| !self.thread.turn_active)
                .map(|raw| self.thread.last_error.clone().unwrap_or_else(|| raw.clone())),
            interrupted: self.interrupted,
            process,
            restored: t.restored,
            held_restored: t.held,
            queued: t.queued,
            last_activity_at: t.last_activity_at,
            turn_id: (t.turns > 0).then(|| format!("{incarnation}:{}", t.turns)),
            exit_seq: (t.exits > 0).then(|| format!("{incarnation}:{}", t.exits)),
        }
    }

    /// Publish the facts to the session handle (when bound). Silence never
    /// closes a continuation: only a result, a Stop or exit, or a respawn
    /// does; a quiet one reads stalled.
    pub(super) fn publish_health(&mut self) {
        if let Some(binding) = &self.remote {
            binding.publish_health(self.health_inputs());
        }
    }

    /// The process is gone: record how, before the binding goes. The code is
    /// read at once (the output closes as the process exits); when it is not
    /// there yet, a short poll fills it into the error afterwards.
    pub(super) fn record_exit(&mut self, cx: &mut Context<Self>) {
        let code = self.connection.as_ref().and_then(|c| c.exit_code());
        self.thread.continuations_open = 0;
        self.health.note_exit(self.interrupted, code);
        self.publish_health();
        if code.is_some() || self.interrupted {
            return;
        }
        let Some(conn) = self.connection.clone() else { return };
        let generation = self.drain_gen;
        cx.spawn(async move |this, cx| {
            for _ in 0..40 {
                cx.background_executor().timer(std::time::Duration::from_millis(50)).await;
                if let Some(code) = conn.exit_code() {
                    let _ = this.update(cx, |view, cx| {
                        // Only while this process is still the chat's last one.
                        if view.drain_gen == generation
                            && view.disconnected
                            && view.health.exit == Some(ExitProof::ConnectionClosed)
                        {
                            view.health.exit = Some(ExitProof::ExitCode(code));
                            if view.thread.last_error.as_deref() == Some(exit_message(None).as_str()) {
                                view.thread.last_error = Some(exit_message(Some(code)));
                            }
                            cx.notify();
                        }
                    });
                    return;
                }
            }
        })
        .detach();
    }

    /// A restored chat: unconfirmed until its first frame, or interrupted when
    /// it was mid-turn and this host provably replaced the one that ran it.
    /// A restart mid-turn also owes the agent a recovery note on the next
    /// message (kept if an earlier restore's note is still unsent).
    pub fn restore_health(&mut self, turn: super::RestoredTurn, cx: &mut Context<Self>) {
        let mid_turn = turn.mid_turn;
        // The send nothing answered before the quit may or may not have reached
        // the agent: offer it back, held and marked, never resend it unasked.
        let last_user = self.thread.entries.iter().rev().find_map(|e| match e {
            ThreadEntry::User { text, .. } => Some(text.clone()),
            _ => None,
        });
        // Cards held from an earlier restore keep their own text; only the
        // live send's text is the latest user entry.
        let live = turn
            .unanswered_send
            .zip(last_user)
            .map(|(id, text)| crate::persisted_chat::UnconfirmedSend { id, text });
        let unconfirmed: Vec<_> =
            turn.unconfirmed_held.into_iter().chain(live).filter(|u| !u.text.trim().is_empty()).collect();
        self.composer.update(cx, |c, cx| {
            // Each goes in first, so the oldest ends up on top.
            for u in unconfirmed.iter().rev() {
                c.hold_unconfirmed(u.text.clone(), cx);
            }
        });
        self.recovery_note = turn.recovery_note.or_else(|| {
            let at = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
            mid_turn.then(|| oximux_agents::recovery_note::build(&at, &turn.stopped_tasks))
        });
        self.sync_composer(cx);
        let prior_host_gone = cx
            .try_global::<crate::remote_control::RemoteControl>()
            .and_then(|rc| rc.host_core())
            .and_then(|core| core.prior_owner())
            .is_some_and(|prior| !matches!(prior, oximux_remote_host::PriorOwner::None));
        // The composer's counts are its own, reported by `QueueChanged`
        // whenever that lands: keep them across the reset.
        let (held, queued) = (self.health.held, self.health.queued);
        self.health = HealthTrack::restored(mid_turn, prior_host_gone);
        self.health.unconfirmed_held = unconfirmed;
        (self.health.held, self.health.queued) = (held, queued);
        // A dormant chat is not bound to the host yet, so this is where its
        // restored state can be seen.
        let h = health::compute(&self.health_inputs(), now_ms(), &health::HealthConfig::default());
        tracing::info!(
            session = %self.remote_session_id,
            status = ?h.status,
            liveness = h.liveness.wire_name(),
            reason = h.reason.as_deref().unwrap_or(""),
            "health (restored)"
        );
    }

    /// Blank the transcript for a fresh conversation (`/clear`). The old one's
    /// debts end with it: a fresh session never ran the turn a restart cut
    /// off, and its first message is not held behind the old one's failure.
    pub(super) fn clear_conversation(&mut self) {
        self.thread.clear();
        self.recovery_note = None;
        self.health.forget_failed_turn();
        self.meta_dirty.set(true);
    }

    /// The user removed the recovery note (the strip's [Remove]).
    pub(super) fn remove_recovery_note(&mut self, cx: &mut Context<Self>) {
        self.recovery_note = None;
        self.meta_dirty.set(true);
        self.sync_composer(cx);
    }

    /// The send written but not answered yet, as the blob keeps it: its id,
    /// empty when it went untagged.
    pub(super) fn unanswered_send(&self) -> Option<String> {
        self.health.sent_unanswered.then(|| self.health.sent_id.clone().unwrap_or_default())
    }

    /// The composer's marked cards changed: keep only the unconfirmed sends
    /// still held. Read from the composer now, not from the event: a restore
    /// can mark a card after an earlier event was queued.
    pub(super) fn sync_unconfirmed(&mut self, cx: &mut Context<Self>) {
        let marked = self.composer.read(cx).unconfirmed_texts();
        let before = self.health.unconfirmed_held.len();
        self.health.unconfirmed_held.retain(|u| marked.contains(&u.text));
        if self.health.unconfirmed_held.len() != before {
            self.meta_dirty.set(true);
        }
    }

    /// Whether work was running that a restart would cut short: a turn,
    /// background tasks, or a continuation.
    pub(super) fn mid_turn(&self) -> bool {
        let inputs = self.health_inputs();
        inputs.turn_active || inputs.background_active > 0 || inputs.continuations_open > 0
    }

    /// The error a dead process leaves, with its code when known.
    pub(super) fn exit_error(&self) -> String {
        match self.health.exit {
            Some(ExitProof::ExitCode(code)) => exit_message(Some(code)),
            _ => exit_message(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::health::{compute, HealthConfig, HealthStatus, Liveness};
    use oximux_agents::thread::StubConnection;
    use oximux_settings::{Density, Theme, Typography};

    use super::*;

    fn chat(cx: &mut TestAppContext, stub: StubConnection) -> gpui::WindowHandle<AgentChatView> {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(stub),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
    }

    fn health_of(view: &AgentChatView) -> (HealthStatus, Liveness, Option<String>) {
        let h = compute(&view.health_inputs(), now_ms(), &HealthConfig::default());
        (h.status, h.liveness, h.reason)
    }

    /// A process that dies on its own is a failure named with its code, in
    /// the health and on the chat's error card.
    #[gpui::test]
    async fn a_dead_process_reads_failed_with_its_exit_code(cx: &mut TestAppContext) {
        let stub = StubConnection::default();
        stub.set_exit_code(Some(137));
        let window = chat(cx, stub);
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.on_disconnect(cx);
                let (status, liveness, reason) = health_of(view);
                assert_eq!(status, HealthStatus::Failed);
                assert_eq!(liveness, Liveness::Exited(ExitProof::ExitCode(137)));
                assert_eq!(reason.as_deref(), Some("agent process exited (code 137)"));
                assert_eq!(view.thread.last_error.as_deref(), Some("Agent process exited (code 137)."));
            })
            .expect("window update");
    }

    /// A process that exits after the user's Stop was asked to.
    #[gpui::test]
    async fn an_exit_after_stop_reads_interrupted(cx: &mut TestAppContext) {
        let stub = StubConnection::default();
        stub.set_exit_code(Some(143));
        let window = chat(cx, stub);
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.stop_turn(cx);
                view.on_disconnect(cx);
                let (status, liveness, _) = health_of(view);
                assert_eq!((status, liveness), (HealthStatus::Interrupted, Liveness::Exited(ExitProof::UserStop)));
                assert!(view.thread.last_error.is_none(), "no error card for a Stop");
            })
            .expect("window update");
    }

    /// After its turn ended, a parent woken for a finished background task is
    /// working, not idle, until the continuation closes.
    #[gpui::test]
    async fn a_continuation_reads_working_until_it_closes(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("go");
                view.on_event(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None }, cx);
                assert_eq!(health_of(view).0, HealthStatus::Idle);
                view.on_event(ThreadEvent::ProviderTurnStarted, cx);
                assert_eq!(health_of(view).0, HealthStatus::Working);
                view.on_event(ThreadEvent::ContinuationEnded, cx);
                assert_eq!(health_of(view).0, HealthStatus::Idle);
            })
            .expect("window update");
    }

    /// What the host derives from the inputs the view publishes is exactly
    /// what the view itself would compute from them.
    #[gpui::test]
    async fn the_host_derives_the_health_the_view_computes(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                let registry = oximux_agents::session_registry::SessionRegistry::new();
                let handle = registry.register("s".into(), Arc::new(StubConnection::default()));
                view.thread.push_user_message("go");
                view.health.note_sent(now_ms(), None);
                for (label, step) in [("sent", 0), ("continuation", 1), ("failed", 2)] {
                    match step {
                        1 => {
                            view.on_event(ThreadEvent::TurnEnded { result: None, usage: None, is_error: false, turn_diff: None }, cx);
                            view.on_event(ThreadEvent::ProviderTurnStarted, cx);
                        }
                        2 => {
                            view.on_event(ThreadEvent::ContinuationEnded, cx);
                            view.thread.push_user_message("again");
                            view.on_event(ThreadEvent::TurnEnded { result: Some("API Error: 500".into()), usage: None, is_error: true, turn_diff: None }, cx);
                        }
                        _ => {}
                    }
                    let inputs = view.health_inputs();
                    handle.publish_health(inputs.clone());
                    let mine = compute(&inputs, now_ms(), &HealthConfig::default());
                    let host = handle.health();
                    assert_eq!((host.status, host.liveness, host.reason), (mine.status, mine.liveness, mine.reason), "{label}");
                }
                assert_eq!(handle.health().status, HealthStatus::Failed);
            })
            .expect("window update");
    }

    /// A failed fork or rewind leaves an error on the chat, not a failed turn.
    #[gpui::test]
    async fn a_non_turn_error_does_not_read_failed(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, _cx| {
                view.thread.last_error = Some("Fork failed: no session".into());
                assert_eq!(health_of(view).0, HealthStatus::Idle);
            })
            .expect("window update");
    }

    /// A restored chat is unconfirmed until a frame arrives on its connection.
    #[gpui::test]
    async fn a_restored_chat_is_unconfirmed_until_its_first_frame(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                view.restore_health(crate::shell::agent_chat::RestoredTurn { mid_turn: true, ..Default::default() }, cx);
                let (status, liveness, _) = health_of(view);
                assert_eq!(liveness, Liveness::RestoredUnconfirmed, "no lock proof in a test: no claim");
                assert_ne!(status, HealthStatus::Failed);
                view.health.note_event(&ThreadEvent::ProviderTurnStarted, now_ms());
                assert_eq!(health_of(view).1, Liveness::Live);
            })
            .expect("window update");
    }

    /// A chat restored mid-turn tells the agent on the next message, once
    /// (F6, root); Remove drops the note.
    #[gpui::test]
    async fn a_mid_turn_restore_attaches_a_recovery_note_to_the_next_message(cx: &mut TestAppContext) {
        let stub = StubConnection::default();
        let window = chat(cx, stub.clone());
        let turn = || crate::shell::agent_chat::RestoredTurn {
            mid_turn: true,
            stopped_tasks: vec!["npm test".into()],
            ..Default::default()
        };
        window
            .update(cx, |view, _window, cx| {
                view.restore_health(turn(), cx);
                view.send_text("do it".into(), Vec::new(), cx);
                view.thread.turn_active = false;
                view.send_text("again".into(), Vec::new(), cx);
                view.restore_health(turn(), cx);
                view.remove_recovery_note(cx);
            })
            .expect("window update");
        cx.run_until_parked();
        let sent: Vec<String> = stub.sent().iter().map(|v| v.to_string()).collect();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains("OxiMux restarted at") && sent[0].contains("npm test") && sent[0].contains("do it"));
        assert!(!sent[1].contains("OxiMux restarted"), "once: {}", sent[1]);
        window
            .update(cx, |view, _window, _cx| assert!(view.recovery_note.is_none(), "Remove drops it"))
            .expect("window update");
    }

    /// The composer's held and queued counts survive a restore, whichever
    /// order its `QueueChanged` and the restore land in.
    #[gpui::test]
    async fn a_restore_keeps_the_composer_s_counts(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                (view.health.held, view.health.queued) = (3, 1);
                view.restore_health(crate::shell::agent_chat::RestoredTurn::default(), cx);
                assert_eq!((view.health.held, view.health.queued), (3, 1));
            })
            .expect("window update");
    }

    /// `/clear` ends the old conversation's debts: no recovery note on the new
    /// session's first message, and its failed turn holds nothing.
    #[gpui::test]
    async fn a_cleared_chat_drops_the_recovery_note_and_the_failed_hold(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                let turn = crate::shell::agent_chat::RestoredTurn { mid_turn: true, ..Default::default() };
                view.restore_health(turn, cx);
                let failed = ThreadEvent::TurnEnded {
                    result: Some("boom".into()),
                    usage: None,
                    is_error: true,
                    turn_diff: None,
                };
                view.health.note_event(&failed, now_ms());
                assert!(view.recovery_note.is_some() && view.health.holds_queue());
                view.new_chat(cx);
                assert!(view.recovery_note.is_none(), "no note for a fresh session");
                assert!(!view.health.holds_queue(), "the old failure holds nothing");
            })
            .expect("window update");
    }

    /// A send nothing answered before the quit comes back held first, marked
    /// "may already have been sent"; nothing resends it.
    #[gpui::test]
    async fn an_unanswered_send_comes_back_held_and_marked(cx: &mut TestAppContext) {
        let stub = StubConnection::default();
        let window = chat(cx, stub.clone());
        window
            .update(cx, |view, window, cx| {
                view.thread.push_user_message_with_images("ship it".to_string(), Vec::new());
                view.seed_draft_and_queue(None, vec!["then tests".into()], window, cx);
                view.restore_health(
                    crate::shell::agent_chat::RestoredTurn { unanswered_send: Some("u-1".into()), ..Default::default() },
                    cx,
                );
                let held = view.composer.read(cx).held_for_test().to_vec();
                assert_eq!(held.len(), 2);
                assert!(held[0].text == "ship it" && held[0].maybe_sent);
                assert!(!held[1].maybe_sent);
                // The composer's queued events land after both (the test view
                // has no composer subscription, so run its handler by hand):
                // the plain card's event came first and must not clear the
                // mark. Saved again before the user acts, it is still marked
                // next time, and not also saved as a plain queued text.
                view.sync_unconfirmed(cx);
                view.sync_unconfirmed(cx);
                let marked = crate::persisted_chat::UnconfirmedSend { id: "u-1".into(), text: "ship it".into() };
                assert_eq!(view.health.unconfirmed_held, vec![marked]);
                assert_eq!(view.queued_texts(cx), vec!["then tests"]);
            })
            .expect("window update");
        assert!(stub.sent().is_empty(), "nothing resent");
    }

    /// A marked card keeps its own text across a later send and a second
    /// restart, though the latest user entry is now that later message; a
    /// dropped card is forgotten.
    #[gpui::test]
    async fn a_marked_card_keeps_its_text_across_a_later_send(cx: &mut TestAppContext) {
        let window = chat(cx, StubConnection::default());
        window
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message_with_images("ship it".to_string(), Vec::new());
                let turn = crate::shell::agent_chat::RestoredTurn { unanswered_send: Some("u-1".into()), ..Default::default() };
                view.restore_health(turn, cx);
                view.send_text("something else".into(), Vec::new(), cx);
                view.health.note_event(&ThreadEvent::ProviderTurnStarted, now_ms());
                // What the blob saves (`build_transcript` copies these).
                assert_eq!(view.unanswered_send(), None, "the later send was answered");
                let saved = view.health.unconfirmed_held.clone();
                assert_eq!(saved.iter().map(|u| u.text.as_str()).collect::<Vec<_>>(), ["ship it"]);

                let turn = crate::shell::agent_chat::RestoredTurn { unconfirmed_held: saved, ..Default::default() };
                view.composer.update(cx, |c, cx| c.drop_all_held_for_test(cx));
                view.restore_health(turn, cx);
                let held = view.composer.read(cx).held_for_test().to_vec();
                assert_eq!(held.len(), 1);
                assert!(held[0].text == "ship it" && held[0].maybe_sent);
                // Dropped: forgotten (the handler, run by hand as above).
                view.composer.update(cx, |c, cx| c.drop_all_held_for_test(cx));
                view.sync_unconfirmed(cx);
                assert!(view.health.unconfirmed_held.is_empty(), "dropped, forgotten");
            })
            .expect("window update");
    }

    #[test]
    fn a_frame_answers_a_send_and_confirms_a_restore() {
        let mut t = HealthTrack::restored(true, true);
        t.note_sent(10, None);
        assert!(t.sent_unanswered);
        t.note_event(&ThreadEvent::UserMessageAccepted { uuid: "u".into() }, 20);
        assert_eq!(t.accepted_at, Some(20), "acceptance starts the not-started clock");
        assert!(t.sent_unanswered && t.restored.is_some(), "a receipt answers nothing");
        t.note_event(&ThreadEvent::ProviderTurnStarted, 30);
        assert!(!t.sent_unanswered && t.accepted_at.is_none() && t.restored.is_none());
        assert_eq!(t.last_activity_at, 30);
    }

    #[test]
    fn an_exit_is_recorded_with_its_proof() {
        let mut t = HealthTrack::default();
        assert_eq!(t.note_exit(false, Some(137)), ExitProof::ExitCode(137));
        assert_eq!(t.note_exit(true, Some(143)), ExitProof::UserStop, "a Stop wins over the code");
        assert_eq!(t.note_exit(false, None), ExitProof::ConnectionClosed);
        assert_eq!(t.exits, 3);
        t.note_connected();
        assert!(t.exit.is_none());
        assert_eq!(exit_message(Some(137)), "Agent process exited (code 137).");
    }
}
