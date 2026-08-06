//! `oximux permit` — list and decide a session's pending permission requests
//! and `AskUserQuestion`s. Pending state is derived from the retained event
//! stream (requests since the last completed turn, while the session reports
//! `awaiting_permission`); deciding echoes the host's own event back at it,
//! which is the protocol's contract for both RPCs.

use std::io::{IsTerminal as _, Write as _};

use oximux_agent_core::thread::{
    AskQuestion, PermissionDecision, QuestionAnswer, QuestionAnswers, ThreadEvent,
};
use oximux_remote_proto::messages::{AnswerQuestionReq, ResolvePermissionReq};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// One undecided request, as reconstructed from the event stream.
enum Pending {
    Permission { request_id: String, tool_name: String, description: String, input: Value },
    Question { request_id: String, questions: Vec<AskQuestion> },
}

impl Pending {
    fn request_id(&self) -> &str {
        match self {
            Pending::Permission { request_id, .. } | Pending::Question { request_id, .. } => {
                request_id
            }
        }
    }
}

/// The session's pending requests: everything asked since the last completed
/// turn, while the session still reports something awaiting a decision. An
/// individual entry may have been decided moments ago — the resolve RPCs are
/// idempotent and report `AlreadyDecided` as success, so a stale row is
/// harmless rather than wrong.
async fn pending(client: &Client, session: &str) -> Result<Vec<Pending>, Failure> {
    let awaiting = match client.call(Request::GetSessionInfo { session_id: session.into() }).await? {
        Response::SessionInfo(info) => info.summary.awaiting_permission,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GetSessionInfo", &other)),
    };
    if !awaiting {
        return Ok(Vec::new());
    }
    let frames = match client
        .call(Request::EventsSince { session_id: session.into(), after_seq: 0 })
        .await?
    {
        Response::Events(frames) => frames,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("EventsSince", &other)),
    };
    let mut list: Vec<Pending> = Vec::new();
    for frame in &frames {
        match frame.event() {
            // A completed turn retires its requests: whatever was pending
            // either got decided or died with the turn.
            Ok(ThreadEvent::TurnEnded { .. }) => list.clear(),
            Ok(ThreadEvent::PermissionRequested {
                request_id, tool_name, description, input, ..
            }) => list.push(Pending::Permission { request_id, tool_name, description, input }),
            Ok(ThreadEvent::QuestionAsked { request_id, questions, .. }) => {
                list.push(Pending::Question { request_id, questions })
            }
            _ => {}
        }
    }
    Ok(list)
}

/// Resolve which pending entry a decision targets: the named one, else the
/// most recent. Naming a request that is no longer reconstructible is an
/// error — deciding it needs the original event (the input to echo).
fn pick<'a>(list: &'a [Pending], request: Option<&str>) -> Result<&'a Pending, Failure> {
    match request {
        Some(id) => list.iter().find(|p| p.request_id() == id).ok_or_else(|| {
            Failure::new("unknown-request", exit::ERROR, format!("no pending request {id}"))
                .with_steps(["list pending requests with `oximux permit ls <session>`".into()])
        }),
        None => list.last().ok_or_else(|| {
            Failure::new("nothing-pending", exit::ERROR, "nothing is awaiting a decision")
                .with_steps(["check state with `oximux permit ls <session>`".into()])
        }),
    }
}

pub async fn ls(client: &Client, session: &str) -> Result<(Value, String), Failure> {
    let list = pending(client, session).await?;
    let rows: Vec<Value> = list
        .iter()
        .map(|p| match p {
            Pending::Permission { request_id, tool_name, description, input } => json!({
                "kind": "permission",
                "request_id": request_id,
                "tool": tool_name,
                "description": description,
                // The arguments the agent proposed. JSON only: the human line
                // is a scannable index, and a tool input can be a multi-line
                // patch. It is here because `permit allow --input` edits from
                // it — without it an operator narrowing an over-broad call
                // would have to guess the tool's own field names.
                "input": input,
            }),
            Pending::Question { request_id, questions } => json!({
                "kind": "question",
                "request_id": request_id,
                "questions": questions.iter().map(|q| &q.question).collect::<Vec<_>>(),
            }),
        })
        .collect();
    let human = if list.is_empty() {
        "nothing awaiting a decision".to_string()
    } else {
        list.iter()
            .map(|p| match p {
                Pending::Permission { request_id, tool_name, description, .. } => {
                    format!("{request_id}  permission  {tool_name}  {description}")
                }
                Pending::Question { request_id, questions } => {
                    let head = questions.first().map(|q| q.question.as_str()).unwrap_or("");
                    format!("{request_id}  question    {head}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(rows), human))
}

/// Shared resolve path for allow/deny. `AlreadyDecided` is success: the
/// request reached the state the caller wanted, just via someone else.
async fn resolve(
    client: &Client,
    session: &str,
    request_id: &str,
    decision: &PermissionDecision,
) -> Result<(Value, String), Failure> {
    let req = ResolvePermissionReq::new(session, request_id, decision).map_err(|e| {
        Failure::new("encode", exit::ERROR, format!("could not encode the decision: {e}"))
    })?;
    match client.call(Request::ResolvePermission(req)).await? {
        Response::Ack => Ok((
            json!({ "session_id": session, "request_id": request_id, "decided": true }),
            format!("decided {request_id}"),
        )),
        Response::Error(RpcError::AlreadyDecided) => Ok((
            json!({ "session_id": session, "request_id": request_id, "decided": false, "already": true }),
            format!("{request_id} was already decided"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("ResolvePermission", &other)),
    }
}

pub async fn allow(
    client: &Client,
    session: &str,
    request: Option<&str>,
    edited_input: Option<&str>,
) -> Result<(Value, String), Failure> {
    // Parse the override BEFORE touching the host. It is a mistake in the argv
    // that no host can fix, and checking it after the lookup meant a malformed
    // `--input` was reported as "no such session" whenever the session was also
    // wrong — a diagnosis about the environment for a problem in the arguments.
    let override_input = edited_input.map(parse_input_override).transpose()?;

    let list = pending(client, session).await?;
    let picked = pick(&list, request)?;
    let Pending::Permission { request_id, input, .. } = picked else {
        return Err(Failure::new(
            "wrong-kind",
            exit::ERROR,
            "that request is a question — answer it with `oximux permit answer`",
        ));
    };
    // An allow always carries the input the tool will run with. Without
    // `--input` that is the agent's own, echoed unmodified — the plain
    // approval. With it, the operator's replaces it: the agent is told the call
    // was permitted and runs these arguments, which is how an over-broad
    // command gets narrowed instead of denied and re-prompted for.
    //
    // Replacement, not a merge. A merge would have to guess whether an absent
    // key means "leave it" or "remove it", and guessing wrong on a tool call
    // the operator is explicitly correcting is the one outcome this flag must
    // not have.
    let decision =
        PermissionDecision::Allow { updated_input: override_input.unwrap_or_else(|| input.clone()) };
    resolve(client, session, request_id, &decision).await
}

/// Parse `--input`, refusing anything a tool could not receive.
///
/// A usage error, not a host error: the argument is wrong in the argv and no
/// host can make it right. Objects only — every tool's input is a named-field
/// object, and a bare array or string would reach the backend as something it
/// cannot destructure, failing far from the mistake.
pub fn parse_input_override(raw: &str) -> Result<Value, Failure> {
    let usage = |msg: String| {
        Failure::new("bad-input", exit::USAGE, msg).with_steps([
            "`oximux permit ls <session> --json` prints the proposed input to edit from".into(),
            "pass a complete JSON object, e.g. --input '{\"command\":\"ls -la\"}'".into(),
        ])
    };
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| usage(format!("--input is not valid JSON: {e}")))?;
    if !value.is_object() {
        return Err(usage(format!(
            "--input must be a JSON object; got {}",
            match &value {
                Value::Array(_) => "an array",
                Value::String(_) => "a string",
                Value::Null => "null",
                _ => "a scalar",
            }
        )));
    }
    Ok(value)
}

pub async fn deny(
    client: &Client,
    session: &str,
    request: Option<&str>,
    message: &str,
) -> Result<(Value, String), Failure> {
    let list = pending(client, session).await?;
    let picked = pick(&list, request)?;
    let Pending::Permission { request_id, .. } = picked else {
        return Err(Failure::new(
            "wrong-kind",
            exit::ERROR,
            "that request is a question — answer it with `oximux permit answer`",
        ));
    };
    let decision = PermissionDecision::Deny { message: message.to_string() };
    resolve(client, session, request_id, &decision).await
}

/// Turn one `--answer` value into a [`QuestionAnswer`] for its question: an
/// option label (case-insensitive), a 1-based option number, or free text.
/// Multi-select accepts comma-separated values.
fn parse_answer(question: &AskQuestion, raw: &str) -> QuestionAnswer {
    let mut selected = Vec::new();
    let mut custom = None;
    let parts: Vec<&str> = if question.multi_select() {
        raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()
    } else {
        vec![raw.trim()]
    };
    for part in parts {
        let by_label = question
            .options
            .iter()
            .find(|o| o.label.eq_ignore_ascii_case(part))
            .map(|o| o.label.clone());
        let by_number = part
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| question.options.get(i))
            .map(|o| o.label.clone());
        match by_label.or(by_number) {
            Some(label) => selected.push(label),
            // Not an option: free text (the "Other" lane).
            None => custom = Some(part.to_string()),
        }
    }
    QuestionAnswer { selected, custom }
}

/// Ask one question at the terminal: numbered options, free text accepted.
fn prompt_at_tty(question: &AskQuestion) -> Result<QuestionAnswer, Failure> {
    let mut out = std::io::stderr();
    let _ = writeln!(out, "\n{}", question.question);
    for (i, option) in question.options.iter().enumerate() {
        let desc = if option.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", option.description)
        };
        let _ = writeln!(out, "  {}. {}{desc}", i + 1, option.label);
    }
    let hint = if question.multi_select() { "numbers/labels, comma-separated" } else { "number, label, or free text" };
    let _ = write!(out, "answer ({hint}): ");
    let _ = out.flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| {
        Failure::new("stdin", exit::ERROR, format!("could not read the answer: {e}"))
    })?;
    let line = line.trim();
    if line.is_empty() {
        return Err(Failure::new("empty-answer", exit::ERROR, "no answer given"));
    }
    Ok(parse_answer(question, line))
}

pub async fn answer(
    client: &Client,
    session: &str,
    request: Option<&str>,
    answers: &[String],
) -> Result<(Value, String), Failure> {
    let list = pending(client, session).await?;
    let picked = pick(&list, request)?;
    let Pending::Question { request_id, questions } = picked else {
        return Err(Failure::new(
            "wrong-kind",
            exit::ERROR,
            "that request is a permission — decide it with `oximux permit allow|deny`",
        ));
    };
    let mut by_question = std::collections::HashMap::new();
    if answers.is_empty() {
        // Interactive: a picker per question, on a terminal only.
        if !std::io::stdin().is_terminal() {
            return Err(Failure::new(
                "needs-answers",
                exit::USAGE,
                "no terminal to ask on — pass one --answer per question",
            ));
        }
        for question in questions {
            by_question.insert(question.id.clone(), prompt_at_tty(question)?);
        }
    } else {
        if answers.len() != questions.len() {
            return Err(Failure::new(
                "answer-count",
                exit::USAGE,
                format!(
                    "{} question(s) but {} --answer value(s) — pass one per question, in order",
                    questions.len(),
                    answers.len()
                ),
            ));
        }
        for (question, raw) in questions.iter().zip(answers) {
            by_question.insert(question.id.clone(), parse_answer(question, raw));
        }
    }
    let payload = QuestionAnswers { by_question, response: None };
    match client
        .call(Request::AnswerQuestion(AnswerQuestionReq {
            session_id: session.into(),
            request_id: request_id.clone(),
            questions: questions.clone(),
            answers: payload,
        }))
        .await?
    {
        Response::Ack => Ok((
            json!({ "session_id": session, "request_id": request_id, "answered": true }),
            format!("answered {request_id}"),
        )),
        Response::Error(RpcError::AlreadyDecided) => Ok((
            json!({ "session_id": session, "request_id": request_id, "answered": false, "already": true }),
            format!("{request_id} was already answered"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("AnswerQuestion", &other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--input` replaces the tool's arguments; anything a tool could not
    /// receive is refused in the argv, not on the wire.
    ///
    /// The object check is the load-bearing half. Every tool input is a
    /// named-field object, so a bare array or string would reach the backend as
    /// something it cannot destructure — failing inside the agent, far from the
    /// mistake, on a call the operator was in the middle of correcting.
    #[test]
    fn an_input_override_must_be_a_json_object() {
        let ok = parse_input_override(r#"{"command":"ls -la"}"#).expect("an object");
        assert_eq!(ok, serde_json::json!({"command": "ls -la"}));

        for bad in [r#"["ls"]"#, r#""ls""#, "42", "null", "{not json"] {
            let err = parse_input_override(bad).expect_err(bad);
            assert_eq!(err.exit, exit::USAGE, "{bad}");
            assert!(
                err.next_steps.iter().any(|s| s.contains("permit ls")),
                "{bad} must point at where the input comes from"
            );
        }
    }

    /// An empty object is a legitimate override — some tools take no arguments,
    /// and refusing it would make the flag unusable for exactly those.
    #[test]
    fn an_empty_object_is_a_valid_override() {
        assert_eq!(parse_input_override("{}").unwrap(), serde_json::json!({}));
    }
}
