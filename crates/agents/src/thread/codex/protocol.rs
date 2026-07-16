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
pub const M_THREAD_FORK: &str = "thread/fork";
pub const M_MODEL_LIST: &str = "model/list";
pub const M_TURN_START: &str = "turn/start";
pub const M_TURN_INTERRUPT: &str = "turn/interrupt";
// --- Account / ChatGPT-OAuth sign-in (0.144.x native flow) ----------------
pub const M_ACCOUNT_READ: &str = "account/read";
pub const M_ACCOUNT_LOGIN_START: &str = "account/login/start";
/// Server → client notification fired when a browser OAuth login resolves.
pub const N_ACCOUNT_LOGIN_COMPLETED: &str = "account/login/completed";
/// Server → client notification carrying the turn's accumulated unified diff
/// (`{diff, threadId, turnId}`). Cumulative — each restates the whole turn — and
/// it covers files written by shell commands as well as by patch items.
pub const N_TURN_DIFF_UPDATED: &str = "turn/diff/updated";

// --- Client → server notifications ---------------------------------------
pub const N_INITIALIZED: &str = "initialized";

// (Server → client notification method literals + their parsing live in `map.rs`,
//  which owns the notification → ThreadEvent mapping.)

// --- Approval-policy wire values (`AskForApproval`) the composer exposes.
pub const APPROVAL_ON_REQUEST: &str = "on-request";
pub const APPROVAL_NEVER: &str = "never";
// --- Sandbox-mode wire values (`SandboxMode`, used on thread/start & resume).
pub const SANDBOX_READ_ONLY: &str = "read-only";
pub const SANDBOX_WORKSPACE_WRITE: &str = "workspace-write";
pub const SANDBOX_DANGER_FULL_ACCESS: &str = "danger-full-access";

/// The default posture a fresh Codex thread starts under (unchanged from the
/// original fixed P1 posture): on-request approvals + workspace-write sandbox.
pub const DEFAULT_APPROVAL_POLICY: &str = APPROVAL_ON_REQUEST;
pub const DEFAULT_SANDBOX: &str = SANDBOX_WORKSPACE_WRITE;

/// Map a `SandboxMode` string (thread/start's `sandbox`) to the tagged
/// `SandboxPolicy` object that `turn/start`'s `sandboxPolicy` override expects.
/// Unknown values fall back to the workspace-write policy.
fn sandbox_policy_json(sandbox: &str) -> Value {
    match sandbox {
        SANDBOX_READ_ONLY => json!({ "type": "readOnly" }),
        SANDBOX_DANGER_FULL_ACCESS => json!({ "type": "dangerFullAccess" }),
        _ => json!({ "type": "workspaceWrite" }),
    }
}

/// `initialize` params. `experimentalApi:true` opts into the experimental v2
/// methods/fields the chat relies on. `version` is the agents crate version.
pub fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "oximux", "title": null, "version": env!("CARGO_PKG_VERSION") },
        "capabilities": { "experimentalApi": true, "requestAttestation": false }
    })
}

/// `thread/start` params for a fresh thread under `posture` (approval policy +
/// sandbox mode; the composer's Approvals/Sandbox selects, defaulting to
/// on-request/workspace-write). `model` is omitted when `None` (Codex picks its
/// configured default).
pub fn thread_start_params(model: Option<&str>, cwd: &Path, posture: Posture) -> Value {
    let mut p = json!({
        "cwd": cwd.to_string_lossy(),
        "approvalPolicy": posture.approval_policy,
        "sandbox": posture.sandbox,
    });
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        p["model"] = json!(m);
    }
    p
}

/// `thread/resume` params — reconnect an existing thread by id under `posture`,
/// optionally overriding the model.
pub fn thread_resume_params(thread_id: &str, model: Option<&str>, posture: Posture) -> Value {
    let mut p = json!({
        "threadId": thread_id,
        "approvalPolicy": posture.approval_policy,
        "sandbox": posture.sandbox,
    });
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        p["model"] = json!(m);
    }
    p
}

/// The approval policy + sandbox mode a Codex session runs under, chosen via the
/// composer's Approvals/Sandbox selects. Borrowed wire strings (`AskForApproval`
/// + `SandboxMode`), so no allocation to build a request.
#[derive(Debug, Clone, Copy)]
pub struct Posture<'a> {
    pub approval_policy: &'a str,
    pub sandbox: &'a str,
}

impl Default for Posture<'_> {
    fn default() -> Self {
        Self { approval_policy: DEFAULT_APPROVAL_POLICY, sandbox: DEFAULT_SANDBOX }
    }
}

/// `turn/start` params carrying one plain-text user message. `model`/`effort`
/// override per turn when set (how the model/effort pickers take effect); the
/// `posture` (approval policy + sandbox) rides as a per-turn override too — Codex
/// applies it "for this turn and subsequent turns", so a posture switch takes
/// effect on the next send with no respawn.
pub fn turn_start_params(
    thread_id: &str,
    text: &str,
    model: Option<&str>,
    effort: Option<&str>,
    posture: Posture,
) -> Value {
    let mut p = json!({
        "threadId": thread_id,
        "input": [ { "type": "text", "text": text, "text_elements": [] } ],
        "approvalPolicy": posture.approval_policy,
        "sandboxPolicy": sandbox_policy_json(posture.sandbox),
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

/// `thread/fork` params — branch `thread_id` into a NEW thread that ends at
/// `last_turn_id` (inclusive); every turn after it is dropped from the fork. The
/// original thread is left intact (recoverable). `None` forks through no turns
/// (an empty conversation — used to rewind before the very first message). The
/// posture is preserved (the fork inherits the parent's). Used for
/// conversation-rewind: pick the turn BEFORE the target user message.
/// (`thread/rollback` is the deprecated alternative — do not use it.)
pub fn thread_fork_params(thread_id: &str, last_turn_id: Option<&str>) -> Value {
    let mut p = json!({ "threadId": thread_id });
    if let Some(tid) = last_turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        p["lastTurnId"] = json!(tid);
    }
    p
}

/// Pull `thread.id` out of a `thread/start` (or `thread/resume`) response.
pub fn thread_id_from_start_response(result: &Value) -> Option<String> {
    result.get("thread")?.get("id")?.as_str().map(String::from)
}

/// The resolved model from a `thread/start` / `thread/resume` response.
pub fn model_from_start_response(result: &Value) -> Option<String> {
    result.get("model")?.as_str().map(String::from)
}

/// `account/login/start` params for the ChatGPT OAuth flow (`codexStreamlinedLogin`
/// = the merged ChatGPT-app browser flow). The response carries a `{loginId,
/// authUrl}`; the client opens `authUrl` in a browser and the app-server runs the
/// `localhost:1455` callback, then fires `account/login/completed`.
pub fn account_login_start_chatgpt_params() -> Value {
    json!({ "type": "chatgpt", "codexStreamlinedLogin": true })
}

/// Whether an `account/read` result says the session is signed out and needs an
/// OpenAI sign-in (`{account: null, requiresOpenaiAuth: true}`). A present
/// `account` (chatgpt/apiKey) is signed in.
pub fn account_read_needs_login(result: &Value) -> bool {
    result.get("account").map(Value::is_null).unwrap_or(true)
        && result.get("requiresOpenaiAuth").and_then(Value::as_bool).unwrap_or(false)
}

/// Pull the browser `authUrl` out of a chatgpt `account/login/start` response.
/// `None` for the immediate `apiKey` variant (no browser step) or a malformed one.
pub fn login_url_from_start_response(result: &Value) -> Option<String> {
    result.get("authUrl")?.as_str().map(String::from)
}

/// Whether an `error` notification is the "logged out" 401 the model websocket
/// raises when a turn runs without auth (`codexErrorInfo.responseStreamDisconnected
/// .httpStatusCode == 401`). The defensive fallback to the proactive `account/read`
/// check — the turn-time signal that a sign-in is needed.
pub fn error_is_unauthorized(params: &Value) -> bool {
    params
        .get("error")
        .and_then(|e| e.get("codexErrorInfo"))
        .and_then(|i| i.get("responseStreamDisconnected"))
        .and_then(|d| d.get("httpStatusCode"))
        .and_then(Value::as_u64)
        == Some(401)
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
    fn account_read_detects_logged_out() {
        // Logged out: null account + requiresOpenaiAuth.
        assert!(account_read_needs_login(&json!({"account": null, "requiresOpenaiAuth": true})));
        // Signed in (chatgpt) → not needing login.
        assert!(!account_read_needs_login(
            &json!({"account": {"type":"chatgpt","email":"x@y.z"}, "requiresOpenaiAuth": false})
        ));
        // A null account but auth not required (e.g. a configured base URL) → no card.
        assert!(!account_read_needs_login(&json!({"account": null, "requiresOpenaiAuth": false})));
    }

    #[test]
    fn login_start_params_and_url_extraction() {
        let p = account_login_start_chatgpt_params();
        assert_eq!(p["type"], "chatgpt");
        assert_eq!(p["codexStreamlinedLogin"], true);
        // The chatgpt response carries authUrl; the apiKey variant does not.
        assert_eq!(
            login_url_from_start_response(&json!({"type":"chatgpt","loginId":"L1","authUrl":"https://auth.openai.com/x"})),
            Some("https://auth.openai.com/x".to_string()),
        );
        assert_eq!(login_url_from_start_response(&json!({"type":"apiKey"})), None);
    }

    #[test]
    fn error_unauthorized_classifier() {
        // The captured logged-out 401 shape.
        assert!(error_is_unauthorized(&json!({
            "error": {"message":"Reconnecting... 2/5",
                "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": 401}}},
            "willRetry": true
        })));
        // A different HTTP status (e.g. 500) is not an auth prompt.
        assert!(!error_is_unauthorized(&json!({
            "error": {"codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": 500}}}
        })));
        // A generic error with no codexErrorInfo is not auth.
        assert!(!error_is_unauthorized(&json!({"error": {"message": "boom"}})));
    }

    #[test]
    fn thread_start_omits_blank_model_and_sets_posture() {
        let p = thread_start_params(None, &PathBuf::from("/tmp/x"), Posture::default());
        assert_eq!(p["approvalPolicy"], "on-request");
        assert_eq!(p["sandbox"], "workspace-write");
        assert!(p.get("model").is_none());
        let p2 = thread_start_params(Some("gpt-5.5"), &PathBuf::from("/tmp/x"), Posture::default());
        assert_eq!(p2["model"], "gpt-5.5");
        // A chosen posture flows through.
        let p3 = thread_start_params(
            None,
            &PathBuf::from("/tmp/x"),
            Posture { approval_policy: APPROVAL_NEVER, sandbox: SANDBOX_READ_ONLY },
        );
        assert_eq!(p3["approvalPolicy"], "never");
        assert_eq!(p3["sandbox"], "read-only");
    }

    #[test]
    fn turn_start_shapes_text_input() {
        let p = turn_start_params("th_1", "hi", None, None, Posture::default());
        assert_eq!(p["threadId"], "th_1");
        assert_eq!(p["input"][0]["type"], "text");
        assert_eq!(p["input"][0]["text"], "hi");
        assert!(p["input"][0]["text_elements"].is_array());
        assert!(p.get("model").is_none());
        // Default posture rides every turn as a per-turn override.
        assert_eq!(p["approvalPolicy"], "on-request");
        assert_eq!(p["sandboxPolicy"], json!({ "type": "workspaceWrite" }));
        // model + effort ride as per-turn overrides when set.
        let p2 = turn_start_params("th_1", "hi", Some("gpt-5.5"), Some("high"), Posture::default());
        assert_eq!(p2["model"], "gpt-5.5");
        assert_eq!(p2["effort"], "high");
    }

    #[test]
    fn turn_start_posture_maps_sandbox_to_tagged_policy() {
        let ro = turn_start_params(
            "t", "hi", None, None,
            Posture { approval_policy: APPROVAL_NEVER, sandbox: SANDBOX_READ_ONLY },
        );
        assert_eq!(ro["approvalPolicy"], "never");
        assert_eq!(ro["sandboxPolicy"], json!({ "type": "readOnly" }));
        let danger = turn_start_params(
            "t", "hi", None, None,
            Posture { approval_policy: APPROVAL_ON_REQUEST, sandbox: SANDBOX_DANGER_FULL_ACCESS },
        );
        assert_eq!(danger["sandboxPolicy"], json!({ "type": "dangerFullAccess" }));
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
    fn thread_fork_addresses_by_last_turn_id() {
        // Fork through a specific turn: turns after it are dropped from the fork.
        let p = thread_fork_params("th_1", Some("turn_5"));
        assert_eq!(p["threadId"], "th_1");
        assert_eq!(p["lastTurnId"], "turn_5");
        // Rewind before the first message: no lastTurnId → fork through nothing.
        let empty = thread_fork_params("th_1", None);
        assert_eq!(empty["threadId"], "th_1");
        assert!(empty.get("lastTurnId").is_none());
        // A blank turn id is treated as absent.
        assert!(thread_fork_params("th_1", Some("  ")).get("lastTurnId").is_none());
    }

    #[test]
    fn parses_thread_start_response() {
        let start = json!({"thread": {"id": "th_9", "sessionId": "s"}, "model": "gpt"});
        assert_eq!(thread_id_from_start_response(&start).as_deref(), Some("th_9"));
        assert_eq!(model_from_start_response(&start).as_deref(), Some("gpt"));
    }
}
