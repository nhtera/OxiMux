//! Minimal newline-delimited JSON-RPC client over `codex app-server`'s stdio.
//!
//! One `\n`-terminated JSON value per message. Three inbound kinds:
//! - **response** (`id` + `result`/`error`, no `method`) → routed to the pending
//!   request that carries that `id`;
//! - **notification** (`method`, no `id`) → forwarded on the [`Inbound`] channel;
//! - **server → client request** (`method` + `id`) → forwarded on [`Inbound`] for
//!   the actor to answer (the approval RPCs).
//!
//! Outgoing requests are `{id, method, params}` (Codex's `ClientRequest` shape has
//! no `jsonrpc` field). A dedicated reader thread owns stdout; callers write via
//! the shared stdin, so a blocking `request()` never deadlocks the reader.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::ops::ControlFlow;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

/// A message the reader could not answer itself, handed to the actor.
pub enum Inbound {
    /// A server → client notification (`method`, no `id`).
    Notification { method: String, params: Value },
    /// A server → client request (`method` + `id`) that expects a `result` back.
    ServerRequest { id: Value, method: String, params: Value },
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

/// A cloneable handle to the running `codex app-server` stdin + pending-request
/// registry. Cloning shares the same child (all fields are `Arc`).
#[derive(Clone)]
pub struct RpcClient {
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    /// Cleared by the reader thread when the child's stdout hits EOF (the
    /// process exited). The actor's worker polls this so it stops parking on its
    /// command channel and drops its event sender when the subprocess dies —
    /// otherwise a crash would leave the chat frozen (the `Receiver` never
    /// closes, so the app's disconnect handler never fires).
    alive: Arc<AtomicBool>,
}

impl RpcClient {
    /// Spawn `codex app-server` in `cwd`, start the reader thread, and return the
    /// client, the [`Inbound`] receiver, and the child (for reaping).
    pub fn spawn(cwd: &Path) -> Result<(RpcClient, Receiver<Inbound>, Child)> {
        let mut cmd = Command::new(crate::cli::program_for_spawn("codex"));
        cmd.arg("app-server")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::spawn_command(cmd)
    }

    /// Spawn an already-built command (the real `codex`, or a fake in tests) and
    /// wire its stdout into the reader/router.
    pub fn spawn_command(mut cmd: Command) -> Result<(RpcClient, Receiver<Inbound>, Child)> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Put codex in its own process group so teardown can kill the whole
            // tree — its `workspace-write` sandbox spawns helper processes for
            // tool execution that would otherwise be orphaned.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().context("spawn codex app-server")?;
        let stdout = child.stdout.take().context("codex stdout missing")?;
        let stdin = child.stdin.take().context("codex stdin missing")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));

        let pending_r = pending.clone();
        let alive_r = alive.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break }; // read error → EOF
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
                    // Codex may print non-JSON diagnostics; skip, don't crash.
                    tracing::debug!(line = %trimmed, "codex: skipping non-JSON stdout line");
                    continue;
                };
                if route(msg, &pending_r, &inbound_tx).is_break() {
                    break; // consumer gone — still run the exit cleanup below
                }
            }
            // stdout closed (EOF / process exit) or the consumer went away. Mark
            // dead and fail every outstanding request so blocked `request()`
            // callers unblock immediately instead of waiting out their timeout;
            // `inbound_tx` drops here too, ending the mapper.
            alive_r.store(false, Ordering::SeqCst);
            if let Ok(mut p) = pending_r.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err("codex app-server exited".to_string()));
                }
            }
        });

        Ok((
            RpcClient {
                stdin: Arc::new(Mutex::new(stdin)),
                next_id: Arc::new(AtomicU64::new(1)),
                pending,
                alive,
            },
            inbound_rx,
            child,
        ))
    }

    /// Whether the `codex app-server` child is still running (its stdout hasn't
    /// hit EOF). The actor's worker polls this to notice a crash.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send a request and block (up to `timeout`) for its response.
    pub fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("codex pending map poisoned"))?
            .insert(id, tx);
        if let Err(e) = self.write_msg(&json!({"id": id, "method": method, "params": params})) {
            self.pending.lock().ok().and_then(|mut p| p.remove(&id));
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("codex {method} error: {e}")),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut p| p.remove(&id));
                Err(anyhow!("codex {method} timed out"))
            }
        }
    }

    /// Send a request without waiting for its response (fire-and-forget) — used
    /// for `turn/interrupt`, where the caller only needs the side effect.
    pub fn fire(&self, method: &str, params: Value) -> Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.write_msg(&json!({"id": id, "method": method, "params": params}))
    }

    /// Send a notification (no `id`). Omits `params` entirely when `Null` (Codex's
    /// `initialized` notification carries no params).
    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = if params.is_null() {
            json!({"method": method})
        } else {
            json!({"method": method, "params": params})
        };
        self.write_msg(&msg)
    }

    /// Answer a server → client request by echoing its `id` with a `result`.
    pub fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.write_msg(&json!({"id": id, "result": result}))
    }

    fn write_msg(&self, v: &Value) -> Result<()> {
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| anyhow!("codex stdin lock poisoned"))?;
        let line = serde_json::to_string(v).context("serialize codex message")?;
        stdin.write_all(line.as_bytes()).context("write codex stdin")?;
        stdin.write_all(b"\n").context("write codex newline")?;
        stdin.flush().context("flush codex stdin")?;
        Ok(())
    }
}

/// Classify one inbound message and route it. Returns `Break` when the inbound
/// consumer is gone (so the reader stops).
fn route(msg: Value, pending: &Pending, inbound_tx: &Sender<Inbound>) -> ControlFlow<()> {
    let has_method = msg.get("method").and_then(|m| m.as_str()).is_some();
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();

    if !has_method {
        // Response: id + result/error.
        if let Some(uid) = id.as_ref().and_then(Value::as_u64) {
            let outcome = match msg.get("error") {
                Some(err) => Err(err.to_string()),
                None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            if let Ok(mut p) = pending.lock()
                && let Some(tx) = p.remove(&uid)
            {
                let _ = tx.send(outcome);
            }
        }
        return ControlFlow::Continue(());
    }

    let method = msg["method"].as_str().unwrap_or_default().to_string();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let out = match id {
        Some(id) => inbound_tx.send(Inbound::ServerRequest { id, method, params }),
        None => inbound_tx.send(Inbound::Notification { method, params }),
    };
    if out.is_err() {
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake "app-server" that, for any request, echoes a response with the same
    /// id and also emits one notification — proving id-correlation + inbound
    /// routing without a real `codex`.
    #[test]
    fn request_correlates_by_id_and_notifications_route() {
        // sh script: read one line (the request), reply with a matching response
        // then a notification.
        let script = r#"
read line
printf '{"id":1,"result":{"ok":true}}\n'
printf '{"method":"turn/started","params":{"turn":{"id":"t1"}}}\n'
sleep 0.2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, inbound, _child) = RpcClient::spawn_command(cmd).expect("spawn fake");
        let res = rpc
            .request("initialize", json!({}), Duration::from_secs(5))
            .expect("response");
        assert_eq!(res["ok"], true);
        // the notification is delivered on the inbound channel
        match inbound.recv_timeout(Duration::from_secs(5)) {
            Ok(Inbound::Notification { method, params }) => {
                assert_eq!(method, "turn/started");
                assert_eq!(params["turn"]["id"], "t1");
            }
            other => panic!("expected a notification, got {}", other.is_ok()),
        }
    }

    #[test]
    fn child_exit_marks_dead_and_fails_pending_fast() {
        // A fake that reads the request then exits WITHOUT responding. The
        // pending request must fail via the EOF drain (fast) rather than waiting
        // out its timeout, and is_alive() must flip false.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("read line; exit 0");
        let (rpc, _inbound, mut child) = RpcClient::spawn_command(cmd).expect("spawn fake");
        assert!(rpc.is_alive());
        let start = std::time::Instant::now();
        let res = rpc.request("x", json!({}), Duration::from_secs(30));
        assert!(res.is_err(), "request must fail once the child exits");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "should fail fast via EOF drain, not wait out the 30s timeout"
        );
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!rpc.is_alive(), "connection must be marked dead after child exit");
    }

    #[test]
    fn non_json_lines_are_skipped_not_fatal() {
        let script = r#"
read line
printf 'not json at all\n'
printf '{"id":1,"result":42}\n'
sleep 0.2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = RpcClient::spawn_command(cmd).expect("spawn fake");
        let res = rpc
            .request("x", json!({}), Duration::from_secs(5))
            .expect("response after a non-JSON line");
        assert_eq!(res, json!(42));
    }
}
