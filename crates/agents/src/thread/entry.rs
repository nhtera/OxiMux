//! Transcript entries for the Agent Chat UI thread.
//!
//! A chat thread is an ordered `Vec<ThreadEntry>`. This is the message-history
//! model OxiMux lacks today (the OSC-9999 sideband keeps only single-slot
//! `Option<String>` values, overwritten per turn). Pure data — no gpui.

use serde::{Deserialize, Serialize};

use super::tool_call::ToolCall;

/// One rendered item in the conversation, in arrival order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadEntry {
    /// A user prompt.
    User(String),
    /// An assistant message: visible text plus any thinking block.
    Assistant(AssistantMessage),
    /// A tool invocation (with its own streaming status + result).
    ToolCall(ToolCall),
}

/// An assistant message. `thinking` is the (optional) extended-thinking block
/// shown collapsed; `text` is the user-visible reply. Both accumulate across
/// streaming deltas within one turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    pub thinking: String,
}

impl AssistantMessage {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.thinking.is_empty()
    }
}
