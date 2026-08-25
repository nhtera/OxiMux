//! Wire types for `omp --mode rpc-ui` — one `\n`-terminated JSON value per
//! message, same envelope as Pi's (omp is a Pi fork) with three deltas this
//! module owns:
//!
//! - a **versioned handshake**: omp's first frame is `ready` declaring the
//!   frame-size contract; `negotiate_protocol{2}` must follow before large
//!   frames flow (probed live on 18.0.4 — `available_commands_update` alone is
//!   ~88KB and models ~114KB, near v1's 1MiB single-line cap);
//! - **inbound `rpc_chunk` reassembly**: after v2, omp→client frames >1MiB
//!   arrive as base64 chunk sequences. Outbound needs NOTHING: a 1.5MB
//!   single-line `prompt` was accepted raw, and client-side chunk frames are
//!   rejected ("Unknown command: rpc_chunk") — reassembly is one-directional
//!   by construction;
//! - **approvals**: rpc-ui delivers tool approvals as
//!   `extension_ui_request{method:"select", title:"Allow tool: …",
//!   options:["Approve","Deny"]}` frames, answered with
//!   `extension_ui_response{id, value}`. Deny verified to block the tool's
//!   effect on disk (probe 01).
//!
//! Commands mirror omp's renamed union (`get_messages`, not Pi's
//! `get_entries`; `get_available_commands`, not `get_commands`); only the
//! ones a phase drives are modelled. Events stay `serde_json::Value` — the
//! mapper owns the taxonomy, and permissiveness is what keeps an unknown
//! event non-fatal on omp's fast release cadence.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use super::super::pi::protocol::THINKING_LEVELS;

/// A command written to omp's stdin. `id` correlates the eventual response;
/// omp echoes it back verbatim. Types are snake_case, fields camelCase —
/// asserted against live-captured frames in this module's tests, because a
/// wrong field case is SILENT (omp ignores the unknown key).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum OmpCommand {
    /// Upgrade to chunked-frame protocol v2. Sent right after `ready`.
    NegotiateProtocol { id: String, protocol_version: u32 },
    /// Handshake + state read: model, thinking level, session id/file,
    /// context usage.
    GetState { id: String },
    /// Start a turn. `images` inline (base64 + mime), verified in omp's
    /// prompt schema.
    Prompt {
        id: String,
        message: String,
        #[serde(skip_serializing_if = "Vec::is_empty", default)]
        images: Vec<ImageContent>,
    },
    /// Interrupt the in-flight turn (in-band, like Pi).
    Abort { id: String },
    /// Redirect the live turn; drains at the next turn boundary.
    Steer { id: String, message: String },
    /// The full transcript of the (possibly resumed) session.
    GetMessages { id: String },
    GetAvailableModels { id: String },
    GetAvailableCommands { id: String },
    SetModel { id: String, provider: String, model_id: String },
    SetThinkingLevel { id: String, level: String },
    /// Depth of subagent event forwarding (`off`/`lifecycle`/`progress`/`full`
    /// per omp's union; OxiMux drives the minimal folding level).
    SetSubagentSubscription { id: String, level: String },
}

impl OmpCommand {
    /// The `id` this command will be answered on.
    pub fn id(&self) -> &str {
        match self {
            OmpCommand::NegotiateProtocol { id, .. }
            | OmpCommand::GetState { id }
            | OmpCommand::Prompt { id, .. }
            | OmpCommand::Abort { id }
            | OmpCommand::Steer { id, .. }
            | OmpCommand::GetMessages { id }
            | OmpCommand::GetAvailableModels { id }
            | OmpCommand::GetAvailableCommands { id }
            | OmpCommand::SetModel { id, .. }
            | OmpCommand::SetThinkingLevel { id, .. }
            | OmpCommand::SetSubagentSubscription { id, .. } => id,
        }
    }
}

/// One inline prompt image, omp's shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Always `"image"`.
    pub r#type: &'static str,
    /// Base64 payload.
    pub data: String,
    pub mime_type: String,
}

/// The answer to an `extension_ui_request`. NOT a command with a response —
/// `id` is the REQUEST's id, and omp answers nothing back — so it is its own
/// type, sent fire-and-forget.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename = "extension_ui_response")]
pub struct ExtensionUiResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

/// omp's startup frame — the first stdout line, declaring the frame-size
/// contract. Asserted at connect: a shape change here is a protocol bump this
/// adapter must re-probe, not silently tolerate.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFrame {
    pub protocol_version: u32,
    #[serde(default)]
    pub supported_protocol_versions: Vec<u32>,
    #[serde(default)]
    pub max_frame_bytes: u64,
    #[serde(default)]
    pub max_reassembled_frame_bytes: u64,
}

/// One inbound line, classified. Same taxonomy as Pi's, typed separately so
/// omp's error strings say "omp".
#[derive(Debug, Clone)]
pub enum Inbound {
    /// A correlated answer to a command.
    Response(RpcResponse),
    /// A dialog request FROM omp (tool approvals ride this in rpc-ui mode).
    ExtensionUiRequest(Value),
    /// A session event (`agent_start`, `message_update`, `tool_execution_*`,
    /// `available_commands_update`, `notice`, …). Untyped: the mapper owns it.
    Event(Value),
}

/// A correlated response. `success` discriminates: on `false`, `error`
/// carries omp's message.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcResponse {
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RpcResponse {
    /// The response's payload, or its error as an `Err`.
    pub fn into_data(self) -> anyhow::Result<Value> {
        if self.success {
            Ok(self.data.unwrap_or(Value::Null))
        } else {
            Err(anyhow::anyhow!(
                "omp {} failed: {}",
                self.command,
                self.error.unwrap_or_else(|| "unknown error".into())
            ))
        }
    }
}

/// Classify one decoded inbound line (post-reassembly).
pub fn classify(v: Value) -> Inbound {
    match v.get("type").and_then(Value::as_str) {
        Some("response") => match serde_json::from_value::<RpcResponse>(v.clone()) {
            Ok(r) => Inbound::Response(r),
            // A response we can't parse is more useful as an event than as a
            // dropped line — the mapper logs it rather than the transport
            // silently swallowing a protocol change.
            Err(_) => Inbound::Event(v),
        },
        Some("extension_ui_request") => Inbound::ExtensionUiRequest(v),
        _ => Inbound::Event(v),
    }
}

/// `get_state`'s payload — the fields this adapter consumes. omp's superset
/// of Pi's state; the genuinely new one is `context_usage`, which seeds the
/// meter (numerator AND denominator) at the handshake.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default)]
    pub model: Option<Model>,
    #[serde(default)]
    pub thinking_level: Option<String>,
    #[serde(default)]
    pub is_streaming: bool,
    #[serde(default)]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(default)]
    pub message_count: u64,
    #[serde(default)]
    pub context_usage: Option<ContextUsage>,
}

/// omp's live context occupancy, reported at `get_state`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    #[serde(default)]
    pub tokens: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// One omp model. Same identity fields as Pi's, but the thinking metadata is
/// restructured: omp reports `thinking.efforts` (an authoritative supported
/// list) where Pi reported a per-level remap table.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub provider: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub thinking: Option<ModelThinking>,
    /// The content kinds this model accepts (`text`, `image`).
    #[serde(default)]
    pub input: Vec<String>,
}

/// omp's per-model thinking support.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelThinking {
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub default_level: Option<String>,
}

impl Model {
    /// The label the picker shows — omp's display name, else the wire id.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// `provider/id` — the form every model reference MUST take. omp kept
    /// Pi's resolver: a bare id is a fuzzy search pattern across every
    /// provider, so an unqualified reference can silently load a different
    /// model (Pi lesson, same code lineage).
    pub fn qualified(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    /// Whether this model accepts image input.
    pub fn accepts_images(&self) -> bool {
        self.input.iter().any(|i| i == "image")
    }

    /// The thinking levels this model supports. omp's `thinking.efforts` is
    /// authoritative when present (probed: gpt-5.6-sol reports
    /// `["low","medium","high","max"]`... plus "off" is always available as
    /// "don't think"); a non-reasoning model is `off`-only; a reasoning model
    /// with no metadata falls back to Pi's conservative default set.
    pub fn supported_thinking_levels(&self) -> Vec<String> {
        if !self.reasoning {
            return vec!["off".to_string()];
        }
        if let Some(t) = self.thinking.as_ref().filter(|t| !t.efforts.is_empty()) {
            let mut levels = vec!["off".to_string()];
            levels.extend(t.efforts.iter().cloned());
            return levels;
        }
        THINKING_LEVELS
            .iter()
            .filter(|l| !matches!(**l, "xhigh" | "max"))
            .map(|l| l.to_string())
            .collect()
    }
}

/// `get_available_models`' payload — wrapped in a `models` key, pre-filtered
/// to providers the user has credentials for.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailableModels {
    #[serde(default)]
    pub models: Vec<Model>,
}

/// One command from `get_available_commands` / the unsolicited
/// `available_commands_update` push. LOOSER than Pi's shape on purpose: the
/// push frames omit `source` entirely (probed live — entries carry only
/// `name`/`description`/`input`/`subcommands`), so every field beyond the
/// name is optional here.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    /// Without the leading slash; skills arrive pre-namespaced (`skill:foo`).
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_info: Option<super::super::pi::protocol::SourceInfo>,
}

impl SlashCommand {
    /// Whether this is a skill-catalog entry — by declared source when
    /// present, else by omp's own `skill:` name prefix (the push frames name
    /// skills without declaring a source).
    pub fn is_skill(&self) -> bool {
        self.source.as_deref() == Some("skill") || self.name.starts_with("skill:")
    }
}

/// `get_available_commands`' payload / the push frame's body — wrapped in a
/// `commands` key.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailableCommands {
    #[serde(default)]
    pub commands: Vec<SlashCommand>,
}

// ---------------------------------------------------------------------------
// Chunked-frame reassembly (protocol v2, inbound only)
// ---------------------------------------------------------------------------

/// One in-flight chunk sequence.
struct ChunkBuffer {
    chunk_id: String,
    count: u64,
    byte_length: u64,
    next_index: u64,
    bytes: Vec<u8>,
}

/// The `preprocess` hook for [`NdjsonRpcClient`]: reassemble `rpc_chunk`
/// sequences into their logical frame, pass everything else through.
///
/// Discipline (red-team F11): a single in-flight buffer; any interrupted,
/// out-of-order, mismatched or oversized sequence hard-drops the buffer with
/// a log and resyncs on the next frame — a torn sequence must cost one frame,
/// never the stream. `max_reassembled` comes from the `ready` frame.
///
/// [`NdjsonRpcClient`]: super::super::ndjson_transport::NdjsonRpcClient
pub fn chunk_reassembler(max_reassembled: u64) -> super::super::ndjson_transport::Preprocess {
    let mut buffer: Option<ChunkBuffer> = None;
    Box::new(move |v: Value| {
        if v.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            // Not a chunk. A chunk sequence is written contiguously by omp, so
            // an interleaved frame mid-sequence means the sequence is torn.
            if buffer.take().is_some() {
                tracing::warn!("omp rpc_chunk sequence interrupted by another frame; dropped");
            }
            return vec![v];
        }
        let (Some(chunk_id), Some(index), Some(count), Some(byte_length), Some(data)) = (
            v.get("chunkId").and_then(Value::as_str),
            v.get("index").and_then(Value::as_u64),
            v.get("count").and_then(Value::as_u64),
            v.get("byteLength").and_then(Value::as_u64),
            v.get("data").and_then(Value::as_str),
        ) else {
            tracing::warn!("omp rpc_chunk missing fields; dropped");
            buffer = None;
            return Vec::new();
        };
        if byte_length > max_reassembled {
            tracing::warn!(byte_length, max_reassembled, "omp rpc_chunk frame over the ceiling; dropped");
            buffer = None;
            return Vec::new();
        }
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(data) else {
            tracing::warn!("omp rpc_chunk carried invalid base64; sequence dropped");
            buffer = None;
            return Vec::new();
        };
        if index == 0 {
            if buffer.is_some() {
                tracing::warn!("omp rpc_chunk restarted mid-sequence; prior sequence dropped");
            }
            buffer = Some(ChunkBuffer {
                chunk_id: chunk_id.to_string(),
                count,
                byte_length,
                next_index: 0,
                bytes: Vec::with_capacity(byte_length.min(max_reassembled) as usize),
            });
        }
        let Some(buf) = buffer.as_mut() else {
            tracing::warn!("omp rpc_chunk continued a sequence that never started; dropped");
            return Vec::new();
        };
        if buf.chunk_id != chunk_id
            || buf.count != count
            || buf.byte_length != byte_length
            || buf.next_index != index
        {
            tracing::warn!(
                expected = buf.next_index,
                got = index,
                "omp rpc_chunk sequence mismatch; dropped"
            );
            buffer = None;
            return Vec::new();
        }
        buf.bytes.extend_from_slice(&decoded);
        buf.next_index += 1;
        if buf.bytes.len() as u64 > buf.byte_length {
            tracing::warn!("omp rpc_chunk sequence overran its declared byteLength; dropped");
            buffer = None;
            return Vec::new();
        }
        if buf.next_index < buf.count {
            return Vec::new();
        }
        // Final chunk: parse the logical frame.
        let done = buffer.take().expect("buffer present at final chunk");
        match serde_json::from_slice::<Value>(&done.bytes) {
            Ok(frame) => vec![frame],
            Err(err) => {
                tracing::warn!(?err, "omp reassembled rpc_chunk frame was not JSON; dropped");
                Vec::new()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Approval parsing (rpc-ui `extension_ui_request`)
// ---------------------------------------------------------------------------

/// A parsed tool-approval request.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolApproval {
    /// The wire id of the `extension_ui_request`, echoed in the response.
    pub id: String,
    /// The tool asking (`bash`, `edit`, `write`, an `mcp__…` name, …).
    pub tool_name: String,
    /// The body under the `Allow tool:` line — `Command: …` / `File: …`.
    pub body: String,
}

/// The value answered for approval, byte-exact: anything ≠ `"Approve"`
/// denies (omp `wrapper.ts:337`).
pub const APPROVE: &str = "Approve";
pub const DENY: &str = "Deny";

/// Parse a tool approval out of an `extension_ui_request`, or `None` for any
/// other dialog (widget updates, freeform inputs, selects that are not the
/// exact approval shape). Deliberately strict — matching the reference
/// integration — so an omp UI dialog can never be MISTAKEN for an approval:
/// `method == "select"`, options exactly `["Approve","Deny"]`, title's first
/// line `Allow tool: <name>`.
pub fn parse_tool_approval(v: &Value) -> Option<ToolApproval> {
    if v.get("method").and_then(Value::as_str) != Some("select") {
        return None;
    }
    let options = v.get("options")?.as_array()?;
    if options.len() != 2
        || options[0].as_str() != Some(APPROVE)
        || options[1].as_str() != Some(DENY)
    {
        return None;
    }
    let id = v.get("id")?.as_str()?.to_string();
    let title = v.get("title")?.as_str()?;
    let (first, body) = title.split_once('\n').unwrap_or((title, ""));
    let tool_name = first.trim().strip_prefix("Allow tool: ")?.trim();
    if tool_name.is_empty() {
        return None;
    }
    Some(ToolApproval { id, tool_name: tool_name.to_string(), body: body.to_string() })
}

/// Structured `input` for the approval card, derived from the title body the
/// way the reference integration does: `Command:` for bash, `File:`/`Path:`
/// for edit/write. Falls back to the raw body so an unrecognized layout still
/// shows the user what would run.
pub fn approval_input(tool_name: &str, body: &str) -> Value {
    let prefixed = |prefix: &str| -> Option<String> {
        body.lines().find_map(|l| l.trim().strip_prefix(prefix).map(|v| v.trim().to_string()))
    };
    match tool_name {
        "bash" => {
            // The command may span lines; take everything after the first
            // `Command:` marker.
            if let Some(at) = body.find("Command:") {
                let cmd = body[at + "Command:".len()..].trim();
                return serde_json::json!({ "command": cmd });
            }
            serde_json::json!({ "detail": body })
        }
        "edit" => match prefixed("File:") {
            Some(f) => serde_json::json!({ "file_path": f }),
            None => serde_json::json!({ "detail": body }),
        },
        "write" => match prefixed("Path:") {
            Some(f) => serde_json::json!({ "file_path": f }),
            None => serde_json::json!({ "detail": body }),
        },
        _ => serde_json::json!({ "detail": body }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- serialization asserted against live-captured frames (probe 01) ---

    #[test]
    fn commands_serialize_to_omps_real_field_names() {
        let neg = OmpCommand::NegotiateProtocol { id: "req_1".into(), protocol_version: 2 };
        assert_eq!(
            serde_json::to_value(&neg).unwrap(),
            json!({"type":"negotiate_protocol","id":"req_1","protocolVersion":2})
        );
        let sm = OmpCommand::SetModel {
            id: "r".into(),
            provider: "openai-codex".into(),
            model_id: "gpt-5.6-sol".into(),
        };
        // `modelId`, not `model` — probed live: the wrong key made omp read
        // "amazon-bedrock/undefined".
        assert_eq!(
            serde_json::to_value(&sm).unwrap(),
            json!({"type":"set_model","id":"r","provider":"openai-codex","modelId":"gpt-5.6-sol"})
        );
        let p = OmpCommand::Prompt { id: "p1".into(), message: "hi".into(), images: Vec::new() };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"type":"prompt","id":"p1","message":"hi"}),
            "empty images must be OMITTED, not sent as []"
        );
        let sub = OmpCommand::SetSubagentSubscription { id: "s".into(), level: "lifecycle".into() };
        assert_eq!(
            serde_json::to_value(&sub).unwrap(),
            json!({"type":"set_subagent_subscription","id":"s","level":"lifecycle"})
        );
    }

    #[test]
    fn the_ui_response_carries_the_requests_own_id() {
        let r = ExtensionUiResponse { id: "15654c5b8e9fba59".into(), value: Some(APPROVE.into()), cancelled: None };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"type":"extension_ui_response","id":"15654c5b8e9fba59","value":"Approve"})
        );
        let c = ExtensionUiResponse { id: "x".into(), value: None, cancelled: Some(true) };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({"type":"extension_ui_response","id":"x","cancelled":true})
        );
    }

    #[test]
    fn the_ready_frame_decodes_the_live_shape() {
        // Byte-for-byte the frame omp 18.0.4 printed at probe 01.
        let live = r#"{"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,"maxReassembledFrameBytes":67108864}"#;
        let r: ReadyFrame = serde_json::from_str(live).unwrap();
        assert_eq!(r.protocol_version, 1);
        assert!(r.supported_protocol_versions.contains(&2));
        assert_eq!(r.max_frame_bytes, 1_048_576);
        assert_eq!(r.max_reassembled_frame_bytes, 67_108_864);
    }

    #[test]
    fn get_state_decodes_the_live_payload_shape() {
        let live = json!({
            "model": {
                "id": "gpt-5.6-sol", "name": "GPT-5.6-Sol", "provider": "openai-codex",
                "reasoning": true, "contextWindow": 272000, "maxTokens": 128000,
                "thinking": {"mode": "effort", "efforts": ["low","medium","high","max"]},
                "input": ["text","image"]
            },
            "thinkingLevel": "high",
            "isStreaming": false,
            "sessionId": "01a037fe-2a2b-76e1-8d1f-db954755a79c",
            "sessionFile": "/tmp/x/sessions/p/s.jsonl",
            "messageCount": 2,
            "contextUsage": {"tokens": 48183, "contextWindow": 1000000, "percent": 4.8}
        });
        let s: SessionState = serde_json::from_value(live).unwrap();
        assert_eq!(s.session_id, "01a037fe-2a2b-76e1-8d1f-db954755a79c");
        let m = s.model.unwrap();
        assert_eq!(m.qualified(), "openai-codex/gpt-5.6-sol");
        assert!(m.accepts_images());
        assert_eq!(
            m.supported_thinking_levels(),
            vec!["off", "low", "medium", "high", "max"],
            "omp's efforts list is authoritative, plus off"
        );
        assert_eq!(s.context_usage.unwrap().tokens, Some(48183));
    }

    #[test]
    fn a_non_reasoning_model_thinks_only_off() {
        let m: Model = serde_json::from_value(json!({
            "id": "m", "provider": "p", "reasoning": false
        }))
        .unwrap();
        assert_eq!(m.supported_thinking_levels(), vec!["off"]);
    }

    // --- approval parsing, against the live-captured frame ---

    #[test]
    fn the_live_approval_frame_parses_and_noise_does_not() {
        // Byte-shape from probe 01.
        let live = json!({
            "type": "extension_ui_request",
            "id": "15654c5b8e9fba59",
            "method": "select",
            "title": "Allow tool: bash\nCommand: echo touched > /tmp/x/marker.txt",
            "options": ["Approve", "Deny"]
        });
        let a = parse_tool_approval(&live).expect("the live frame is an approval");
        assert_eq!(a.id, "15654c5b8e9fba59");
        assert_eq!(a.tool_name, "bash");
        assert_eq!(approval_input(&a.tool_name, &a.body), json!({"command": "echo touched > /tmp/x/marker.txt"}));

        // The widget-update noise omp also sends over the same frame type —
        // treating it as an approval would deny a phantom tool forever.
        let widget = json!({
            "type": "extension_ui_request", "id": "x",
            "method": "setWidget", "widgetKey": "autoresearch"
        });
        assert!(parse_tool_approval(&widget).is_none());

        // A select that is NOT the exact Approve/Deny pair is some other
        // dialog — never an approval.
        let other = json!({
            "type": "extension_ui_request", "id": "y", "method": "select",
            "title": "Pick a thing", "options": ["A", "B", "C"]
        });
        assert!(parse_tool_approval(&other).is_none());
        let reordered = json!({
            "type": "extension_ui_request", "id": "z", "method": "select",
            "title": "Allow tool: bash\nCommand: rm -rf /", "options": ["Deny", "Approve"]
        });
        assert!(parse_tool_approval(&reordered).is_none(), "exact option order is part of the shape");
    }

    #[test]
    fn edit_and_write_approvals_extract_their_file_paths() {
        assert_eq!(approval_input("edit", "File: /a/b.rs\n@@ -1 +1 @@"), json!({"file_path": "/a/b.rs"}));
        assert_eq!(approval_input("write", "Path: /a/c.txt\nContent:\nhello"), json!({"file_path": "/a/c.txt"}));
        assert_eq!(approval_input("mcp__x_y", "whatever"), json!({"detail": "whatever"}));
    }

    // --- chunk reassembly ---

    fn chunk(chunk_id: &str, index: u64, count: u64, total: &[u8], part: &[u8]) -> Value {
        json!({
            "type": "rpc_chunk", "chunkId": chunk_id, "index": index, "count": count,
            "byteLength": total.len(),
            "data": base64::engine::general_purpose::STANDARD.encode(part)
        })
    }

    #[test]
    fn a_chunk_sequence_reassembles_to_its_logical_frame() {
        let mut pre = chunk_reassembler(64 * 1024 * 1024);
        let frame = br#"{"type":"message_end","message":{"role":"assistant"}}"#;
        let (a, b) = frame.split_at(20);
        assert!(pre(chunk("c1", 0, 2, frame, a)).is_empty());
        let out = pre(chunk("c1", 1, 2, frame, b));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "message_end");
    }

    #[test]
    fn non_chunk_frames_pass_through_untouched() {
        let mut pre = chunk_reassembler(1024);
        let ev = json!({"type": "agent_start"});
        assert_eq!(pre(ev.clone()), vec![ev]);
    }

    #[test]
    fn a_torn_sequence_costs_one_frame_never_the_stream() {
        let mut pre = chunk_reassembler(64 * 1024 * 1024);
        let frame = br#"{"type":"x"}"#;
        let (a, _b) = frame.split_at(4);
        assert!(pre(chunk("c1", 0, 2, frame, a)).is_empty());
        // An ordinary frame lands mid-sequence: the sequence is torn, the
        // frame still flows.
        let ev = json!({"type": "agent_end"});
        assert_eq!(pre(ev.clone()), vec![ev]);
        // A fresh, complete sequence works after the tear.
        let whole = br#"{"type":"y"}"#;
        let out = pre(chunk("c2", 0, 1, whole, whole));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "y");
    }

    #[test]
    fn mismatched_and_oversized_sequences_are_dropped() {
        let mut pre = chunk_reassembler(8);
        let big = br#"{"type":"too-big-for-the-ceiling"}"#;
        assert!(pre(chunk("c1", 0, 1, big, big)).is_empty(), "over the ceiling");
        let mut pre = chunk_reassembler(1024);
        let frame = br#"{"type":"x"}"#;
        let (a, b) = frame.split_at(4);
        assert!(pre(chunk("c1", 0, 2, frame, a)).is_empty());
        // Wrong index order.
        assert!(pre(chunk("c1", 0, 2, frame, b)).is_empty());
        // The stream recovers.
        let whole = br#"{"type":"ok"}"#;
        assert_eq!(pre(chunk("c2", 0, 1, whole, whole)).len(), 1);
    }
}
