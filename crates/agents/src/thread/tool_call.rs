//! Tool-call model for the Agent Chat UI thread.
//!
//! Mirrors the ACP `ToolCall`/`ToolCallStatus` shape (so a future ACP backend
//! slots in without churn) but stays gpui-free: this crate is pure domain
//! logic, the GPUI entity wrapper lives in the app crate.
//!
//! A stream-json tool call is constructed directly as `InProgress` (the model
//! already emitted the `tool_use` block, so it is being attempted); if approval
//! is required it moves to `WaitingForConfirmation`, then to `Completed`/
//! `Failed` on the tool result, or `Rejected` on deny. `Pending` is reserved
//! for ACP parity (an announced-but-not-yet-started call) and is not
//! constructed by the stream-json backend.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single tool invocation inside an assistant turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The `tool_use_id` from the assistant `tool_use` block — the join key
    /// used to correlate the later `tool_result` and any permission request.
    pub id: String,
    /// Tool name, e.g. `Edit`, `Bash`, `Read`.
    pub name: String,
    /// Raw tool input as the model produced it (e.g. `{file_path, old_string,
    /// new_string}` for `Edit`). Rendered behind a "raw input" disclosure and
    /// used by later slices to synthesize a diff.
    pub input: Value,
    pub status: ToolCallStatus,
    /// Textual tool result once it completes (Read body, Bash output, …).
    pub result: Option<String>,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            status: ToolCallStatus::InProgress,
            result: None,
        }
    }
}

/// Lifecycle of a tool call. `WaitingForConfirmation` carries the pending
/// permission request so the UI can render Allow/Reject; the decision is
/// routed back to the agent by `request_id` (not an in-process channel),
/// which suits the subprocess/stream-json transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Pending,
    WaitingForConfirmation(PermissionRequest),
    InProgress,
    Completed,
    Failed(String),
    Rejected,
    Canceled,
}

/// A permission prompt surfaced by the agent (`can_use_tool` control request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Correlation id echoed back in the `control_response`.
    pub request_id: String,
    /// Short human label, e.g. the file name (`description` field).
    pub description: String,
    /// Agent-offered shortcuts (e.g. "always allow edits this session",
    /// "always allow this bash pattern"), rendered as extra buttons.
    pub suggestions: Vec<PermissionSuggestion>,
}

/// A single agent-offered permission shortcut. Kept as loosely-typed as the
/// wire so unknown suggestion kinds degrade gracefully instead of failing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionSuggestion {
    /// `setMode` (e.g. mode=acceptEdits) or `addRules` (allow a pattern).
    pub kind: String,
    /// Human label for the button.
    pub label: String,
    /// Opaque payload echoed back with the allow decision when chosen.
    pub raw: Value,
}

/// The user's answer to a `PermissionRequest`, sent back to the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    /// Allow this one call. `updated_input` echoes (optionally edits) the
    /// tool input — REQUIRED by the CLI, an allow without it is malformed.
    Allow { updated_input: Value },
    /// Allow and apply a suggestion (e.g. acceptEdits for the session).
    AllowWithSuggestion { updated_input: Value, suggestion: PermissionSuggestion },
    /// Deny this call; `message` is shown to the model.
    Deny { message: String },
}
