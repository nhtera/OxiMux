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
pub mod claude_stream_json;
pub mod codex;
pub mod codex_session_import;
pub mod connect;
pub mod connection;
pub mod pi;
pub mod session_file_fork;
pub mod session_import;
pub mod transport;

// The pure fold + wire vocabulary + stream-json decoder now live in the
// dependency-minimal, mobile-portable `oximux-agent-core` crate. Re-export the
// modules under their original `crate::thread::*` paths so every downstream
// import site (and this file's own type re-exports below) resolves unchanged.
pub use oximux_agent_core::thread::{
    background_task, context_chip, entry, event, question, state, stream_json, tool_call,
    tool_detail, turn_diff,
};

pub use acp::AcpConnection;
pub use claude_stream_json::{build_args, claude_model_choices, ClaudeStreamJsonConnection};
pub use codex::CodexAppServerConnection;
pub use pi::PiRpcConnection;
pub use connect::{connect, probe_catalog, ChatBackend, ConnectSpec, ProbedCatalog};
pub use connection::{
    control_response_json, question_answer_json, user_message_json, user_message_json_with_images,
    AgentCapabilities, AgentConnection, EffortChoice, FeatureControl, FeatureKind,
    FeatureSelectOption, FeatureValue, ModeChoice, ModelChoice, StubConnection,
};
pub use transport::Transport;
pub use background_task::{BackgroundTask, BackgroundTaskKind, TaskStatus};
pub use context_chip::{prepend_context, ContextChip, ContextKind};
pub use entry::{AssistantMessage, ChatImage, CheckpointState, ThreadEntry};
pub use turn_diff::TurnFileChange;
pub use session_import::{transcript_from_jsonl, transcript_from_str, MAX_IMPORT_ENTRIES};
pub use codex_session_import::{
    import_codex_rollout, locate_rollout, transcript_from_codex_str, CodexRolloutImport,
};
pub use event::{
    AuthMethodInfo, AuthMethodKind, McpServerStatus, PlanEntryLite, SessionMeta, ThreadEvent,
    TurnUsage,
};
pub use question::{
    parse_questions, updated_input_json, AskQuestion, QuestionAnswer, QuestionAnswers,
    QuestionKind, QuestionOption, QuestionRequest,
};
pub use state::ChatThread;
pub use stream_json::decode_line;
pub use tool_call::{
    PermissionDecision, PermissionKind, PermissionRequest, PermissionSuggestion, ToolCall,
    ToolCallStatus,
};
pub use tool_detail::ToolDetail;
