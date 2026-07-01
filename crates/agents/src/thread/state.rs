//! `ChatThread` — the in-memory conversation state machine.
//!
//! Folds a stream of `ThreadEvent`s into an ordered `Vec<ThreadEntry>` (the
//! message history OxiMux lacks today). Pure and gpui-free: the app crate
//! owns a GPUI entity that holds a `ChatThread`, calls `apply` on each event,
//! and repaints. Streaming deltas build the current assistant message and are
//! reconciled by the finalized `AssistantText`/`AssistantThinking` so text is
//! never doubled.

use super::entry::{AssistantMessage, ThreadEntry};
use super::event::ThreadEvent;
use super::tool_call::{PermissionRequest, ToolCall, ToolCallStatus};

#[derive(Debug, Default)]
pub struct ChatThread {
    pub entries: Vec<ThreadEntry>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// Latest one-line turn summary (`post_turn_summary`), for a status chip.
    pub last_summary: Option<String>,
    /// Latest transport/protocol error, surfaced non-fatally.
    pub last_error: Option<String>,
    /// Whether a turn is currently in flight (between a user send and
    /// `TurnEnded`). Drives the composer's send/stop affordance.
    pub turn_active: bool,
    /// Index of the assistant entry currently being streamed, if any.
    current_assistant: Option<usize>,
}

impl ChatThread {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a user prompt (called when we send one to the agent).
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.entries.push(ThreadEntry::User(text.into()));
        self.current_assistant = None;
        self.turn_active = true;
    }

    /// Fold one decoded event into the transcript.
    pub fn apply(&mut self, event: &ThreadEvent) {
        match event {
            ThreadEvent::SessionInit { session_id, model, permission_mode } => {
                self.session_id = Some(session_id.clone());
                self.model = Some(model.clone());
                self.permission_mode = Some(permission_mode.clone());
            }
            ThreadEvent::AssistantTextDelta(t) => self.assistant_mut().text.push_str(t),
            ThreadEvent::ThinkingDelta(t) => self.assistant_mut().thinking.push_str(t),
            // Finalized blocks REPLACE (reconcile) the streamed value.
            ThreadEvent::AssistantText(t) => self.assistant_mut().text = t.clone(),
            ThreadEvent::AssistantThinking(t) => self.assistant_mut().thinking = t.clone(),
            ThreadEvent::ToolCallStarted { id, name, input } => {
                self.entries.push(ThreadEntry::ToolCall(ToolCall::new(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                )));
                // A tool call ends the current assistant text block; later text
                // starts a fresh assistant message.
                self.current_assistant = None;
            }
            ThreadEvent::ToolResult { tool_use_id, content, is_error } => {
                if let Some(tc) = self.tool_call_mut(tool_use_id) {
                    tc.result = Some(content.clone());
                    tc.status = if *is_error {
                        ToolCallStatus::Failed(content.clone())
                    } else {
                        ToolCallStatus::Completed
                    };
                }
            }
            ThreadEvent::PermissionRequested {
                request_id, tool_use_id, tool_name, input, description, suggestions,
            } => {
                let req = PermissionRequest {
                    request_id: request_id.clone(),
                    description: description.clone(),
                    suggestions: suggestions.clone(),
                };
                let matched = tool_use_id
                    .as_deref()
                    .and_then(|id| self.tool_call_mut(id));
                match matched {
                    Some(tc) => tc.status = ToolCallStatus::WaitingForConfirmation(req),
                    None => {
                        // Permission arrived without a matching tool_use block
                        // (unexpected ordering) — synthesize the card anyway.
                        let mut tc = ToolCall::new(
                            tool_use_id.clone().unwrap_or_else(|| request_id.clone()),
                            tool_name.clone(),
                            input.clone(),
                        );
                        tc.status = ToolCallStatus::WaitingForConfirmation(req);
                        self.entries.push(ThreadEntry::ToolCall(tc));
                    }
                }
            }
            ThreadEvent::TurnSummary { detail, .. } => {
                self.last_summary = Some(detail.clone());
                self.current_assistant = None;
            }
            ThreadEvent::TurnEnded { is_error, result, .. } => {
                self.turn_active = false;
                self.current_assistant = None;
                if *is_error {
                    self.last_error = result.clone().or(Some("turn ended with error".into()));
                }
            }
            ThreadEvent::Error(msg) => {
                self.last_error = Some(msg.clone());
                self.turn_active = false;
            }
        }
    }

    /// Transition a tool call's status directly (e.g. to `InProgress` when the
    /// user allows, or `Rejected` when they deny) — called by the connection
    /// layer alongside sending the control response.
    pub fn set_tool_status(&mut self, tool_id: &str, status: ToolCallStatus) {
        if let Some(tc) = self.tool_call_mut(tool_id) {
            tc.status = status;
        }
    }

    /// The pending permission request (if any) awaiting a decision.
    pub fn pending_permission(&self) -> Option<(&str, &PermissionRequest)> {
        self.entries.iter().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) => match &tc.status {
                ToolCallStatus::WaitingForConfirmation(req) => Some((tc.id.as_str(), req)),
                _ => None,
            },
            _ => None,
        })
    }

    fn tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.entries.iter_mut().rev().find_map(|e| match e {
            ThreadEntry::ToolCall(tc) if tc.id == id => Some(tc),
            _ => None,
        })
    }

    /// The assistant message currently being built, creating one if the last
    /// entry isn't an in-progress assistant message.
    fn assistant_mut(&mut self) -> &mut AssistantMessage {
        if self.current_assistant.is_none() {
            self.entries.push(ThreadEntry::Assistant(AssistantMessage::default()));
            self.current_assistant = Some(self.entries.len() - 1);
        }
        let idx = self.current_assistant.expect("just set");
        match &mut self.entries[idx] {
            ThreadEntry::Assistant(m) => m,
            _ => unreachable!("current_assistant always indexes an Assistant entry"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant_text(e: &ThreadEntry) -> &str {
        match e {
            ThreadEntry::Assistant(m) => &m.text,
            _ => panic!("not an assistant entry"),
        }
    }

    #[test]
    fn builds_a_simple_turn() {
        let mut t = ChatThread::new();
        t.push_user_message("hi");
        t.apply(&ThreadEvent::AssistantTextDelta("Hel".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("lo".into()));
        t.apply(&ThreadEvent::AssistantText("Hello!".into())); // finalize reconciles
        t.apply(&ThreadEvent::TurnEnded { result: Some("Hello!".into()), cost_usd: Some(0.1), is_error: false });

        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0], ThreadEntry::User("hi".into()));
        assert_eq!(assistant_text(&t.entries[1]), "Hello!"); // not "Hellolo Hello!"
        assert!(!t.turn_active);
    }

    #[test]
    fn tool_call_flows_to_completed() {
        let mut t = ChatThread::new();
        t.push_user_message("read it");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_1".into(), content: "file body".into(), is_error: false });

        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.name, "Read");
                assert_eq!(tc.status, ToolCallStatus::Completed);
                assert_eq!(tc.result.as_deref(), Some("file body"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn permission_marks_matching_tool_call_and_is_findable() {
        let mut t = ChatThread::new();
        t.push_user_message("edit");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_9".into(), name: "Edit".into(), input: json!({}) });
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-1".into(), tool_use_id: Some("toolu_9".into()),
            tool_name: "Edit".into(), input: json!({}), description: "notes.txt".into(),
            suggestions: vec![] });

        let (tool_id, req) = t.pending_permission().expect("a pending permission");
        assert_eq!(tool_id, "toolu_9");
        assert_eq!(req.request_id, "rid-1");
        assert_eq!(req.description, "notes.txt");
        // only one tool-call entry was created (permission matched, not duplicated)
        assert_eq!(t.entries.iter().filter(|e| matches!(e, ThreadEntry::ToolCall(_))).count(), 1);

        // deny → rejected, no longer pending
        t.set_tool_status("toolu_9", ToolCallStatus::Rejected);
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn text_then_tool_then_text_makes_two_assistant_messages() {
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("Let me check.".into()));
        t.apply(&ThreadEvent::ToolCallStarted { id: "t1".into(), name: "Bash".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult { tool_use_id: "t1".into(), content: "ok".into(), is_error: false });
        t.apply(&ThreadEvent::AssistantText("Done.".into()));

        let assistants: Vec<&str> = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.as_str()), _ => None }).collect();
        assert_eq!(assistants, vec!["Let me check.", "Done."]);
        // order: User, Assistant, ToolCall, Assistant
        assert!(matches!(t.entries[0], ThreadEntry::User(_)));
        assert!(matches!(t.entries[1], ThreadEntry::Assistant(_)));
        assert!(matches!(t.entries[2], ThreadEntry::ToolCall(_)));
        assert!(matches!(t.entries[3], ThreadEntry::Assistant(_)));
    }
}
