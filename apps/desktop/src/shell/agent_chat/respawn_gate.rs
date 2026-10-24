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
            self.respawn_with_env(Vec::new(), None, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::StubConnection;
    use oximux_settings::{Density, Theme, Typography};

    use super::AgentChatView;

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
