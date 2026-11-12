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
        let cut_off = self.thread.interrupt_pending_prompts();
        if cut_off == 0 {
            return;
        }
        self.thread.apply(&oximux_agents::thread::ThreadEvent::Notice(match cut_off {
            1 => "The agent restarted. The request it was waiting on was cut off; \
                  nothing was approved."
                .to_string(),
            n => format!(
                "The agent restarted. The {n} requests it was waiting on were cut off; \
                 nothing was approved."
            ),
        }));
        // The host derives this session's state from the fold, so it has to
        // hear that the prompt is gone — otherwise the rail keeps offering a
        // request nobody can answer.
        self.publish_health();
    }

    /// Whether a respawn asked for now has to wait for the turn to end. When it
    /// does, it is recorded and runs from [`Self::run_pending_respawn`].
    pub(super) fn defer_respawn_mid_turn(&mut self) -> bool {
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
                self.drain_gen += 1; // what the real respawn's `start_drain` does
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
}
