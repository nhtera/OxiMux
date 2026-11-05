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
    /// closed. Fail closed: if a permission was still pending, reject it (the
    /// tool never ran, since the process is gone). Best-effort deny in case
    /// stdin is briefly still writable, then mark the tool `Rejected` so the UI
    /// never shows a dangling approval prompt.
    pub(super) fn on_disconnect(&mut self, cx: &mut Context<Self>) {
        // Record how the process ended while the session is still bound, so the
        // host sees the exit before the session leaves its list.
        self.record_exit(cx);
        // The live process is gone: drop the remote binding so the phone's session
        // list reflects only live sessions (a resume respawns + re-binds).
        self.unbind_remote();
        self.revoke_spawn_credential(cx);
        let pending = self
            .thread
            .pending_permission()
            .map(|(tool_id, req)| (tool_id.to_string(), req.request_id.clone()));
        if let Some((tool_id, request_id)) = pending {
            if let Some(conn) = &self.connection {
                let _ = conn.resolve_permission(
                    &request_id,
                    PermissionDecision::Deny { message: "agent disconnected".into() },
                );
            }
            self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
        }
        // Fail-close a pending AskUserQuestion too: the process is gone, so it can
        // never be answered — reject it and drop its card rather than stranding an
        // unanswerable prompt.
        if let Some(tool_id) = self.thread.pending_question().map(|(id, _)| id.to_string()) {
            self.thread.set_tool_status(&tool_id, ToolCallStatus::Rejected);
            self.question_cards.remove(&tool_id);
            self.question_card_subs.remove(&tool_id);
        }
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
