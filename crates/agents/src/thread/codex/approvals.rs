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
use super::super::question::{AskQuestion, QuestionAnswers, QuestionKind, QuestionOption};
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
        // A clarifying question: surface the interactive card and route the
        // user's selections back. Codex questions carry their own `id`
        // (preserved so the answer keys by it) and, unlike Claude's
        // AskUserQuestion, no `multiSelect` flag — each is single-select with an
        // optional free-text "Other" (`isOther`). An empty question set can't be
        // answered, so degrade to the auto-empty reply rather than emitting an
        // unanswerable card that would stall the turn.
        "item/tool/requestUserInput" => {
            let questions = parse_codex_questions(params);
            if questions.is_empty() {
                return ServerRequestAction::AutoRespond(json!({ "answers": {} }));
            }
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or_default();
            let key = id_key(id);
            st.pending_approvals.insert(key.clone(), id.clone());
            ServerRequestAction::Emit(vec![ThreadEvent::QuestionAsked {
                request_id: key,
                tool_use_id: (!item_id.is_empty()).then(|| item_id.to_string()),
                questions,
            }])
        }
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

/// Parse Codex `ToolRequestUserInputParams.questions` into the shared
/// [`AskQuestion`] model. Codex supplies each question's own `id` (kept so the
/// answer can be keyed by it), a `header`/`question`, optional `options`
/// (`{label, description}`), and an `isOther` flag for the free-text choice.
/// There is no `multiSelect`, so every question is single-select. Missing fields
/// degrade to empty rather than panicking (the schema is experimental).
fn parse_codex_questions(params: &Value) -> Vec<AskQuestion> {
    let Some(arr) = params.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|q| {
            let id = q.get("id").and_then(Value::as_str)?.to_string();
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .map(|o| QuestionOption {
                            label: str_field(o, "label"),
                            description: str_field(o, "description"),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(AskQuestion {
                id,
                header: str_field(q, "header"),
                question: str_field(q, "question"),
                options,
                kind: QuestionKind::SingleSelect,
                other_allowed: q.get("isOther").and_then(Value::as_bool).unwrap_or(false),
                is_secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// Build the Codex `ToolRequestUserInputResponse` payload: `{answers: {<qid>:
/// {answers: [..]}}}`, keyed by each question's Codex `id`. Custom "Other" text
/// wins over selected labels; a question with no resolved answer is omitted.
pub fn codex_answers_json(questions: &[AskQuestion], answers: &QuestionAnswers) -> Value {
    let mut map = serde_json::Map::new();
    for q in questions {
        let Some(a) = answers.by_question.get(&q.id) else { continue };
        // Custom free-text supersedes selections (matches the shared resolution).
        let list: Vec<String> = match a.custom.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(c) => vec![c.to_string()],
            None => a
                .selected
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        };
        if !list.is_empty() {
            map.insert(q.id.clone(), json!({ "answers": list }));
        }
    }
    json!({ "answers": Value::Object(map) })
}

/// String field helper (missing/non-string → empty).
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
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
    fn request_user_input_empty_questions_auto_answers() {
        // No answerable questions → auto-empty reply (never a stalled turn).
        let mut st = CodexState::default();
        match map_server_request(&json!(2), "item/tool/requestUserInput", &json!({"questions": []}), &mut st) {
            ServerRequestAction::AutoRespond(v) => assert!(v["answers"].is_object()),
            _ => panic!("expected AutoRespond"),
        }
        assert!(st.pending_approvals.is_empty(), "nothing stashed for an empty question set");
    }

    #[test]
    fn request_user_input_emits_question_card_and_stashes_id() {
        let mut st = CodexState::default();
        let action = map_server_request(
            &json!(11),
            "item/tool/requestUserInput",
            &json!({"itemId":"it9","threadId":"t","turnId":"u","questions":[
                {"id":"q_lib","header":"Library","question":"Which HTTP client?",
                 "options":[{"label":"reqwest","description":"async"},{"label":"ureq","description":"blocking"}],
                 "isOther":true,"isSecret":true}]}),
            &mut st,
        );
        match action {
            ServerRequestAction::Emit(evs) => match &evs[..] {
                [ThreadEvent::QuestionAsked { request_id, tool_use_id, questions }] => {
                    assert_eq!(request_id, "11");
                    assert_eq!(tool_use_id.as_deref(), Some("it9"));
                    assert_eq!(questions.len(), 1);
                    // Codex's own question id is preserved (keys the answer).
                    assert_eq!(questions[0].id, "q_lib");
                    assert_eq!(questions[0].header, "Library");
                    assert_eq!(questions[0].options.len(), 2);
                    assert!(!questions[0].multi_select(), "Codex questions are single-select");
                    assert!(questions[0].other_allowed, "isOther maps to other_allowed");
                    assert!(questions[0].is_secret, "isSecret maps to is_secret");
                }
                other => panic!("expected one QuestionAsked, got {other:?}"),
            },
            _ => panic!("expected Emit"),
        }
        assert_eq!(st.pending_approvals.get("11"), Some(&json!(11)));
    }

    #[test]
    fn codex_answers_json_keys_by_question_id_with_array_values() {
        use super::super::super::question::{QuestionAnswer, QuestionAnswers};
        let questions = parse_codex_questions(&json!({"questions":[
            {"id":"q_lib","header":"Library","question":"Which?","options":[{"label":"reqwest","description":""}]},
            {"id":"q_fw","header":"FW","question":"Which framework?","isOther":true},
            {"id":"q_skip","header":"Skip","question":"Unanswered?"}]}));
        let mut answers = QuestionAnswers::default();
        answers.by_question.insert("q_lib".into(), QuestionAnswer { selected: vec!["reqwest".into()], custom: None });
        // Custom "Other" text wins over any selection.
        answers.by_question.insert("q_fw".into(), QuestionAnswer { selected: vec![], custom: Some("axum".into()) });
        let v = codex_answers_json(&questions, &answers);
        assert_eq!(v["answers"]["q_lib"], json!({"answers":["reqwest"]}));
        assert_eq!(v["answers"]["q_fw"], json!({"answers":["axum"]}));
        // An unanswered question is omitted entirely.
        assert!(v["answers"].get("q_skip").is_none());
    }
}
