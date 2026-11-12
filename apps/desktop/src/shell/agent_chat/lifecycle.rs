//! What happens when a turn ends and when the agent's process goes away:
//! release the next queued message, or fail closed on a dead connection.

use super::*;

impl AgentChatView {
    /// Send the oldest message the composer parked while a turn streamed, if any.
    /// Driven off the natural turn-end edge in [`Self::on_event`]; since each
    /// completed turn releases exactly one, a lined-up batch drains in order.
    ///
    /// Never after a failed turn: the queue waits (chips read "Waiting · agent
    /// failed") until the user sends, so a failing session is not fed the rest.
    pub(super) fn flush_next_queued(&mut self, cx: &mut Context<Self>) {
        if self.health.holds_queue() {
            return;
        }
        match self.composer.update(cx, |c, cx| c.take_next_queued(cx)) {
            Some((text, images)) => self.send_text(text, images, cx),
            None => self.flush_send_gate(cx),
        }
    }

    /// The event channel closed — the agent process exited or its stdout was
    /// closed. Fail closed: every prompt it was waiting on is retired (the
    /// tools never ran, since the process is gone), with a best-effort deny
    /// first in case stdin is briefly still writable.
    pub(super) fn on_disconnect(&mut self, cx: &mut Context<Self>) {
        // Record how the process ended while the session is still bound, so the
        // host sees the exit before the session leaves its list.
        self.record_exit(cx);
        // Best-effort deny for every one of them, in case stdin is briefly
        // still writable — a child whose stdout closed can still be alive and
        // blocked. Answering only the oldest would leave it waiting on the
        // rest until something else reaps it.
        let requests: Vec<String> = self
            .thread
            .pending_permissions()
            .iter()
            .map(|(_, req)| req.request_id.clone())
            .collect();
        if let Some(conn) = &self.connection {
            for request_id in &requests {
                let _ = conn.resolve_permission(
                    request_id,
                    PermissionDecision::Deny { message: "agent disconnected".into() },
                );
            }
        }
        // Every one of them, not just the first: a process can exit with
        // several outstanding, and one left `WaitingForConfirmation` both
        // strands an unanswerable card and makes `defer_respawn_mid_turn` hold
        // a respawn this dead chat can never run. Before the unbind, so a
        // subscriber still on the stream is told.
        self.retire_pending_prompts();
        // The live process is gone: drop the remote binding so the phone's session
        // list reflects only live sessions (a resume respawns + re-binds).
        self.unbind_remote();
        self.revoke_spawn_credential(cx);
        self.thread.turn_active = false;
        if self.interrupted {
            // Intentional Stop: the child exited exactly as asked. Stay
            // resumable-idle (the next send respawns via `--resume`) instead of
            // marking the tab unavailable.
            self.thread.last_error = None;
            self.sync_composer(cx);
            cx.notify();
            return;
        }
        self.disconnected = true;
        if self.thread.last_error.is_none() {
            self.thread.last_error = Some(self.exit_error());
        }
        self.sync_composer(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::TestAppContext;
    use oximux_agents::thread::{StubConnection, ThreadEvent};
    use oximux_settings::{Density, Theme, Typography};
    use serde_json::json;

    use super::AgentChatView;

    /// A process whose stdout closed can still be alive and blocked, so the
    /// fail-closed path answers every request it was waiting on — and leaves
    /// no card claiming the agent is waiting on a person.
    #[gpui::test]
    async fn disconnect_fails_closed_pending_permission(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let stub = StubConnection::default();
        let stub_probe = stub.clone();
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
            .update(cx, |view, _window, cx| {
                view.thread.push_user_message("edit notes");
                // A second request the same child is also blocked on: a deny
                // for the oldest alone leaves it waiting on this one.
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t2".into(),
                    name: "Bash".into(),
                    input: json!({"command": "rm -rf build"}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r2".into(),
                    tool_use_id: Some("t2".into()),
                    tool_name: "Bash".into(),
                    input: json!({"command": "rm -rf build"}),
                    description: "build".into(),
                    suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                });
                view.thread.apply(&ThreadEvent::ToolCallStarted {
                    id: "t1".into(),
                    name: "Edit".into(),
                    input: json!({"file_path": "notes.txt"}),
                });
                view.thread.apply(&ThreadEvent::PermissionRequested {
                    request_id: "r1".into(),
                    tool_use_id: Some("t1".into()),
                    tool_name: "Edit".into(),
                    input: json!({}),
                    description: "notes.txt".into(),
                    suggestions: vec![],
                    kind: oximux_agents::thread::PermissionKind::Tool,
                });
                assert_eq!(
                    view.thread.pending_permissions().len(),
                    2,
                    "both are pending before disconnect"
                );

                view.on_disconnect(cx);

                assert!(
                    view.thread.pending_permissions().is_empty(),
                    "fail-closed clears every pending permission, not just the oldest"
                );
                assert!(view.disconnected, "view marks itself disconnected");
            })
            .expect("window update");

        // A best-effort deny reached the (stub) connection for EACH of them:
        // stdout can close while the child is alive, and a child answered on
        // one request is still blocked on the other.
        let sent = stub_probe.sent();
        let denied: Vec<_> = sent
            .iter()
            .filter(|s| s["response"]["response"]["behavior"] == "deny")
            .filter_map(|s| s["response"]["request_id"].as_str().map(str::to_string))
            .collect();
        assert_eq!(denied.len(), 2, "one deny per pending request, got {sent:?}");
        for id in ["r1", "r2"] {
            assert!(denied.iter().any(|d| d == id), "{id} was denied, got {denied:?}");
        }
    }
}
