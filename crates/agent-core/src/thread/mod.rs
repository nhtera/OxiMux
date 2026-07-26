//! Portable agent-chat fold + wire vocabulary.
//!
//! This is the pure, gpui-free, dependency-minimal core extracted from
//! `oximux-agents`: the `ThreadEvent` event model, the `stream-json` decoder,
//! and the `ChatThread` state machine that folds events into a message history.
//! It carries no `oximux-pty` / `rusqlite` / `agent-client-protocol` / `gpui`
//! deps, so the same fold that renders the desktop transcript compiles for
//! `aarch64-apple-ios` / `aarch64-linux-android` and is reused by the phone's
//! Rust core.
//!
//! The event model is deliberately ACP-shaped so any backend (Claude
//! stream-json, Codex, ACP) can feed the same `ChatThread` without changing the
//! state machine or the renderer.
//!
//! Files keep their original `crate::thread::*` module path so `oximux-agents`
//! can re-export them verbatim (`pub use oximux_agent_core::thread::*`) with zero
//! churn to the ~46 downstream import sites.

pub mod background_task;
pub mod context_chip;
pub mod entry;
pub mod event;
pub mod mcp_server_spec;
pub mod question;
pub mod state;
pub mod stream_json;
pub mod tool_call;
pub mod tool_detail;
pub mod turn_diff;

pub use background_task::{BackgroundTask, BackgroundTaskKind, TaskStatus};
pub use context_chip::{prepend_context, ContextChip, ContextKind};
pub use entry::{AssistantMessage, ChatImage, CheckpointState, ThreadEntry};
pub use event::{
    AuthMethodInfo, AuthMethodKind, McpServerStatus, PlanEntryLite, SessionMeta, ThreadEvent,
    TurnUsage,
};
pub use mcp_server_spec::{to_claude_mcp_config, McpServerSpec};
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
pub use turn_diff::TurnFileChange;
