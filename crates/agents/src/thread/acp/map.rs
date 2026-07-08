//! Maps ACP `SessionUpdate` notifications → the transport-agnostic
//! [`ThreadEvent`] vocabulary the `ChatThread`/UI consume.
//!
//! Phase 1 covers the **text round-trip** only: the agent's message and thought
//! chunks stream as deltas (and accumulate into the shared buffers so the worker
//! can emit a finalized block at turn end). Tool calls, plans, slash commands,
//! usage, and mode changes are later phases — unhandled variants are ignored so
//! an unexpected update never breaks the stream (`SessionUpdate` is
//! `#[non_exhaustive]`).

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};

use super::AcpState;
use crate::thread::event::ThreadEvent;

/// Flatten one `SessionUpdate` into zero or more `ThreadEvent`s, accumulating the
/// streamed text into `state` so the worker can finalize the turn's blocks.
pub(crate) fn map_session_update(update: SessionUpdate, state: &mut AcpState) -> Vec<ThreadEvent> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => {
                state.text_buf.push_str(&t);
                vec![ThreadEvent::AssistantTextDelta(t)]
            }
            None => Vec::new(),
        },
        SessionUpdate::AgentThoughtChunk(chunk) => match text_of(&chunk.content) {
            Some(t) => {
                state.thinking_buf.push_str(&t);
                vec![ThreadEvent::ThinkingDelta(t)]
            }
            None => Vec::new(),
        },
        // Later phases: ToolCall / ToolCallUpdate (tool cards), Plan (plan panel),
        // AvailableCommandsUpdate (slash palette), UsageUpdate (footer),
        // CurrentModeUpdate (mode picker). User-message echoes are the client's
        // own prompt — never re-rendered.
        _ => Vec::new(),
    }
}

/// The plain text of a `ContentBlock`, when it carries any.
fn text_of(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentChunk, TextContent};

    fn text_chunk(s: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(s.to_string())))
    }

    #[test]
    fn agent_message_chunk_streams_and_accumulates() {
        let mut st = AcpState::default();
        let evs = map_session_update(SessionUpdate::AgentMessageChunk(text_chunk("Hel")), &mut st);
        assert_eq!(evs, vec![ThreadEvent::AssistantTextDelta("Hel".into())]);
        map_session_update(SessionUpdate::AgentMessageChunk(text_chunk("lo")), &mut st);
        assert_eq!(st.text_buf, "Hello");
    }

    #[test]
    fn agent_thought_chunk_maps_to_thinking() {
        let mut st = AcpState::default();
        let evs = map_session_update(SessionUpdate::AgentThoughtChunk(text_chunk("hm")), &mut st);
        assert_eq!(evs, vec![ThreadEvent::ThinkingDelta("hm".into())]);
        assert_eq!(st.thinking_buf, "hm");
    }
}
