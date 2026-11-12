//! No respawn mid-turn.
//!
//! A respawn reaps the live process and starts a new one. A model, effort or
//! fast-mode switch, a posture or credential change, arriving while a turn is
//! streaming would strand the reply half-written (or, with a permission or
//! question card open, kill the process that asked). So such a respawn is held
//! until the turn ends and run once then, before the next queued message is
//! taken — N changes in one turn coalesce into one respawn. Stop and an
//! interrupt end the turn first, so they never wait; any respawn that does run
//! satisfies a pending one.

use gpui::Context;

use super::AgentChatView;

impl AgentChatView {
    /// Reap the process being replaced and retire everything it was waiting
    /// on.
    ///
    /// `Child`'s Drop neither kills nor waits, and a Stop now interrupts the
    /// turn over the protocol rather than signalling the process, so after one
    /// the child is still *alive* and this is what ends it; it also harvests a
    /// child that died on its own.
    ///
    /// Then the prompts. A permission or a question is answered by routing a
    /// decision back to the process that raised it, by request id, so once
    /// that process is gone nothing can carry an answer there — the request is
    /// dead whatever the card still says. Left alone it tells the person an
    /// agent is blocked on them, offers buttons that now fail, and keeps the
    /// derived health reporting `NeedsPermission` with a request id nobody can
    /// act on, which puts the session in the rail's "needs you" for good.
    ///
    /// Done before the new connection rather than after a successful one, so
    /// it covers a respawn that fails (the session ends up dead and the
    /// request is just as unanswerable) and a dormant restore, whose first
    /// connect comes through here with the pre-quit card still in the fold.
    pub(super) fn end_the_old_process(&mut self) {
        if let Some(old) = self.connection.take() {
            old.shutdown();
        }
        // Retire the drain with the process, not with the *successful* start of
        // its replacement. `start_drain` bumps this too, but only on the Ok
        // path: a respawn that fails to spawn would otherwise leave the dead
        // child's drain current, and whatever it had already decoded and queued
        // would still be folded — re-raising the very prompts retired below.
        self.drain_gen += 1;
        self.retire_pending_prompts();
    }

    /// Tell this chat — and anyone watching it — that the prompts the departed
    /// process raised are dead.
    ///
    /// The fold, the question cards and every remote subscriber have to learn
    /// it, so it travels as one [`ThreadEvent::PromptsRetired`]: applied here
    /// and ingested into the event ring, which is how an already-open phone
    /// follows this session. Publishing the transcript instead would not do —
    /// that snapshot is read by clients *opening* the session, and a phone that
    /// is already in the chat would keep its answer buttons.
    pub(super) fn retire_pending_prompts(&mut self) {
        let tool_use_ids = self.thread.pending_prompt_ids();
        if tool_use_ids.is_empty() {
            return;
        }
        // The card entities are keyed by tool id and the fold no longer shows
        // these as asked, so drop them with the prompt rather than leaving the
        // view holding a card for a question that has been cut off.
        for id in &tool_use_ids {
            self.question_cards.remove(id);
            self.question_card_subs.remove(id);
        }
        let retired = oximux_agents::thread::ThreadEvent::PromptsRetired { tool_use_ids };
        self.thread.apply(&retired);
        if let Some(binding) = self.remote.as_ref() {
            binding.ingest(retired);
        }
        // The host derives this session's state from the fold, so it has to
        // hear that the prompt is gone — otherwise the rail keeps offering a
        // request nobody can answer.
        self.publish_health();
    }

    /// Whether a respawn asked for now has to wait for the turn to end. When it
    /// does, it is recorded and runs from [`Self::run_pending_respawn`].
    ///
    /// Never for a chat with no live process. Deferral exists to protect a
    /// streaming reply and the process that raised an open card; with the
    /// process gone there is nothing to protect, and waiting would be a trap:
    /// the deferred respawn runs off the turn-end edge, which a dead chat can
    /// never reach, so Retry would quietly do nothing for good.
    pub(super) fn defer_respawn_mid_turn(&mut self) -> bool {
        if self.disconnected || self.connection.is_none() {
            return false;
        }
        let busy = self.thread.turn_active
            || self.thread.pending_permission().is_some()
            || self.thread.pending_question().is_some();
        if busy {
            tracing::debug!("respawn deferred until the turn ends");
            self.pending_respawn = true;
        }
        busy
    }

    /// Respawn so the next process starts with this chat's credential (local
    /// access or confinement was just switched on). Deferred like any other
    /// respawn while a turn is live; nothing to do for a chat not yet spawned,
    /// which picks one up when it connects.
    pub(crate) fn respawn_for_credential(&mut self, cx: &mut Context<Self>) {
        if !self.unbound && self.connection.is_some() {
            self.respawn(cx);
        }
    }

    /// The turn ended: run the respawn a mid-turn change asked for, if any,
    /// before anything else uses the connection.
    pub(super) fn run_pending_respawn(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.pending_respawn) {
            #[cfg(test)]
            if tests::RESPAWN_IN_PLACE.with(std::cell::Cell::get) {
                self.drain_gen += 1; // what the real respawn's `end_the_old_process` does
                return;
            }
            self.respawn_with_env(Vec::new(), None, cx);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::StubConnection;
    use oximux_settings::{Density, Theme, Typography};

    use super::AgentChatView;

    thread_local! {
        /// Stand the deferred respawn in with its drain switch alone: a test
        /// cannot spawn the real agent (see `drain`'s tests).
        pub(crate) static RESPAWN_IN_PLACE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Mid-turn, a respawn is recorded instead of reaping the live process,
    /// and several requests in one turn coalesce; idle, nothing waits.
    #[gpui::test]
    async fn a_mid_turn_respawn_waits_and_coalesces(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _window, cx| {
                let live = view.connection.clone().expect("connected");
                assert!(!view.defer_respawn_mid_turn(), "an idle chat respawns at once");
                assert!(!view.pending_respawn);

                view.thread.turn_active = true;
                view.respawn(cx);
                view.respawn(cx);
                assert!(view.pending_respawn, "recorded for the turn's end");
                let still = view.connection.clone().expect("still connected");
                assert!(Arc::ptr_eq(&live, &still), "the streaming process is not reaped mid-turn");
            })
            .expect("window update");
    }

    fn chat(cx: &mut TestAppContext) -> gpui::WindowHandle<AgentChatView> {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Arc::new(StubConnection::default()),
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

    /// Raise `n` permission requests, as a process can before it dies.
    fn ask_for_permissions(view: &mut AgentChatView, n: usize) {
        use oximux_agents::thread::{PermissionKind, ThreadEvent};
        for i in 0..n {
            let (tool, rid) = (format!("toolu_{i}"), format!("rid_{i}"));
            view.thread.apply(&ThreadEvent::ToolCallStarted {
                id: tool.clone(),
                name: "Bash".into(),
                input: serde_json::json!({"command": format!("rm -rf {i}")}),
            });
            view.thread.apply(&ThreadEvent::PermissionRequested {
                request_id: rid,
                tool_use_id: Some(tool),
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": format!("rm -rf {i}")}),
                description: String::new(),
                suggestions: vec![],
                kind: PermissionKind::Tool,
            });
        }
    }

    /// The trap: a process can exit with several prompts outstanding. Clearing
    /// only the first left one `WaitingForConfirmation`, which made this gate
    /// defer the respawn Retry asks for — and a deferred respawn runs off the
    /// turn-end edge, which a dead process never reaches. Retry then did
    /// nothing, for good.
    #[gpui::test]
    async fn a_dead_chat_defers_no_respawn_behind_a_prompt_nothing_can_answer(
        cx: &mut TestAppContext,
    ) {
        let window = chat(cx);
        window
            .update(cx, |view, _window, cx| {
                ask_for_permissions(view, 2);
                assert_eq!(view.thread.pending_prompt_ids().len(), 2, "two were outstanding");

                view.on_disconnect(cx);

                assert!(
                    view.thread.pending_prompt_ids().is_empty(),
                    "every one of them is retired, not just the first"
                );
                assert!(
                    !view.defer_respawn_mid_turn(),
                    "a dead chat waits for nothing, so Retry can respawn it"
                );
                assert!(!view.pending_respawn, "and nothing is left recorded to run later");
            })
            .expect("window update");
    }

    /// And a prompt that lands *after* the process has gone — the drain had it
    /// queued — must not re-arm the trap. The gate asks whether there is a live
    /// process to protect, not only what the fold still shows.
    #[gpui::test]
    async fn a_prompt_that_arrives_after_the_process_died_defers_nothing(cx: &mut TestAppContext) {
        let window = chat(cx);
        window
            .update(cx, |view, _window, cx| {
                view.on_disconnect(cx);
                ask_for_permissions(view, 1);
                assert_eq!(view.thread.pending_prompt_ids().len(), 1, "the fold shows it waiting");

                assert!(
                    !view.defer_respawn_mid_turn(),
                    "there is no live process to strand, so nothing is deferred"
                );
            })
            .expect("window update");
    }

    /// A phone already in this chat follows the event ring, not the transcript
    /// snapshot (which is read by clients *opening* a session). So the
    /// retirement has to travel as an event, or the phone keeps offering
    /// answer buttons for a process that no longer exists.
    #[gpui::test]
    async fn a_watching_phone_is_told_the_prompts_were_retired(cx: &mut TestAppContext) {
        use crate::remote_control::RemoteControl;
        cx.update(|cx| {
            let rc = RemoteControl::new();
            rc.set_enabled(true); // a paired phone can read this session
            cx.set_global(rc);
        });
        let window = chat(cx);
        let session = window
            .update(cx, |view, _window, cx| {
                ask_for_permissions(view, 1);
                assert!(view.remote.is_some(), "the test chat is bound into the registry");
                view.retire_pending_prompts();
                let _ = cx;
                view.remote_session_id.clone()
            })
            .expect("window update");
        // Read it the way a remote reader does: the session's event ring.
        let ring = cx.update(|cx| {
            cx.global::<RemoteControl>().registry().events_since(&session, 0)
        });

        let retired: Vec<_> = ring
            .iter()
            .filter_map(|(_, ev)| match ev {
                oximux_agents::thread::ThreadEvent::PromptsRetired { tool_use_ids } => {
                    Some(tool_use_ids.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            retired,
            vec![vec!["toolu_0".to_string()]],
            "the ring carries the retirement, naming the prompt it ends: {ring:?}"
        );
    }
}
