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

use super::{Ctx, tools};

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
        "initialize" => initialize(&params),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::list() }),
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
                    let result = tools::call(&ctx, &name, args).await;
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

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "oximux", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_protocol_version_is_negotiated() {
        assert_eq!(initialize(&json!({ "protocolVersion": "2025-06-18" }))["protocolVersion"], "2025-06-18");
        assert_eq!(initialize(&json!({ "protocolVersion": "1999-01-01" }))["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(initialize(&json!({}))["capabilities"], json!({ "tools": {} }));
    }
}
