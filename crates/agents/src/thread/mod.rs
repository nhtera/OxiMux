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

pub mod acp;
pub mod background_task;
pub mod claude_stream_json;
pub mod codex;
pub mod connect;
pub mod connection;
pub mod context_chip;
pub mod entry;
pub mod event;
pub mod question;
pub mod session_file_fork;
pub mod session_import;
pub mod state;
pub mod stream_json;
pub mod tool_call;
pub mod transport;

pub use acp::AcpConnection;
pub use claude_stream_json::{build_args, ClaudeStreamJsonConnection};
pub use codex::CodexAppServerConnection;
pub use connect::{connect, ChatBackend, ConnectSpec};
pub use connection::{
    control_response_json, question_answer_json, user_message_json, user_message_json_with_images,
    AgentCapabilities, AgentConnection, EffortChoice, ModeChoice, ModelChoice, StubConnection,
};
pub use transport::Transport;
pub use background_task::{BackgroundTask, BackgroundTaskKind, TaskStatus};
pub use context_chip::{prepend_context, ContextChip, ContextKind};
pub use entry::{AssistantMessage, ChatImage, CheckpointState, ThreadEntry};
pub use session_import::{transcript_from_jsonl, transcript_from_str, MAX_IMPORT_ENTRIES};
pub use event::{PlanEntryLite, ThreadEvent, TurnUsage};
pub use question::{
    parse_questions, updated_input_json, AskQuestion, QuestionAnswer, QuestionAnswers,
    QuestionKind, QuestionOption, QuestionRequest,
};
pub use state::ChatThread;
pub use stream_json::decode_line;
pub use tool_call::{
    PermissionDecision, PermissionRequest, PermissionSuggestion, ToolCall, ToolCallStatus,
};
