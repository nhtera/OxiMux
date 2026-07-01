//! `ChatThread` — the in-memory conversation state machine.
//!
//! Folds a stream of `ThreadEvent`s into an ordered `Vec<ThreadEntry>` (the
//! message history OxiMux lacks today). Pure and gpui-free: the app crate
//! owns a GPUI entity that holds a `ChatThread`, calls `apply` on each event,
//! and repaints.
//!
//! Reconciliation: streaming deltas build the current assistant block; the
//! FIRST finalized block of that kind replaces the streamed text (so it isn't
//! doubled), and any FURTHER finalized block of the same kind in the same
//! message is appended (a message may legitimately carry more than one text
//! block). A tool call ends the current assistant block, so later text starts
//! a fresh assistant message.

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
    /// True once a text delta has streamed into the current assistant block
    /// but its finalized text hasn't reconciled it yet.
    text_streaming: bool,
    /// Same, for the thinking block.
    thinking_streaming: bool,
}

impl ChatThread {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild a thread from a persisted transcript on session restore. Seeds
    /// `entries` + the `session_id`/`model` needed to `--resume`, and leaves the
    /// streaming/turn flags at rest (no turn is in flight after a relaunch).
    ///
    /// **Fail-closed:** any tool call left `WaitingForConfirmation` when the app
    /// last quit is downgraded to `Rejected`. The process that asked is dead, so
    /// the request can never be answered — restoring it as pending would strand a
    /// permanently-unanswerable prompt in the UI (and, worse, imply the edit is
    /// still gated). Denying is the safe default, matching the live-disconnect
    /// fail-closed rule in the view.
    pub fn rehydrated(
        session_id: Option<String>,
        model: Option<String>,
        mut entries: Vec<ThreadEntry>,
    ) -> Self {
        for entry in &mut entries {
            if let ThreadEntry::ToolCall(tc) = entry
                && matches!(tc.status, ToolCallStatus::WaitingForConfirmation(_))
            {
                tc.status = ToolCallStatus::Rejected;
            }
        }
        Self {
            entries,
            session_id,
            model,
            ..Self::default()
        }
    }

    /// Record a user prompt (called when we send one to the agent).
    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.entries.push(ThreadEntry::User(text.into()));
        self.end_assistant_window();
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
            ThreadEvent::AssistantTextDelta(t) => {
                self.assistant_mut().text.push_str(t);
                self.text_streaming = true;
            }
            ThreadEvent::ThinkingDelta(t) => {
                self.assistant_mut().thinking.push_str(t);
                self.thinking_streaming = true;
            }
            // First finalized block reconciles (replaces) the streamed value;
            // a further finalized block of the same kind appends.
            ThreadEvent::AssistantText(t) => {
                if self.text_streaming {
                    self.assistant_mut().text = t.clone();
                    self.text_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.text.is_empty() {
                        m.text.push('\n');
                    }
                    m.text.push_str(t);
                }
            }
            ThreadEvent::AssistantThinking(t) => {
                if self.thinking_streaming {
                    self.assistant_mut().thinking = t.clone();
                    self.thinking_streaming = false;
                } else {
                    let m = self.assistant_mut();
                    if !m.thinking.is_empty() {
                        m.thinking.push('\n');
                    }
                    m.thinking.push_str(t);
                }
            }
            ThreadEvent::ToolCallStarted { id, name, input } => {
                self.entries.push(ThreadEntry::ToolCall(ToolCall::new(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                )));
                // A tool call ends the current assistant block; later text
                // starts a fresh assistant message.
                self.end_assistant_window();
            }
            ThreadEvent::ToolResult { tool_use_id, content, is_error } => {
                // A result for an unknown id is silently dropped (no matching
                // tool call to attach it to) rather than mutating the wrong
                // entry — see the `tool_result_for_unknown_id_is_ignored` test.
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
                        // (unexpected ordering, or a null tool_use_id) —
                        // synthesize the card anyway so the user can still act.
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
                self.end_assistant_window();
            }
            ThreadEvent::TurnEnded { is_error, result, .. } => {
                self.turn_active = false;
                self.end_assistant_window();
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

    /// Close the current assistant message: subsequent text/thinking starts a
    /// fresh entry, and the streamed-vs-finalized reconciliation flags reset.
    fn end_assistant_window(&mut self) {
        self.current_assistant = None;
        self.text_streaming = false;
        self.thinking_streaming = false;
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
    fn multiple_finalized_text_blocks_append_not_overwrite() {
        // A single assistant message carrying two top-level text blocks (no
        // tool_use between) must keep both — not silently drop the first.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantText("First.".into()));
        t.apply(&ThreadEvent::AssistantText("Second.".into()));
        let joined = t.entries.iter().filter_map(|e| match e {
            ThreadEntry::Assistant(m) => Some(m.text.clone()), _ => None }).collect::<Vec<_>>();
        assert_eq!(joined, vec!["First.\nSecond.".to_string()]);
    }

    #[test]
    fn streamed_then_multi_finalized_reconciles_then_appends() {
        // Deltas stream the first block; the first finalized replaces the
        // stream; a second finalized block appends.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::AssistantTextDelta("Fir".into()));
        t.apply(&ThreadEvent::AssistantTextDelta("st.".into()));
        t.apply(&ThreadEvent::AssistantText("First.".into())); // reconcile
        t.apply(&ThreadEvent::AssistantText("Second.".into())); // append
        assert_eq!(assistant_text(&t.entries[1]), "First.\nSecond.");
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
    fn tool_result_for_unknown_id_is_ignored() {
        // A result whose id matches no tool call must not panic or corrupt the
        // existing tool call — it is simply dropped.
        let mut t = ChatThread::new();
        t.push_user_message("go");
        t.apply(&ThreadEvent::ToolCallStarted {
            id: "toolu_1".into(), name: "Read".into(), input: json!({}) });
        t.apply(&ThreadEvent::ToolResult {
            tool_use_id: "toolu_UNKNOWN".into(), content: "stray".into(), is_error: false });
        match &t.entries[1] {
            ThreadEntry::ToolCall(tc) => {
                assert_eq!(tc.status, ToolCallStatus::InProgress, "existing tool call untouched");
                assert!(tc.result.is_none());
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
    fn orphan_permission_synthesizes_a_tool_call() {
        // Permission arrives with no matching tool_use (null id, or the
        // tool_use event was lost) — the card is synthesized so the user can
        // still allow/deny, keyed on the request_id.
        let mut t = ChatThread::new();
        t.push_user_message("do it");
        t.apply(&ThreadEvent::PermissionRequested {
            request_id: "rid-orphan".into(), tool_use_id: None,
            tool_name: "Bash".into(), input: json!({"command":"ls"}),
            description: "ls".into(), suggestions: vec![] });

        let (tool_id, req) = t.pending_permission().expect("synthesized pending permission");
        assert_eq!(tool_id, "rid-orphan", "falls back to request_id as the tool id");
        assert_eq!(req.request_id, "rid-orphan");
        match t.entries.iter().find(|e| matches!(e, ThreadEntry::ToolCall(_))) {
            Some(ThreadEntry::ToolCall(tc)) => assert_eq!(tc.name, "Bash"),
            _ => panic!("a synthesized ToolCall entry should exist"),
        }
    }

    #[test]
    fn rehydrated_seeds_entries_and_session_and_rests_flags() {
        let entries = vec![
            ThreadEntry::User("hi".into()),
            ThreadEntry::Assistant(AssistantMessage { text: "hello".into(), thinking: String::new() }),
        ];
        let t = ChatThread::rehydrated(Some("sid-9".into()), Some("opus".into()), entries);
        assert_eq!(t.session_id.as_deref(), Some("sid-9"));
        assert_eq!(t.model.as_deref(), Some("opus"));
        assert_eq!(t.entries.len(), 2);
        assert!(!t.turn_active, "a restored thread has no turn in flight");
        assert!(t.pending_permission().is_none());
    }

    #[test]
    fn rehydrated_fail_closes_a_pending_permission() {
        // A tool call left WaitingForConfirmation at quit must restore as
        // Rejected — the process that asked is gone, so it can never be
        // answered; leaving it pending would strand an unanswerable prompt.
        let req = PermissionRequest {
            request_id: "rid".into(),
            description: "notes.txt".into(),
            suggestions: vec![],
        };
        let mut tc = ToolCall::new("toolu_1", "Edit", serde_json::json!({}));
        tc.status = ToolCallStatus::WaitingForConfirmation(req);
        let t = ChatThread::rehydrated(None, None, vec![ThreadEntry::ToolCall(tc)]);
        assert!(t.pending_permission().is_none(), "no pending permission survives restore");
        match &t.entries[0] {
            ThreadEntry::ToolCall(tc) => assert_eq!(tc.status, ToolCallStatus::Rejected),
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn transcript_survives_a_serde_round_trip() {
        // The persisted-blob path serializes the entry list and reloads it.
        // Prove a representative transcript (user + assistant + completed tool)
        // round-trips byte-for-byte through serde_json.
        let mut src = ChatThread::new();
        src.push_user_message("read it");
        src.apply(&ThreadEvent::AssistantText("On it.".into()));
        src.apply(&ThreadEvent::ToolCallStarted {
            id: "t1".into(), name: "Read".into(), input: json!({"file_path":"a"}) });
        src.apply(&ThreadEvent::ToolResult {
            tool_use_id: "t1".into(), content: "body".into(), is_error: false });

        let json = serde_json::to_string(&src.entries).expect("serialize entries");
        let back: Vec<ThreadEntry> = serde_json::from_str(&json).expect("deserialize entries");
        assert_eq!(back, src.entries);
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
