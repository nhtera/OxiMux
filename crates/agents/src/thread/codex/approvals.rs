//! Codex server → client approval requests → OxiMux permission cards, and the
//! `PermissionDecision` → Codex-decision translation for the reply.
//!
//! Codex (v2) asks the client to approve tool execution via JSON-RPC *requests*
//! (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`).
//! The reader must NOT block awaiting the user's choice: we emit a
//! [`ThreadEvent::PermissionRequested`], stash the request's JSON-RPC id, keep
//! reading, and answer with a `{decision}` result once `resolve_permission`
//! arrives. The command/cwd aren't in the approval params — they were sent
//! earlier on `item/started`, so we correlate by `itemId` via `CodexState`.
//! Verified against codex-cli 0.141.0 `generate-ts`.

use serde_json::{json, Value};

use super::super::event::ThreadEvent;
use super::super::tool_call::PermissionDecision;
use super::CodexState;

/// What the mapper should do with a server → client request.
pub enum ServerRequestAction {
    /// A permission/question card was emitted and the request's id stashed —
    /// do NOT answer yet (the reply follows the user's decision).
    Emit(Vec<ThreadEvent>),
    /// Answer this request immediately with the given `result` (unhandled /
    /// declined requests, so a turn never stalls waiting on us).
    AutoRespond(Value),
}

/// Classify a server → client request. For the two interactive approval RPCs,
/// emit a `PermissionRequested` and stash `id` under a stable key; everything
/// else is auto-declined with a shape-appropriate result.
pub fn map_server_request(
    id: &Value,
    method: &str,
    params: &Value,
    st: &mut CodexState,
) -> ServerRequestAction {
    match method {
        "item/commandExecution/requestApproval" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or_default();
            // Recover the command/cwd captured when the item started.
            let (command, cwd) = st
                .cmd_items
                .get(item_id)
                .map(|it| {
                    (
                        it.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                        it.get("cwd").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                    )
                })
                .unwrap_or_default();
            let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or_default();
            let description = if command.is_empty() {
                reason.to_string()
            } else {
                command.clone()
            };
            emit_permission(
                id,
                item_id,
                "Bash",
                json!({ "command": command, "cwd": cwd }),
                description,
                st,
            )
        }
        "item/fileChange/requestApproval" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or_default();
            let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("Apply file changes");
            let changes = st
                .cmd_items
                .get(item_id)
                .and_then(|it| it.get("changes").cloned())
                .unwrap_or(Value::Null);
            emit_permission(
                id,
                item_id,
                "apply_patch",
                json!({ "changes": changes }),
                reason.to_string(),
                st,
            )
        }
        // Experimental question RPC — not yet surfaced as a card (a later phase
        // maps it to AskUserQuestion); answer with an empty answers map so the
        // turn continues.
        "item/tool/requestUserInput" => ServerRequestAction::AutoRespond(json!({ "answers": {} })),
        // Everything else (legacy exec/apply-patch approvals, permissions,
        // mcp elicitation, attestation, …): decline with the common shape.
        _ => ServerRequestAction::AutoRespond(json!({ "decision": "decline" })),
    }
}

/// Emit a `PermissionRequested` and stash the request id so `resolve_permission`
/// can answer it.
fn emit_permission(
    id: &Value,
    item_id: &str,
    tool_name: &str,
    input: Value,
    description: String,
    st: &mut CodexState,
) -> ServerRequestAction {
    let key = id_key(id);
    st.pending_approvals.insert(key.clone(), id.clone());
    ServerRequestAction::Emit(vec![ThreadEvent::PermissionRequested {
        request_id: key,
        tool_use_id: (!item_id.is_empty()).then(|| item_id.to_string()),
        tool_name: tool_name.to_string(),
        input,
        description,
        suggestions: Vec::new(),
    }])
}

/// Map an OxiMux [`PermissionDecision`] to Codex's decision string.
/// `Allow` → run; `AllowWithSuggestion` (allow-always) → run for the session;
/// `Deny` → refuse.
pub fn to_codex_decision(decision: &PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow { .. } => "accept",
        PermissionDecision::AllowWithSuggestion { .. } => "acceptForSession",
        PermissionDecision::Deny { .. } => "decline",
    }
}

/// The stable string key for a JSON-RPC id (number or string).
pub fn id_key(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_translation() {
        assert_eq!(to_codex_decision(&PermissionDecision::Allow { updated_input: json!({}) }), "accept");
        assert_eq!(to_codex_decision(&PermissionDecision::Deny { message: "no".into() }), "decline");
    }

    #[test]
    fn command_approval_emits_card_and_stashes_id() {
        let mut st = CodexState::default();
        // The command item was seen earlier (item/started stashes it).
        st.cmd_items.insert(
            "it1".to_string(),
            json!({"type": "commandExecution", "id": "it1", "command": "rm -rf x", "cwd": "/tmp"}),
        );
        let action = map_server_request(
            &json!(7),
            "item/commandExecution/requestApproval",
            &json!({"itemId": "it1", "threadId": "t", "turnId": "u"}),
            &mut st,
        );
        match action {
            ServerRequestAction::Emit(evs) => match &evs[..] {
                [ThreadEvent::PermissionRequested { request_id, tool_use_id, tool_name, input, description, .. }] => {
                    assert_eq!(request_id, "7");
                    assert_eq!(tool_use_id.as_deref(), Some("it1"));
                    assert_eq!(tool_name, "Bash");
                    assert_eq!(input["command"], "rm -rf x");
                    assert_eq!(description, "rm -rf x");
                }
                other => panic!("expected one PermissionRequested, got {other:?}"),
            },
            _ => panic!("expected Emit"),
        }
        // the id is stashed for the reply
        assert_eq!(st.pending_approvals.get("7"), Some(&json!(7)));
    }

    #[test]
    fn unknown_request_is_auto_declined() {
        let mut st = CodexState::default();
        match map_server_request(&json!(1), "attestation/generate", &json!({}), &mut st) {
            ServerRequestAction::AutoRespond(v) => assert_eq!(v["decision"], "decline"),
            _ => panic!("expected AutoRespond"),
        }
        assert!(st.pending_approvals.is_empty());
    }

    #[test]
    fn request_user_input_auto_answers_empty() {
        let mut st = CodexState::default();
        match map_server_request(&json!(2), "item/tool/requestUserInput", &json!({"questions": []}), &mut st) {
            ServerRequestAction::AutoRespond(v) => assert!(v["answers"].is_object()),
            _ => panic!("expected AutoRespond"),
        }
    }
}
