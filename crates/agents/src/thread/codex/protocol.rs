//! Codex `app-server` (v2) wire protocol — the minimal slice OxiMux drives.
//!
//! Verified against the local binary `codex-cli 0.144.1` via
//! `codex app-server generate-json-schema` / `generate-ts` (NOT reverse-engineered).
//! Only the fields OxiMux actually sends/reads are modeled; everything is built as
//! `serde_json::Value` so unknown/added fields on the wire are tolerated. Framing is
//! newline-delimited JSON (one value per `\n`), NOT Content-Length.
//!
//! Method names + shapes (codex 0.144.1):
//! - `initialize` → `{ clientInfo:{name,title,version}, capabilities:{experimentalApi,requestAttestation} }`
//! - `initialized` (client notification, no params)
//! - `thread/start` → `{ model?, cwd?, approvalPolicy?, sandbox? }` ; resp `{ thread:{id,..}, model, .. }`
//! - `turn/start` → `{ threadId, input:[{type:"text",text,text_elements:[]}], .. }`
//! - `turn/interrupt` → `{ threadId, turnId }`
//! - notifications: `item/agentMessage/delta {delta}`, `turn/started {turn:{id,..}}`,
//!   `turn/completed {turn}`, `thread/tokenUsage/updated {tokenUsage:{total,last,..}}`, `error`.

use std::path::Path;

use serde_json::{json, Value};

// --- Method names (client → server requests) -----------------------------
pub const M_INITIALIZE: &str = "initialize";
pub const M_THREAD_START: &str = "thread/start";
pub const M_THREAD_RESUME: &str = "thread/resume";
pub const M_MODEL_LIST: &str = "model/list";
pub const M_TURN_START: &str = "turn/start";
pub const M_TURN_INTERRUPT: &str = "turn/interrupt";

// --- Client → server notifications ---------------------------------------
pub const N_INITIALIZED: &str = "initialized";

// (Server → client notification method literals + their parsing live in `map.rs`,
//  which owns the notification → ThreadEvent mapping.)

// --- Fixed P1 posture (validated): on-request approvals + workspace sandbox
pub const APPROVAL_ON_REQUEST: &str = "on-request";
pub const SANDBOX_WORKSPACE_WRITE: &str = "workspace-write";

/// `initialize` params. `experimentalApi:true` opts into the experimental v2
/// methods/fields the chat relies on. `version` is the agents crate version.
pub fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "oximux", "title": null, "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": true, "requestAttestation": false }
    })
}

/// `thread/start` params for a fresh thread under the fixed P1 posture. `model`
/// is omitted when `None` (Codex picks its configured default).
pub fn thread_start_params(model: Option<&str>, cwd: &Path) -> Value {
    let mut p = json!({
        "cwd": cwd.to_string_lossy(),
        "approvalPolicy": APPROVAL_ON_REQUEST,
        "sandbox": SANDBOX_WORKSPACE_WRITE,
    });
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        p["model"] = json!(m);
    }
    p
}

/// `thread/resume` params — reconnect an existing thread by id under the fixed
/// posture, optionally overriding the model.
pub fn thread_resume_params(thread_id: &str, model: Option<&str>) -> Value {
    let mut p = json!({
        "threadId": thread_id,
        "approvalPolicy": APPROVAL_ON_REQUEST,
        "sandbox": SANDBOX_WORKSPACE_WRITE,
    });
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        p["model"] = json!(m);
    }
    p
}

/// `turn/start` params carrying one plain-text user message. The thread posture
/// (approval/sandbox) is inherited; `model`/`effort` override per turn when set
/// (how the model/effort pickers take effect).
pub fn turn_start_params(thread_id: &str, text: &str, model: Option<&str>, effort: Option<&str>) -> Value {
    let mut p = json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text, "text_elements": [] } ]
    });
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        p["model"] = json!(m);
    }
    if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
        p["effort"] = json!(e);
    }
    p
}

/// `turn/interrupt` params — cancels the in-flight turn on `thread_id`/`turn_id`.
pub fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// Pull `thread.id` out of a `thread/start` (or `thread/resume`) response.
pub fn thread_id_from_start_response(result: &Value) -> Option<String> {
    result.get("thread")?.get("id")?.as_str().map(String::from)
}

/// The resolved model from a `thread/start` / `thread/resume` response.
pub fn model_from_start_response(result: &Value) -> Option<String> {
    result.get("model")?.as_str().map(String::from)
}

/// One entry from `model/list` (the fields the picker needs). `wire` is the id
/// passed to `turn/start`'s `model`; `efforts` are the reasoning-effort options.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexModel {
    pub wire: String,
    pub display: String,
    /// The app-server's one-line capability blurb for this model, when present
    /// (e.g. "Frontier model for complex coding, research, and agentic tasks").
    /// Rendered muted beneath the name in the picker; `None` renders single-line.
    pub description: Option<String>,
    pub efforts: Vec<String>,
    pub default_effort: String,
    pub is_default: bool,
}

/// Parse a `model/list` response into the non-hidden model catalog.
pub fn parse_model_list(result: &Value) -> Vec<CodexModel> {
    let Some(arr) = result.get("data").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter(|m| !m.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false))
        .filter_map(|m| {
            let wire = m.get("id").and_then(|v| v.as_str())?.to_string();
            let efforts = m
                .get("supportedReasoningEfforts")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e.get("reasoningEffort").and_then(|r| r.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Some(CodexModel {
                display: m.get("displayName").and_then(|v| v.as_str()).unwrap_or(&wire).to_string(),
                description: m.get("description").and_then(|v| v.as_str()).map(String::from),
                default_effort: m.get("defaultReasoningEffort").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                is_default: m.get("isDefault").and_then(|v| v.as_bool()).unwrap_or(false),
                efforts,
                wire,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn thread_start_omits_blank_model_and_sets_posture() {
        let p = thread_start_params(None, &PathBuf::from("/tmp/x"));
        assert_eq!(p["approvalPolicy"], "on-request");
        assert_eq!(p["sandbox"], "workspace-write");
        assert!(p.get("model").is_none());
        let p2 = thread_start_params(Some("gpt-5.5"), &PathBuf::from("/tmp/x"));
        assert_eq!(p2["model"], "gpt-5.5");
    }

    #[test]
    fn turn_start_shapes_text_input() {
        let p = turn_start_params("th_1", "hi", None, None);
        assert_eq!(p["threadId"], "th_1");
        assert_eq!(p["input"][0]["type"], "text");
        assert_eq!(p["input"][0]["text"], "hi");
        assert!(p["input"][0]["text_elements"].is_array());
        assert!(p.get("model").is_none());
        // model + effort ride as per-turn overrides when set.
        let p2 = turn_start_params("th_1", "hi", Some("gpt-5.5"), Some("high"));
        assert_eq!(p2["model"], "gpt-5.5");
        assert_eq!(p2["effort"], "high");
    }

    #[test]
    fn parses_model_list_catalog() {
        let result = json!({"data": [
            {"id": "gpt-5.5", "displayName": "GPT-5.5", "isDefault": true, "hidden": false,
             "description": "Frontier model for complex coding, research, and agentic tasks.",
             "defaultReasoningEffort": "medium",
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "high"}]},
            {"id": "o3", "displayName": "o3", "hidden": false},
            {"id": "secret", "displayName": "Secret", "hidden": true},
        ]});
        let models = parse_model_list(&result);
        assert_eq!(models.len(), 2, "hidden models are dropped");
        assert_eq!(models[0].wire, "gpt-5.5");
        assert!(models[0].is_default);
        assert_eq!(models[0].efforts, vec!["low", "high"]);
        assert_eq!(models[0].default_effort, "medium");
        // The app-server blurb is surfaced; a model without one parses to `None`.
        assert_eq!(
            models[0].description.as_deref(),
            Some("Frontier model for complex coding, research, and agentic tasks.")
        );
        assert_eq!(models[1].description, None);
    }

    #[test]
    fn parses_thread_start_response() {
        let start = json!({"thread": {"id": "th_9", "sessionId": "s"}, "model": "gpt"});
        assert_eq!(thread_id_from_start_response(&start).as_deref(), Some("th_9"));
        assert_eq!(model_from_start_response(&start).as_deref(), Some("gpt"));
    }
}
