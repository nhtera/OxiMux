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

use super::entry::ChatImage;
use super::question::QuestionRequest;

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
    /// The structured result recorded alongside `result` — `toolUseResult` in
    /// the session file (camelCase), `tool_use_result` on the live wire
    /// (snake_case). Carries what the flattened string loses: Bash `{stdout,
    /// stderr, interrupted}`, an `Agent` subagent `{agentType, status,
    /// totalTokens, …}`, a Read's `numLines`, etc. `None` when absent.
    /// `#[serde(default)]` keeps older persisted transcript blobs loadable.
    #[serde(default)]
    pub structured: Option<Value>,
    /// Inline images the tool returned (a `Read` of an image, a screenshot),
    /// decoded from the result's `image` content blocks and rendered as
    /// thumbnails in the card body. Empty for the common text-only result.
    /// `#[serde(default)]` keeps older persisted transcript blobs loadable.
    #[serde(default)]
    pub images: Vec<ChatImage>,
    /// The ACP embedded-terminal id when this tool call hosts a live terminal
    /// (`ToolCallContent::Terminal`); the app mounts an inline `TerminalView`
    /// bound to it inside the card. `None` for the common non-terminal tool (all
    /// Claude/Codex calls). `#[serde(default)]` keeps older persisted blobs
    /// loadable; the live PTY is not itself persisted, so a restored transcript
    /// shows the tool card without a re-attached terminal.
    #[serde(default)]
    pub terminal_id: Option<String>,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            status: ToolCallStatus::InProgress,
            result: None,
            structured: None,
            images: Vec::new(),
            terminal_id: None,
        }
    }
}

/// Flatten a `tool_result.content` (a plain string, or an array of content
/// blocks) into a display string, shared by the live decoder and the session
/// importer. Text blocks pass through; an `image` block becomes an `[image]`
/// placeholder (the base64 payload isn't rendered inline in the transcript);
/// a `tool_reference` becomes its `tool_name`. Without this an image-returning
/// tool (a screenshot) or a tool-search result would flatten to a blank output.
pub fn flatten_tool_result_content(c: Option<&Value>) -> String {
    match c {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts: Vec<String> = Vec::new();
            let mut images = 0usize;
            let mut refs: Vec<String> = Vec::new();
            for b in arr {
                match b.get("type").and_then(Value::as_str) {
                    Some("image") => images += 1,
                    Some("tool_reference") => {
                        if let Some(n) = b.get("tool_name").and_then(Value::as_str) {
                            refs.push(n.to_string());
                        }
                    }
                    // Text blocks — and, defensively, any other block that still
                    // carries a `text` field.
                    _ => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                }
            }
            if images > 0 {
                parts.push(if images == 1 {
                    "[image]".to_string()
                } else {
                    format!("[{images} images]")
                });
            }
            if !refs.is_empty() {
                parts.push(format!("Tools: {}", refs.join(", ")));
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Extract inline base64 `image` blocks from a `tool_result.content` array (a
/// `Read` of an image file, a screenshot tool) as [`ChatImage`]s for inline
/// rendering — the actual pixels the flattened `[image]` placeholder stands in
/// for. Only base64 `source`s are inlined; a plain-string content, a non-image
/// array, or a URL-sourced image yields an empty vec. Shape verified against the
/// live CLI (`Read` of a PNG → `content:[{type:"image",source:{type:"base64",
/// media_type,data}}]`).
pub fn extract_tool_result_images(c: Option<&Value>) -> Vec<ChatImage> {
    let Some(Value::Array(arr)) = c else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|b| {
            if b.get("type").and_then(Value::as_str) != Some("image") {
                return None;
            }
            let src = b.get("source")?;
            if src.get("type").and_then(Value::as_str) != Some("base64") {
                return None;
            }
            Some(ChatImage {
                media_type: src.get("media_type").and_then(Value::as_str)?.to_string(),
                data: src.get("data").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect()
}

/// Lifecycle of a tool call. `WaitingForConfirmation` carries the pending
/// permission request so the UI can render Allow/Reject; the decision is
/// routed back to the agent by `request_id` (not an in-process channel),
/// which suits the subprocess/stream-json transport.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Pending,
    WaitingForConfirmation(PermissionRequest),
    /// An `AskUserQuestion` awaiting the user's selections (rendered as the
    /// interactive question card rather than an Allow/Reject card).
    AwaitingAnswer(QuestionRequest),
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
