//! Agent Chat UI thread model.
//!
//! The structured conversation layer that backs the chat view (as opposed to
//! the raw-PTY terminal view). Slice 2A is the pure, gpui-free, fully-tested
//! core: the event vocabulary, the Claude `stream-json` decoder, and the
//! `ChatThread` state machine that folds events into a message history.
//!
//! Later slices add the subprocess connection (spawn `claude` in stream-json
//! mode, wire stdout→decoder and stdin←user-messages/permission-responses) and
//! the app-crate GPUI entity that renders a `ChatThread`.
//!
//! The event model is deliberately ACP-shaped so a future ACP backend can feed
//! the same `ChatThread` without changing the state machine or the view.

pub mod claude_stream_json;
pub mod connection;
pub mod entry;
pub mod event;
pub mod state;
pub mod stream_json;
pub mod tool_call;

pub use claude_stream_json::{build_args, ClaudeStreamJsonConnection};
pub use connection::{
    control_response_json, user_message_json, user_message_json_with_images, AgentCapabilities,
    AgentConnection, StubConnection,
};
pub use entry::{AssistantMessage, ChatImage, ThreadEntry};
pub use event::{ThreadEvent, TurnUsage};
pub use state::ChatThread;
pub use stream_json::decode_line;
pub use tool_call::{
    PermissionDecision, PermissionRequest, PermissionSuggestion, ToolCall, ToolCallStatus,
};
