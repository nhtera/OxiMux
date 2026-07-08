//! Codex `app-server` (v2) wire protocol — the minimal slice OxiMux drives.
//!
//! Verified against the local binary `codex-cli 0.141.0` via
//! `codex app-server generate-json-schema` / `generate-ts` (NOT reverse-engineered).
//! Only the fields OxiMux actually sends/reads are modeled; everything is built as
//! `serde_json::Value` so unknown/added fields on the wire are tolerated. Framing is
//! newline-delimited JSON (one value per `\n`), NOT Content-Length.
//!
//! Method names + shapes (codex 0.141.0):
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

/// `turn/start` params carrying one plain-text user message. The thread posture
/// (approval/sandbox) set at `thread/start` is inherited, so no per-turn override.
pub fn turn_start_params(thread_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text, "text_elements": [] } ]
    })
}

/// `turn/interrupt` params — cancels the in-flight turn on `thread_id`/`turn_id`.
pub fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// Pull `thread.id` out of a `thread/start` (or `thread/resume`) response.
pub fn thread_id_from_start_response(result: &Value) -> Option<String> {
    result.get("thread")?.get("id")?.as_str().map(String::from)
}

/// The resolved model from a `thread/start` response, if present.
pub fn model_from_start_response(result: &Value) -> Option<String> {
    result.get("model")?.as_str().map(String::from)
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
        let p = turn_start_params("th_1", "hi");
        assert_eq!(p["threadId"], "th_1");
        assert_eq!(p["input"][0]["type"], "text");
        assert_eq!(p["input"][0]["text"], "hi");
        assert!(p["input"][0]["text_elements"].is_array());
    }

    #[test]
    fn parses_thread_start_response() {
        let start = json!({"thread": {"id": "th_9", "sessionId": "s"}, "model": "gpt"});
        assert_eq!(thread_id_from_start_response(&start).as_deref(), Some("th_9"));
        assert_eq!(model_from_start_response(&start).as_deref(), Some("gpt"));
    }
}
