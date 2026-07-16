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
    /// The ACP `ToolKind` (snake_case wire string: `execute`, `read`, `edit`,
    /// `search`, `fetch`, …) when the backend classified this call. Only ACP
    /// populates it — Claude/Codex classify by `name` and leave it `None`. Feeds
    /// the provider-agnostic tool-detail classifier so an ACP tool whose
    /// freeform `name` (a human title) can't be pattern-matched still routes to
    /// the right rich body. `#[serde(default)]` keeps older persisted blobs
    /// loadable.
    #[serde(default)]
    pub kind: Option<String>,
    /// This call asked a question the backend flagged `isSecret`, so whatever it
    /// reports back echoes a credential the user typed. Set when the answer is
    /// sent (the only moment the secret-ness is known — the awaiting status that
    /// carries it is replaced at that instant), and read by the fold to redact
    /// the result before it can reach the persisted transcript. `#[serde(default)]`
    /// keeps older persisted blobs loadable; it persists so a restored session
    /// keeps redacting if the backend re-reports the call.
    #[serde(default)]
    pub redact_result: bool,
    /// Transient accumulator for a tool call whose arguments are still streaming
    /// in as `input_json_delta` fragments (Claude live tool-input). Holds the raw
    /// partial-JSON string; the renderer best-effort parses it to preview the
    /// growing args until the finalized `tool_use` block sets the authoritative
    /// `input` and clears this. `#[serde(skip)]` — a persisted transcript only
    /// ever holds fully-formed tool calls, so this is never written or read.
    #[serde(skip)]
    pub partial_input: String,
    /// Rolling action log of a subagent/child thread this call spawned (Claude
    /// Task, Codex collab-subagent): one line per completed child action (a tool
    /// call or a first-line text summary). Rendered under the card body so the
    /// user sees "what the subagent is doing" without child bubbles leaking into
    /// the root transcript. Capped ({@link MAX_SUBAGENT_LOG}) — the oldest lines
    /// are evicted with a "+N earlier" marker synthesized at render time.
    /// `#[serde(default)]` keeps older persisted transcript blobs loadable, and it
    /// persists so a restored session shows the settled log.
    #[serde(default)]
    pub subagent_log: Vec<String>,
}

/// Cap on a tool call's retained [`ToolCall::subagent_log`] lines. A chatty
/// subagent (dozens of tool calls) must not grow the card — or a persisted blob
/// — without bound; past the cap the oldest lines are evicted.
pub const MAX_SUBAGENT_LOG: usize = 100;

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
            redact_result: false,
            terminal_id: None,
            kind: None,
            partial_input: String::new(),
            subagent_log: Vec::new(),
        }
    }

    /// Append a subagent action line, evicting the oldest when the log is at
    /// [`MAX_SUBAGENT_LOG`] (a bounded ring). Empty lines are ignored.
    pub fn push_subagent_action(&mut self, line: impl Into<String>) {
        let line = line.into();
        if line.is_empty() {
            return;
        }
        if self.subagent_log.len() >= MAX_SUBAGENT_LOG {
            self.subagent_log.remove(0);
        }
        self.subagent_log.push(line);
    }

    /// The best-effort preview of a still-streaming tool call's arguments: the
    /// accumulated partial JSON parsed leniently, or `None` when nothing usable
    /// has arrived yet. Used by the card renderer to show growing args before the
    /// finalized `tool_use` block. Once `input` is populated (finalized), callers
    /// should prefer it — this only matters while `partial_input` is non-empty.
    pub fn preview_input(&self) -> Option<Value> {
        if self.partial_input.is_empty() {
            return None;
        }
        parse_partial_json(&self.partial_input)
    }
}

/// Cap on the accumulated partial-JSON buffer before we stop recomputing the
/// preview: a multi-megabyte Write shouldn't re-parse growing JSON on every
/// fragment. Past this the card shows a "streaming…" affordance and the
/// authoritative input still lands at finalize.
pub const MAX_PARTIAL_INPUT_BYTES: usize = 64 * 1024;

/// Best-effort parse of a truncated JSON fragment (a prefix of a valid JSON
/// value, as Claude streams tool args) into a `Value`. Closes any unterminated
/// string / array / object so the partial object renders live. Returns `None`
/// when the fragment is empty or too mangled to close (e.g. mid-escape). Pure —
/// unit-tested against truncated objects, strings, escapes, and nested arrays.
pub fn parse_partial_json(fragment: &str) -> Option<Value> {
    let trimmed = fragment.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Fast path: already-complete JSON.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    // Repair: walk the fragment tracking string/escape state and the open
    // bracket stack, then append the closers needed to make it parseable. A
    // fragment ending mid-escape (`\`) drops the trailing backslash so the
    // closing quote isn't escaped away.
    let mut repaired = String::with_capacity(trimmed.len() + 8);
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut last_was_backslash_in_string = false;
    for c in trimmed.chars() {
        if in_string {
            if escaped {
                escaped = false;
                last_was_backslash_in_string = false;
            } else if c == '\\' {
                escaped = true;
                last_was_backslash_in_string = true;
            } else if c == '"' {
                in_string = false;
                last_was_backslash_in_string = false;
            }
        } else {
            match c {
                '"' => in_string = true,
                '{' => stack.push('}'),
                '[' => stack.push(']'),
                '}' | ']' => {
                    stack.pop();
                }
                _ => {}
            }
        }
        repaired.push(c);
    }
    // Drop a dangling backslash that would escape our closing quote.
    if in_string && last_was_backslash_in_string {
        repaired.pop();
    }
    // A trailing key with no value (`{"a":`) or a trailing comma can't be closed
    // meaningfully — strip a dangling `:`/`,` and any incomplete `"key"` tail.
    if in_string {
        repaired.push('"');
    }
    let tail = repaired.trim_end();
    let mut repaired = tail.to_string();
    while repaired.ends_with([':', ',']) {
        repaired.pop();
        repaired = repaired.trim_end().to_string();
    }
    for closer in stack.iter().rev() {
        repaired.push(*closer);
    }
    serde_json::from_str::<Value>(&repaired).ok()
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

/// What a permission request is asking for — lets the UI route a request to a
/// dedicated card (e.g. a plan-approval card for `Plan`) instead of the generic
/// key:value permission card. `Tool` is the common case (a tool wants to run);
/// the rest are special channels that ride the same `can_use_tool` /
/// server-request transport. Defaults to `Tool` so old persisted transcripts
/// (which had no `kind`) and every unchanged decoder path stay correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    /// A tool wants to execute (the overwhelming majority).
    #[default]
    Tool,
    /// Claude's `ExitPlanMode`: the request body is a plan the user approves or
    /// rejects, rendered as a markdown plan-approval card.
    Plan,
    /// A permission/edit-mode change request.
    Mode,
    /// An MCP server is eliciting input/consent mid-tool (Codex
    /// `mcpServer/elicitation/request`), labeled "MCP · <server>".
    Mcp,
    /// Anything else that should surface as a generic consent card.
    Other,
}

/// A permission prompt surfaced by the agent (`can_use_tool` control request).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Correlation id echoed back in the `control_response`.
    pub request_id: String,
    /// What is being requested — drives which card renders it. `#[serde(default)]`
    /// so pre-taxonomy persisted transcripts load as `Tool`.
    #[serde(default)]
    pub kind: PermissionKind,
    /// Short human label, e.g. the file name (`description` field). For a `Plan`
    /// request this carries the plan markdown itself.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_partial_json_complete_value_passes_through() {
        assert_eq!(parse_partial_json(r#"{"a":1}"#), Some(json!({"a":1})));
    }

    #[test]
    fn push_subagent_action_caps_and_evicts_oldest() {
        let mut tc = ToolCall::new("t", "Agent", json!({}));
        tc.push_subagent_action(""); // empty ignored
        assert!(tc.subagent_log.is_empty());
        for i in 0..(MAX_SUBAGENT_LOG + 5) {
            tc.push_subagent_action(format!("line {i}"));
        }
        assert_eq!(tc.subagent_log.len(), MAX_SUBAGENT_LOG, "log is a bounded ring");
        // Oldest five evicted; newest retained.
        assert_eq!(tc.subagent_log.first().unwrap(), "line 5");
        assert_eq!(tc.subagent_log.last().unwrap(), &format!("line {}", MAX_SUBAGENT_LOG + 4));
    }

    #[test]
    fn subagent_log_survives_serde_round_trip_and_defaults_on_old_blobs() {
        let mut tc = ToolCall::new("t", "Agent", json!({}));
        tc.push_subagent_action("Read src/main.rs");
        let blob = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.subagent_log, vec!["Read src/main.rs".to_string()]);
        // An older persisted blob (no `subagent_log` key) loads with an empty log.
        let old = json!({"id":"t","name":"Agent","input":{},"status":"InProgress","result":null}).to_string();
        let loaded: ToolCall = serde_json::from_str(&old).unwrap();
        assert!(loaded.subagent_log.is_empty(), "serde default keeps old blobs loadable");
    }

    #[test]
    fn parse_partial_json_truncated_object_and_open_string() {
        // Mid-value fragments Claude actually streams for a Write.
        assert_eq!(
            parse_partial_json(r#"{"file_path": "/tmp/poem.txt"#),
            Some(json!({"file_path": "/tmp/poem.txt"}))
        );
        assert_eq!(
            parse_partial_json(r#"{"file_path": "/tmp/p.txt", "content": "Beneath the moon"#),
            Some(json!({"file_path": "/tmp/p.txt", "content": "Beneath the moon"}))
        );
    }

    #[test]
    fn parse_partial_json_escapes_and_newlines_in_open_string() {
        // A fragment ending inside a string that contains escaped quotes/newlines.
        let v = parse_partial_json(r#"{"content": "line one\nline \"two\""#).unwrap();
        assert_eq!(v["content"], "line one\nline \"two\"");
    }

    #[test]
    fn parse_partial_json_dangling_backslash_is_dropped() {
        // Ending mid-escape must not escape our synthesized closing quote.
        let v = parse_partial_json(r#"{"content": "abc\"#).unwrap();
        assert_eq!(v["content"], "abc");
    }

    #[test]
    fn parse_partial_json_nested_arrays_and_objects() {
        assert_eq!(
            parse_partial_json(r#"{"items": [1, 2, {"k": "v"#),
            Some(json!({"items": [1, 2, {"k": "v"}]}))
        );
    }

    #[test]
    fn parse_partial_json_empty_or_hopeless_returns_none() {
        assert_eq!(parse_partial_json(""), None);
        assert_eq!(parse_partial_json("   "), None);
        // A trailing key with no value can't be closed meaningfully.
        assert_eq!(parse_partial_json(r#"{"a":"#), None);
        // A partial literal (`tr` for `true`) is unparseable.
        assert_eq!(parse_partial_json(r#"{"a": tr"#), None);
    }

    #[test]
    fn parse_partial_json_growing_object_stays_parseable_each_step() {
        // Emulate the accumulating buffer never panicking / never rendering raw.
        let full = r#"{"file_path": "/tmp/x", "content": "hello world"}"#;
        for i in 1..=full.len() {
            let _ = parse_partial_json(&full[..i]); // must never panic
        }
        assert_eq!(parse_partial_json(full), Some(json!({"file_path":"/tmp/x","content":"hello world"})));
    }

    #[test]
    fn preview_input_reflects_accumulated_fragments() {
        let mut tc = ToolCall::new("t1", "Write", Value::Null);
        assert_eq!(tc.preview_input(), None, "nothing streamed yet");
        tc.partial_input.push_str(r#"{"file_path": "/tmp/x", "content": "abc"#);
        assert_eq!(
            tc.preview_input(),
            Some(json!({"file_path": "/tmp/x", "content": "abc"}))
        );
    }
}
