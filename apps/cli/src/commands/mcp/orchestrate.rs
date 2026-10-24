//! The delegation tools of `oximux mcp --tools orchestrate`, for an agent
//! OxiMux started: spawn helpers, wait on them, read and steer them, answer
//! their permits, share state.
//!
//! Each tool is a thin wrapper over the CLI verb an agent could run itself
//! (`delegate`, `send`, `transcript`, `permit`, `state`), connected with the
//! agent's own session credential, so the host's subtree rules decide what it
//! may reach. Nothing here widens them.
//!
//! `oximux_spawn_agent` is listed only while the host says the caller may
//! spawn (depth, posture, the delegation setting), so an agent that cannot
//! delegate is never offered the tool. A call that races a changed setting
//! still gets the host's refusal, which ends with what to do instead.

use std::collections::HashMap;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt as _};
use oximux_agent_core::thread::ThreadEvent;
use oximux_remote_proto::lineage::{LineageStateWire, LineageWire, WhoamiWire};
use oximux_remote_proto::messages::HostEvent;
use oximux_remote_proto::proto::{Request, Response, RpcError, SPAWN_MIN_VERSION};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::Ctx;
use super::tools::{failure_text, text_item};
use crate::cli::{NotifyArg, PostureArg};
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::commands::attach::{Stop, StreamEnd, StreamOpts, stream_session};
use crate::commands::{delegate, permit, send, state, transcript};
use crate::output::Failure;

/// Every tool here is named with this prefix.
pub(super) const PREFIX: &str = "oximux_";

pub(super) const INSTRUCTIONS: &str = "Delegates work to helper agents OxiMux runs beside you. \
Start one with oximux_spawn_agent, then call oximux_wait_agents once with the ids you depend on. \
Child events also arrive in your conversation on their own when a child finishes or gets stuck.";

/// How to wait, said once and reused wherever an agent is told to wait
/// (this tool's description, the skill guide, plan runs).
pub const WAIT_GUIDANCE: &str = "Do not poll. Call once with the ids you depend on and let it wait. \
If three waits in a row return nothing new, call oximux_list_children and act on what you see: read \
a finished result, answer a permit, send a prompt, or cancel.";

/// How a parent treats its children's results.
pub const ETIQUETTE: &str = "Report your children's results in your own final response. There is no \
need to send acknowledgements. A queued status is not a failure, so do not resend.";

/// The longest single wait.
const MAX_WAIT_MS: u64 = 60_000;
/// The most children one wait watches.
const MAX_WAIT_IDS: usize = 20;
/// Budget for every call but a wait: connection, the verb, the answer.
const CALL_BUDGET: Duration = Duration::from_secs(30);
/// Added to a wait's own timeout for its call budget.
const WAIT_SLACK: Duration = Duration::from_secs(10);
/// How much of a child's last reply a wait returns.
const SUMMARY_LIMIT: usize = 4000;
const READ_TAIL_DEFAULT: u64 = 20;
const READ_TAIL_MAX: u64 = 200;
/// How often a wait looks again at a child that has not registered yet.
const START_POLL: Duration = Duration::from_secs(1);

/// `tools/list` for the delegation set.
pub(super) async fn list(ctx: &Ctx) -> Vec<Value> {
    let me = match connect(ctx).await {
        Ok(client) => whoami(&client).await.ok(),
        Err(_) => None,
    };
    tools_for(me.as_ref(), ctx.default_agent.as_deref())
}

/// The set for a caller the host described as `me`. Unknown (the host was
/// unreachable) reads as "may not spawn": never offer a spawn the host may
/// refuse for a reason the agent cannot see.
fn tools_for(me: Option<&WhoamiWire>, default_agent: Option<&str>) -> Vec<Value> {
    let mut tools = Vec::new();
    if let Some(me) = me.filter(|me| me.spawn_allowed) {
        tools.push(spawn_tool(me, default_agent));
    }
    tools.extend(fixed_tools());
    tools
}

fn spawn_tool(me: &WhoamiWire, default_agent: Option<&str>) -> Value {
    let installed: Vec<&str> = me.agents.iter().filter(|a| a.installed).map(|a| a.agent_id.as_str()).collect();
    let mut description = String::from(
        "Start a helper agent on a self-contained task. It runs beside you in its own tab and its \
         events reach you when it finishes or gets stuck. ",
    );
    if !installed.is_empty() {
        description.push_str(&format!("Agents: {}. ", installed.join(", ")));
    }
    if let Some(agent) = default_agent {
        description.push_str(&format!("Default agent: {agent}. "));
    }
    description.push_str(ETIQUETTE);
    let mut agent = json!({ "type": "string", "description": "Which agent to start." });
    if !installed.is_empty() {
        agent["enum"] = json!(installed);
    }
    let mut required = vec!["prompt", "request_id"];
    if default_agent.is_none() {
        required.push("agent");
    }
    tool(
        "oximux_spawn_agent",
        &description,
        json!({
            "prompt": { "type": "string", "description": "The task, complete enough to work on alone." },
            "request_id": { "type": "string", "description": "A key you choose. Repeating a call with the same key returns the same child instead of starting another." },
            "agent": agent,
            "model": { "type": "string" },
            "posture": { "type": "string", "enum": ["default", "review", "verify"], "default": "default", "description": "review: reads only. verify: reads and runs tests, no edits." },
            "notify_parent": { "type": "string", "enum": ["auto", "stuck", "off"], "default": "auto", "description": "Which of its events reach you: all, only when stuck, or none." },
            "allow_parent_permits": { "type": "boolean", "default": false, "description": "Let you answer its read-only permission prompts (oximux_answer_permit)." },
            "cwd": { "type": "string", "default": "inherit", "description": "inherit, worktree, worktree:NAME, or a path inside your directory." }
        }),
        &required,
    )
}

fn fixed_tools() -> Vec<Value> {
    vec![
        tool(
            "oximux_wait_agents",
            &format!(
                "Wait until the children you name finish or need something, up to timeout_ms (at most 60000). \
                 until=all waits for every one, until=any for the first. {WAIT_GUIDANCE} {ETIQUETTE}"
            ),
            json!({
                "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": MAX_WAIT_IDS },
                "until": { "type": "string", "enum": ["all", "any"], "default": "all" },
                "timeout_ms": { "type": "integer", "default": MAX_WAIT_MS, "maximum": MAX_WAIT_MS }
            }),
            &["ids"],
        ),
        tool(
            "oximux_send_agent",
            "Send a child a message. It queues behind the child's running turn; sending to a finished child starts it again.",
            json!({
                "id": { "type": "string" },
                "text": { "type": "string" },
                "mode": { "type": "string", "enum": ["queue", "steer"], "default": "queue", "description": "steer: into the running turn, when the child supports it." }
            }),
            &["id", "text"],
        ),
        tool(
            "oximux_read_agent",
            "A child's conversation, the last `tail` entries (default 20).",
            json!({ "id": { "type": "string" }, "tail": { "type": "integer", "default": READ_TAIL_DEFAULT, "maximum": READ_TAIL_MAX } }),
            &["id"],
        ),
        tool(
            "oximux_cancel_agent",
            "Stop a child for good. cascade (default true) stops the agents it started too.",
            json!({ "id": { "type": "string" }, "cascade": { "type": "boolean", "default": true } }),
            &["id"],
        ),
        tool(
            "oximux_list_children",
            "Your children and where each stands. recursive includes their children.",
            json!({ "recursive": { "type": "boolean", "default": false } }),
            &[],
        ),
        tool(
            "oximux_answer_permit",
            "Answer a child's permission prompt, when you started it with allow_parent_permits: deny it, or allow it unchanged. Prompts to run commands or change files go to the user.",
            json!({
                "id": { "type": "string" },
                "request_id": { "type": "string" },
                "decision": { "type": "string", "enum": ["allow", "deny"] },
                "message": { "type": "string", "description": "Why, for a deny." }
            }),
            &["id", "request_id", "decision"],
        ),
        tool("oximux_state_get", "Read a shared state key (version 0 when unset).", json!({ "key": { "type": "string" } }), &["key"]),
        tool(
            "oximux_state_cas",
            "Write a shared state key only if it is still at if_version (0: only if unset). A conflict returns the current entry.",
            json!({ "key": { "type": "string" }, "value": { "description": "Any JSON value." }, "if_version": { "type": "integer" } }),
            &["key", "value", "if_version"],
        ),
    ]
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": properties, "required": required } })
}

/// `tools/call` for the delegation set.
pub(super) async fn call(ctx: &Ctx, name: &str, args: Value) -> Value {
    let budget = if name == "oximux_wait_agents" { wait_timeout(&args) + WAIT_SLACK } else { CALL_BUDGET };
    let outcome = match tokio::time::timeout(budget, run(ctx, name, &args)).await {
        Ok(outcome) => outcome,
        Err(_) => Err(format!("stopped waiting after {} s, this call's limit; nothing was undone", budget.as_secs())),
    };
    match outcome {
        Ok(text) => json!({ "content": [text_item(&text)], "isError": false }),
        Err(text) => json!({ "content": [text_item(&text)], "isError": true }),
    }
}

async fn run(ctx: &Ctx, name: &str, args: &Value) -> Result<String, String> {
    let client = connect(ctx).await?;
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned);
    let required = |key: &str| text(key).filter(|v| !v.trim().is_empty()).ok_or(format!("`{key}` is required"));
    let flag = |key: &str, default: bool| args.get(key).and_then(Value::as_bool).unwrap_or(default);
    let answer = |(data, _human): (Value, String)| pretty(&data);
    match name {
        "oximux_spawn_agent" => {
            let agent = text("agent").or_else(|| ctx.default_agent.clone()).ok_or("`agent` is required")?;
            let spawn = delegate::SpawnArgs {
                agent,
                model: text("model"),
                posture: match text("posture").as_deref() {
                    None | Some("default") => PostureArg::Default,
                    Some("review") => PostureArg::Review,
                    Some("verify") => PostureArg::Verify,
                    Some(other) => return Err(format!("unknown posture `{other}`")),
                },
                notify: match text("notify_parent").as_deref() {
                    None | Some("auto") => NotifyArg::Auto,
                    Some("stuck") => NotifyArg::Stuck,
                    Some("off") => NotifyArg::Off,
                    Some(other) => return Err(format!("unknown notify_parent `{other}`")),
                },
                allow_parent_permits: flag("allow_parent_permits", false),
                cwd: text("cwd").unwrap_or_else(|| "inherit".into()),
                parent: None,
                request_id: Some(required("request_id")?),
                prompt: vec![required("prompt")?],
            };
            delegate::spawn(&client, spawn).await.map(answer).map_err(|f| failure_text(&f))
        }
        "oximux_wait_agents" => wait_agents(ctx, &client, args).await,
        "oximux_send_agent" => {
            let id = required("id")?;
            send::run(&client, &id, &required("text")?, true, None, None, true)
                .await
                .map(answer)
                .map_err(|f| failure_text(&f))
        }
        "oximux_read_agent" => {
            let tail = args.get("tail").and_then(Value::as_u64).unwrap_or(READ_TAIL_DEFAULT).clamp(1, READ_TAIL_MAX);
            read_agent(&client, &required("id")?, tail as usize).await.map_err(|f| failure_text(&f))
        }
        "oximux_cancel_agent" => {
            delegate::cancel(&client, &required("id")?, flag("cascade", true)).await.map(answer).map_err(|f| failure_text(&f))
        }
        "oximux_list_children" => {
            delegate::children(&client, None, flag("recursive", false)).await.map(answer).map_err(|f| failure_text(&f))
        }
        "oximux_answer_permit" => {
            let (id, request) = (required("id")?, required("request_id")?);
            let outcome = match text("decision").as_deref() {
                Some("allow") => permit::allow(&client, &id, Some(&request), None).await,
                Some("deny") => {
                    let message = text("message").unwrap_or_else(|| "Denied by the agent that started this one.".into());
                    permit::deny(&client, &id, Some(&request), &message).await
                }
                _ => return Err("`decision` is allow or deny".into()),
            };
            outcome.map(answer).map_err(|f| failure_text(&f))
        }
        "oximux_state_get" => state::get(&client, &required("key")?).await.map(answer).map_err(|f| failure_text(&f)),
        "oximux_state_cas" => {
            let key = required("key")?;
            let value = args.get("value").ok_or("`value` is required")?.to_string();
            let version = args.get("if_version").and_then(Value::as_u64).ok_or("`if_version` is required (0: only if unset)")?;
            state::set(&client, &key, &value, Some(version)).await.map(answer).map_err(|f| failure_text(&f))
        }
        other => Err(format!("no tool `{other}`")),
    }
}

async fn connect(ctx: &Ctx) -> Result<Client, String> {
    let client = Client::connect_session(ctx.dir.clone(), CALL_BUDGET.as_secs(), ctx.credential_file.as_deref())
        .await
        .map_err(|f| failure_text(&f))?;
    client.require_version(SPAWN_MIN_VERSION, "oximux mcp --tools orchestrate").map_err(|f| failure_text(&f))?;
    Ok(client)
}

async fn whoami(client: &Client) -> Result<WhoamiWire, Failure> {
    match client.call(Request::Whoami).await? {
        Response::Whoami(me) => Ok(me),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("Whoami", &other)),
    }
}

fn pretty(data: &Value) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
}

// ── wait ────────────────────────────────────────────────────────────────────

fn wait_timeout(args: &Value) -> Duration {
    Duration::from_millis(args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(MAX_WAIT_MS).clamp(1_000, MAX_WAIT_MS))
}

/// Where one child stands at the end of a wait.
#[derive(Debug, Clone, PartialEq)]
struct Outcome {
    state: &'static str,
    /// Its turn is over (it will not change without a new message).
    terminal: bool,
    /// Nothing more will happen without someone acting (a terminal state, or
    /// a decision it waits on).
    settled: bool,
}

impl Outcome {
    const STARTING: Outcome = Outcome { state: "starting", terminal: false, settled: false };
    const RUNNING: Outcome = Outcome { state: "running", terminal: false, settled: false };
    const FINISHED: Outcome = Outcome { state: "finished", terminal: true, settled: true };
    const FAILED: Outcome = Outcome { state: "failed", terminal: true, settled: true };
    const NEEDS_DECISION: Outcome = Outcome { state: "needs_decision", terminal: false, settled: true };
    const IDLE: Outcome = Outcome { state: "idle", terminal: true, settled: true };
    const ENDED: Outcome = Outcome { state: "ended", terminal: true, settled: true };
}

async fn wait_agents(ctx: &Ctx, client: &Client, args: &Value) -> Result<String, String> {
    let mut ids: Vec<String> = Vec::new();
    for id in args.get("ids").and_then(Value::as_array).ok_or("`ids` is a list of child ids")? {
        let id = id.as_str().ok_or("`ids` holds strings")?.to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() || ids.len() > MAX_WAIT_IDS {
        return Err(format!("`ids` names 1 to {MAX_WAIT_IDS} children"));
    }
    let any = match args.get("until").and_then(Value::as_str) {
        None | Some("all") => false,
        Some("any") => true,
        Some(other) => return Err(format!("`until` is all or any, not `{other}`")),
    };
    let deadline = Instant::now() + wait_timeout(args);
    let rows = lineage(client).await.map_err(|f| failure_text(&f))?;
    if let Some(stranger) = ids.iter().find(|id| !rows.contains_key(*id)) {
        return Err(format!("{stranger} is not an agent you started (oximux_list_children lists them)"));
    }
    let mut outcomes: HashMap<String, Outcome> = HashMap::new();
    let mut watching = FuturesUnordered::new();
    for id in &ids {
        match from_lineage(rows[id].state) {
            Some(outcome) => drop(outcomes.insert(id.clone(), outcome)),
            None => watching.push(async move { (id.clone(), settle(ctx, id, deadline).await) }),
        }
    }
    let done = |outcomes: &HashMap<String, Outcome>| {
        let settled = outcomes.values().filter(|o| o.settled).count();
        if any { settled > 0 } else { settled == ids.len() }
    };
    while !done(&outcomes) {
        match tokio::time::timeout_at(deadline, watching.next()).await {
            Ok(Some((id, outcome))) => drop(outcomes.insert(id, outcome)),
            Ok(None) | Err(_) => break,
        }
    }
    drop(watching);
    let mut children = Vec::new();
    for id in &ids {
        let outcome = outcomes.get(id).cloned().unwrap_or(Outcome::RUNNING);
        children.push(child_report(client, id, rows[id].state, &outcome).await);
    }
    let settled = outcomes.values().filter(|o| o.settled).count();
    Ok(pretty(&json!({
        "children": children,
        "settled": settled,
        "waiting": ids.len() - settled,
        "timed_out": !done(&outcomes),
    })))
}

/// The caller's subtree, by child id.
async fn lineage(client: &Client) -> Result<HashMap<String, LineageWire>, Failure> {
    let own = whoami(client).await?.session.unwrap_or_default();
    match client.call(Request::ListChildren { of: own, recursive: true }).await? {
        Response::Children(rows) => Ok(rows.into_iter().map(|r| (r.session_id.clone(), r)).collect()),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("ListChildren", &other)),
    }
}

/// A parked or ended child is answered from its lineage row: reading its
/// session would build it (start an agent process) just to say it is idle.
fn from_lineage(state: LineageStateWire) -> Option<Outcome> {
    match state {
        LineageStateWire::Parked => Some(Outcome::IDLE),
        LineageStateWire::Ended => Some(Outcome::ENDED),
        LineageStateWire::Reserved | LineageStateWire::Active => None,
    }
}

/// Watch one working child until its turn ends, it waits on a decision, or
/// the deadline. Its own connection: a subscription is per connection.
async fn settle(ctx: &Ctx, id: &str, deadline: Instant) -> Outcome {
    let Ok(client) = connect(ctx).await else { return Outcome::RUNNING };
    // A child still launching has no session to read yet.
    let info = loop {
        match client.call(Request::GetSessionInfo { session_id: id.into() }).await {
            Ok(Response::SessionInfo(info)) => break info,
            Ok(Response::Error(RpcError::UnknownSession)) => {
                if let Ok(rows) = lineage(&client).await
                    && let Some(outcome) = rows.get(id).and_then(|r| from_lineage(r.state))
                {
                    return outcome;
                }
                if Instant::now() + START_POLL >= deadline {
                    return Outcome::STARTING;
                }
                tokio::time::sleep(START_POLL).await;
            }
            _ => return Outcome::RUNNING,
        }
    };
    if info.summary.awaiting_permission {
        return Outcome::NEEDS_DECISION;
    }
    let Ok(Response::Events(backlog)) =
        client.call(Request::EventsSince { session_id: id.into(), after_seq: 0 }).await
    else {
        return Outcome::RUNNING;
    };
    let (ended, last_seq) = scan(&backlog);
    if let Some(is_error) = ended {
        return if is_error { Outcome::FAILED } else { Outcome::FINISHED };
    }
    let opts = StreamOpts { from: Some(last_seq), json_mode: true, quiet: true, stop: Stop::Settled, deadline: Some(deadline), stall_after: None };
    match stream_session(&client, id, opts).await {
        Ok(StreamEnd::TurnEnded { is_error: false }) => Outcome::FINISHED,
        Ok(StreamEnd::TurnEnded { is_error: true }) => Outcome::FAILED,
        Ok(StreamEnd::NeedsApproval) => Outcome::NEEDS_DECISION,
        _ => Outcome::RUNNING,
    }
}

/// Whether the retained events show the latest turn over (`Some(is_error)`),
/// and the newest sequence number. A child with no turn yet is not over: it
/// has not received its prompt.
fn scan(frames: &[HostEvent]) -> (Option<bool>, u64) {
    let (mut last_user, mut last_end, mut last_seq) = (None, None, 0);
    for frame in frames {
        last_seq = frame.seq;
        match frame.event() {
            Ok(ThreadEvent::UserMessage { .. }) => last_user = Some(frame.seq),
            Ok(ThreadEvent::TurnEnded { is_error, .. }) => last_end = Some((frame.seq, is_error)),
            _ => {}
        }
    }
    let over = match (last_user, last_end) {
        (Some(user), Some((end, is_error))) if end > user => Some(is_error),
        (None, Some((_, is_error))) => Some(is_error),
        _ => None,
    };
    (over, last_seq)
}

async fn child_report(client: &Client, id: &str, lineage: LineageStateWire, outcome: &Outcome) -> Value {
    let mut report = json!({
        "id": id,
        "state": outcome.state,
        "lineage_state": lineage_text(lineage),
        "terminal": outcome.terminal,
    });
    if outcome.terminal
        && let Ok(t) = transcript::fetch_entries(client, id).await
        && let Some(last) = t.entries.iter().rev().find_map(assistant_text)
    {
        let (kept, cut) = truncate_chars(last, SUMMARY_LIMIT);
        report["summary"] = json!(kept);
        if cut > 0 {
            report["summary_truncated"] = json!(cut);
            report["more"] = json!(format!("oximux_read_agent id={id}"));
        }
    }
    if *outcome == Outcome::FAILED {
        report["error"] = json!("its last turn ended with an error");
    }
    if *outcome == Outcome::NEEDS_DECISION
        && let Ok((pending, _)) = permit::ls(client, id).await
    {
        report["pending_permit"] = pending;
    }
    report
}

fn lineage_text(state: LineageStateWire) -> &'static str {
    match state {
        LineageStateWire::Reserved => "reserved",
        LineageStateWire::Active => "active",
        LineageStateWire::Parked => "parked",
        LineageStateWire::Ended => "ended",
    }
}

fn assistant_text(entry: &Value) -> Option<&str> {
    entry.pointer("/Assistant/text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty())
}

/// The first `limit` chars of `text`, and how many were cut.
fn truncate_chars(text: &str, limit: usize) -> (&str, usize) {
    match text.char_indices().nth(limit) {
        Some((at, _)) => (&text[..at], text[at..].chars().count()),
        None => (text, 0),
    }
}

async fn read_agent(client: &Client, id: &str, tail: usize) -> Result<String, Failure> {
    let t = transcript::fetch_entries(client, id).await?;
    let lines: Vec<String> = t.entries.iter().filter_map(crate::render::render_entry).collect();
    let skip = lines.len().saturating_sub(tail);
    let mut out = String::new();
    if skip > 0 {
        out.push_str(&format!("({skip} earlier entries not shown; raise tail to see them)\n"));
    }
    out.push_str(&lines[skip..].join("\n"));
    if out.is_empty() {
        out.push_str("(nothing yet)");
    }
    Ok(out)
}

#[cfg(test)]
#[path = "orchestrate_tests.rs"]
mod tests;
