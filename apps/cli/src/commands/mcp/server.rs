//! The JSON-RPC side of `oximux mcp`: one message per line on stdin, one per
//! line on stdout (the MCP stdio transport). `initialize`, `ping`,
//! `tools/list` and `tools/call` are answered; a tool call runs in a task of
//! its own (so `ping` stays answered, and `notifications/cancelled` drops it,
//! which closes its connection and so its verb on the host).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Ctx, orchestrate, tools};

/// The MCP revisions this server speaks, newest first. A client asking for
/// another gets the newest (the spec's negotiation).
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Drives the iOS simulator or Android device attached to one OxiMux worktree. \
Call sim_observe to see the screen as a list of @eN refs, then sim_act to tap, type or swipe them; each step reports what changed. \
The first call asks the user in OxiMux: then call sim_wait_consent and retry.";

pub(super) async fn serve(ctx: Ctx) {
    let ctx = Arc::new(ctx);
    let (out, mut outbox) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = outbox.recv().await {
            let mut line = message.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let calls: Arc<Mutex<HashMap<String, JoinHandle<()>>>> = Arc::default();
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdin.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A bad line is answered and skipped, never the end of the server.
        let Ok(text) = std::str::from_utf8(&line) else {
            let _ = out.send(error(Value::Null, -32700, "not UTF-8"));
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(text) {
            // Batches left the protocol in 2025-06-18; no client of ours sends one.
            Ok(Value::Array(_)) => drop(out.send(error(Value::Null, -32600, "batches are not supported"))),
            Ok(message) => handle(message, &ctx, &out, &calls),
            Err(e) => drop(out.send(error(Value::Null, -32700, &format!("not JSON: {e}")))),
        }
    }
    // The client closed stdin: it is gone, and so are its calls.
    for (_, call) in calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).drain() {
        call.abort();
    }
    drop(out);
    let _ = writer.await;
}

fn handle(message: Value, ctx: &Arc<Ctx>, out: &mpsc::UnboundedSender<Value>, calls: &Arc<Mutex<HashMap<String, JoinHandle<()>>>>) {
    let Some(method) = message.get("method").and_then(Value::as_str).map(str::to_owned) else {
        // A response to something the server never asks: nothing to do.
        return;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = message.get("id").cloned() else {
        // A notification: only a cancel needs doing.
        if method == "notifications/cancelled"
            && let Some(request) = params.get("requestId")
            && let Some(call) = calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&request.to_string())
        {
            call.abort();
        }
        return;
    };
    let reply = match method.as_str() {
        "initialize" => initialize(&params, ctx),
        "ping" => json!({}),
        "tools/list" => {
            // The delegation tools depend on what the host says the caller may
            // do right now, which takes a round trip: answered from a task.
            let (ctx, out) = (ctx.clone(), out.clone());
            tokio::spawn(async move {
                let tools = list(&ctx).await;
                let _ = out.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools } }));
            });
            return;
        }
        "tools/call" => {
            let (ctx, out, key, calls_for_task) = (ctx.clone(), out.clone(), id.to_string(), calls.clone());
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let mut running = calls.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if running.contains_key(&key) {
                let _ = out.send(error(id, -32600, "a call with this id is still running"));
                return;
            }
            let task_key = key.clone();
            running.insert(
                key,
                tokio::spawn(async move {
                    let result = call(&ctx, &name, args).await;
                    calls_for_task.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&task_key);
                    let _ = out.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                }),
            );
            return;
        }
        other => {
            let _ = out.send(error(id, -32601, &format!("no method `{other}`")));
            return;
        }
    };
    let _ = out.send(json!({ "jsonrpc": "2.0", "id": id, "result": reply }));
}

/// Every tool this server offers right now.
async fn list(ctx: &Ctx) -> Value {
    let mut all = Vec::new();
    if ctx.sim()
        && let Value::Array(sim) = tools::list()
    {
        all.extend(sim);
    }
    if ctx.orchestrate {
        all.extend(orchestrate::list(ctx).await);
    }
    Value::Array(all)
}

/// Route a call to its tool set. A tool from a set this server does not
/// serve does not exist.
async fn call(ctx: &Ctx, name: &str, args: Value) -> Value {
    if ctx.orchestrate && name.starts_with(orchestrate::PREFIX) {
        return orchestrate::call(ctx, name, args).await;
    }
    if ctx.sim() && name.starts_with("sim_") {
        return tools::call(ctx, name, args).await;
    }
    json!({ "content": [tools::text_item(&format!("no tool `{name}`"))], "isError": true })
}

fn initialize(params: &Value, ctx: &Ctx) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
    let instructions = [ctx.sim().then_some(INSTRUCTIONS), ctx.orchestrate.then_some(orchestrate::INSTRUCTIONS)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "oximux", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions,
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(sim: bool, orchestrate: bool) -> Ctx {
        Ctx {
            dir: None,
            worktree: sim.then(|| "/tmp/repo".into()),
            orchestrate,
            credential_file: None,
            default_agent: None,
        }
    }

    #[test]
    fn the_protocol_version_is_negotiated() {
        let sim = ctx(true, false);
        assert_eq!(initialize(&json!({ "protocolVersion": "2025-06-18" }), &sim)["protocolVersion"], "2025-06-18");
        assert_eq!(initialize(&json!({ "protocolVersion": "1999-01-01" }), &sim)["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(initialize(&json!({}), &sim)["capabilities"], json!({ "tools": {} }));
    }

    #[test]
    fn the_instructions_describe_only_the_sets_served() {
        let text = |c: Ctx| initialize(&json!({}), &c)["instructions"].as_str().unwrap().to_string();
        assert_eq!(text(ctx(true, false)), INSTRUCTIONS);
        assert_eq!(text(ctx(false, true)), orchestrate::INSTRUCTIONS);
        assert!(text(ctx(true, true)).contains(orchestrate::INSTRUCTIONS));
    }

    /// An outside client (the `sim` set alone) is never offered a delegation
    /// tool, and cannot call one by name.
    #[tokio::test]
    async fn the_sim_set_alone_has_no_delegation_tools() {
        let sim = ctx(true, false);
        let names: Vec<String> = list(&sim)
            .await
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.is_empty());
        assert!(names.iter().all(|n| n.starts_with("sim_")), "{names:?}");
        let refused = call(&sim, "oximux_spawn_agent", json!({})).await;
        assert_eq!(refused["isError"], true);
        assert!(refused["content"][0]["text"].as_str().unwrap().contains("no tool"));
    }
}
