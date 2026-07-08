//! Maps ACP `SessionUpdate` notifications → the transport-agnostic
//! [`ThreadEvent`] vocabulary the `ChatThread`/UI consume.
//!
//! Covered: assistant message + thought chunks (streamed as deltas — the ACP
//! chunks are authoritative, so the `ChatThread` builds the message from them and
//! seals it on `TurnEnded`; no separate finalized block is emitted, which would
//! clobber text interleaved with tool cards), and tool calls (`ToolCall` → a tool
//! card; `ToolCallUpdate` at a terminal status → its result). ACP `ToolKind`/
//! `title` don't line up with Claude's tool names, so cards render via the
//! generic path rather than a Bash/Read/Edit-specific renderer — a deliberate
//! fallback, not a leak of raw JSON.
//!
//! Deferred (each needs plumbing the `ThreadEvent` vocabulary doesn't have yet):
//! `AvailableCommands` (slash palette), `CurrentMode` (mode picker), `Plan` (plan
//! panel). `UsageUpdate` is intentionally ignored (`emits_usage = false`).

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
};

use crate::thread::event::ThreadEvent;

/// Flatten one `SessionUpdate` into zero or more `ThreadEvent`s.
pub(crate) fn map_session_update(update: SessionUpdate) -> Vec<ThreadEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => vec![ThreadEvent::AssistantTextDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => vec![ThreadEvent::ThinkingDelta(t)],
            None => Vec::new(),
        },
        SessionUpdate::ToolCall(tc) => map_tool_call(tc),
        SessionUpdate::ToolCallUpdate(tcu) => map_tool_call_update(tcu),
        // Deferred / ignored (see module docs). `UserMessageChunk` is the client's
        // own prompt echoed back — never re-rendered.
        _ => Vec::new(),
    }
}

/// A `ToolCall` opens a tool card; when it already carries a terminal status it
/// also emits the result in the same batch (some agents send one shot).
fn map_tool_call(tc: ToolCall) -> Vec<ThreadEvent> {
    let id = tc.tool_call_id.0.to_string();
    let mut out = vec![ThreadEvent::ToolCallStarted {
        id: id.clone(),
        name: tc.title.clone(),
        input: tc.raw_input.clone().unwrap_or(serde_json::Value::Null),
    }];
    if is_terminal(&tc.status) {
        out.push(ThreadEvent::ToolResult {
            tool_use_id: id,
            content: content_text(&tc.content),
            is_error: matches!(tc.status, ToolCallStatus::Failed),
            structured: tc.raw_output,
        });
    }
    out
}

/// A `ToolCallUpdate` at a terminal status closes the card with its result;
/// intermediate updates (still running) carry no `ThreadEvent`.
fn map_tool_call_update(tcu: ToolCallUpdate) -> Vec<ThreadEvent> {
    match tcu.fields.status {
        Some(status) if is_terminal(&status) => vec![ThreadEvent::ToolResult {
            tool_use_id: tcu.tool_call_id.0.to_string(),
            content: tcu.fields.content.as_deref().map(content_text).unwrap_or_default(),
            is_error: matches!(status, ToolCallStatus::Failed),
            structured: tcu.fields.raw_output,
        }],
        _ => Vec::new(),
    }
}

/// `Completed`/`Failed` are terminal; `Pending`/`InProgress` are not.
fn is_terminal(status: &ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}

/// The plain text of a `ContentBlock`, when it carries any.
fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

/// Flatten a tool call's content items into a legible result body: text blocks
/// verbatim, file diffs as a minimal `path` + new-content note (rich diff cards
/// are a later refinement), terminal output skipped.
fn content_text(items: &[ToolCallContent]) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            ToolCallContent::Content(c) => {
                if let Some(t) = text_of(&c.content) {
                    out.push_str(&t);
                }
            }
            ToolCallContent::Diff(d) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("{}\n{}", d.path.display(), d.new_text));
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentChunk, TextContent, ToolCallUpdateFields,
    };

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s.to_string())))
    }

    #[test]
    fn agent_message_chunk_streams_as_delta() {
        let evs = map_session_update(SessionUpdate::AgentMessageChunk(text_chunk("Hello")));
        assert_eq!(evs, vec![ThreadEvent::AssistantTextDelta("Hello".into())]);
    }

    #[test]
    fn agent_thought_chunk_maps_to_thinking() {
        let evs = map_session_update(SessionUpdate::AgentThoughtChunk(text_chunk("hm")));
        assert_eq!(evs, vec![ThreadEvent::ThinkingDelta("hm".into())]);
    }

    #[test]
    fn tool_call_opens_a_card() {
        let tc = ToolCall::new("call-1", "Read src/main.rs")
            .raw_input(serde_json::json!({"path": "src/main.rs"}));
        let evs = map_session_update(SessionUpdate::ToolCall(tc));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ThreadEvent::ToolCallStarted { id, name, input } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "Read src/main.rs");
                assert_eq!(input["path"], "src/main.rs");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_completed_emits_result() {
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::from(ContentBlock::Text(TextContent::new(
                "file contents".to_string(),
            )))]);
        let tcu = ToolCallUpdate::new("call-1", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            ThreadEvent::ToolResult { tool_use_id, content, is_error, .. } => {
                assert_eq!(tool_use_id, "call-1");
                assert_eq!(content, "file contents");
                assert!(!is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_update_in_progress_is_silent() {
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let tcu = ToolCallUpdate::new("call-1", fields);
        assert!(map_session_update(SessionUpdate::ToolCallUpdate(tcu)).is_empty());
    }

    #[test]
    fn tool_call_update_failed_marks_error() {
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        let tcu = ToolCallUpdate::new("call-9", fields);
        let evs = map_session_update(SessionUpdate::ToolCallUpdate(tcu));
        match &evs[0] {
            ThreadEvent::ToolResult { is_error, .. } => assert!(is_error),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }
}
