//! Newline-JSON client over `pi --mode rpc`'s stdio.
//!
//! A dedicated reader thread owns stdout and routes each line: responses go to
//! the pending request that carries their `id` (pi echoes ours back verbatim),
//! everything else is broadcast on the [`Inbound`] channel. Callers write via the
//! shared stdin, so a blocking `request()` never deadlocks the reader.
//!
//! **Framing is LF-only, deliberately.** pi's `jsonl.ts` warns that payload
//! strings may contain U+2028/U+2029 and that clients must split on `\n` alone —
//! pi refuses Node's `readline` for exactly this reason. So this reads via
//! `read_until(b'\n')` rather than `BufRead::lines()`: `lines()` also strips a
//! trailing `\r`, which would corrupt a payload byte.
//!
//! Modelled on `thread/codex/transport.rs` (same subprocess + newline-JSON
//! shape) and on pi's own two reference clients — `modes/rpc/rpc-client.ts` and
//! `packages/orchestrator/src/rpc-process.ts`. The stderr-in-the-error detail
//! below is theirs.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::protocol::{classify, Inbound, PiCommand, RpcResponse};

type Pending = Arc<Mutex<HashMap<String, Sender<Result<RpcResponse, String>>>>>;

/// How long the exit path waits for stderr to finish after stdout hits EOF.
/// Generous relative to "the process is already exiting", tight enough that a
/// child holding stderr open can't stall the drain that unblocks the UI.
const STDERR_SETTLE: Duration = Duration::from_millis(250);

/// A cloneable handle to a running `pi --mode rpc` child's stdin + pending
/// registry. Cloning shares the same child (all fields are `Arc`).
#[derive(Clone)]
pub struct PiRpcClient {
    /// `None` once closed. Held as an `Option` specifically so [`Self::close_stdin`]
    /// can drop the pipe: pi treats stdin EOF as "no more commands are coming",
    /// and a shared handle that merely flushes would never deliver that.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    /// Cleared by the reader thread on stdout EOF (the process exited). The
    /// worker polls this so a crash ends the session instead of freezing the chat.
    alive: Arc<AtomicBool>,
    /// pi's stderr, accumulated. Folded into the error when the process dies —
    /// a bare "pi exited" is useless for diagnosing bad auth or a bad flag, and
    /// pi's own client attaches stderr for the same reason.
    stderr: Arc<Mutex<String>>,
}

impl PiRpcClient {
    /// Spawn an already-built command (the real `pi`, or a fake in tests) and
    /// wire its stdout into the reader/router.
    pub fn spawn_command(mut cmd: Command) -> Result<(PiRpcClient, Receiver<Inbound>, Child)> {
        use oximux_no_window::NoWindow as _;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).no_window();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group so teardown can reach the whole tree. pi's bash
            // tool spawns detached children; SIGTERM to pi runs its handler
            // (which reaps them), and the group is the backstop for the rest.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().context("spawn pi --mode rpc")?;
        let stdout = child.stdout.take().context("pi stdout missing")?;
        let stdin = child.stdin.take().context("pi stdin missing")?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        // Set once stderr hits EOF, so the exit path can tell "pi printed
        // nothing" apart from "we haven't read it yet".
        let stderr_done = Arc::new(AtomicBool::new(false));
        if let Some(mut err) = child.stderr.take() {
            let sink = stderr_buf.clone();
            let done = stderr_done.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut s) = sink.lock() {
                        s.push_str(&String::from_utf8_lossy(&buf[..n]));
                        // Bound it: a chatty agent must not grow this forever.
                        if s.len() > 8192 {
                            let tail = s[s.len() - 4096..].to_string();
                            *s = tail;
                        }
                    }
                }
                done.store(true, Ordering::SeqCst);
            });
        } else {
            stderr_done.store(true, Ordering::SeqCst);
        }

        let pending_r = pending.clone();
        let alive_r = alive.clone();
        let stderr_r = stderr_buf.clone();
        let stderr_done_r = stderr_done.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                // LF-only: never `lines()` (it also strips `\r`).
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break, // EOF or read error
                    Ok(_) => {}
                }
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else {
                    // pi may print non-JSON diagnostics; skip, don't crash.
                    tracing::debug!(
                        line = %String::from_utf8_lossy(&line),
                        "pi: skipping non-JSON stdout line"
                    );
                    continue;
                };
                match classify(v) {
                    Inbound::Response(r) => {
                        let Some(id) = r.id.clone() else {
                            // A response with no id can't be correlated; surface
                            // it rather than dropping it silently.
                            let _ = inbound_tx.send(Inbound::Response(r));
                            continue;
                        };
                        let tx = pending_r.lock().ok().and_then(|mut p| p.remove(&id));
                        match tx {
                            Some(tx) => {
                                let _ = tx.send(Ok(r));
                            }
                            // Unsolicited / late response — hand to the mapper.
                            None => {
                                if inbound_tx.send(Inbound::Response(r)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    other => {
                        if inbound_tx.send(other).is_err() {
                            break; // consumer gone — still run the exit cleanup
                        }
                    }
                }
            }
            // stdout closed (EOF / exit) or the consumer went away. Fail every
            // outstanding request so blocked callers unblock now rather than
            // waiting out a timeout — pi dying during the `get_state` handshake
            // would otherwise hang a new chat forever.
            //
            // First give stderr a moment to finish: stdout EOF means the process
            // is going away, so its stderr EOF follows almost immediately, and
            // the whole value of this error is *why* pi died. Bounded, because a
            // process that closed stdout while holding stderr open must not hang
            // the drain — the exact freeze this path exists to prevent.
            let deadline = std::time::Instant::now() + STDERR_SETTLE;
            while !stderr_done_r.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            // Ordering matters: stderr is complete before `alive` flips, so a
            // racing `request()` that observes "dead" also sees the full message.
            let tail = stderr_r.lock().ok().map(|s| s.trim().to_string()).unwrap_or_default();
            alive_r.store(false, Ordering::SeqCst);
            let msg = if tail.is_empty() {
                "pi --mode rpc exited".to_string()
            } else {
                format!("pi --mode rpc exited. Stderr: {tail}")
            };
            if let Ok(mut p) = pending_r.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(msg.clone()));
                }
            }
        });

        Ok((
            PiRpcClient {
                stdin: Arc::new(Mutex::new(Some(stdin))),
                next_id: Arc::new(AtomicU64::new(1)),
                pending,
                alive,
                stderr: stderr_buf,
            },
            inbound_rx,
            child,
        ))
    }

    /// Whether pi is still running (its stdout hasn't hit EOF).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// pi's captured stderr so far (bounded tail).
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().ok().map(|s| s.clone()).unwrap_or_default()
    }

    /// A fresh correlation id.
    pub fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Send a command and block (up to `timeout`) for its response.
    ///
    /// NOTE on `abort`: pi emits `response:abort` *after* `agent_settled`, so a
    /// request-shaped abort naturally blocks until the turn has fully settled.
    /// That is useful (it is a free cancel-and-wait) but it is not instant —
    /// callers that must not block should use [`Self::send`].
    pub fn request(&self, cmd: PiCommand, timeout: Duration) -> Result<RpcResponse> {
        let id = cmd.id().to_string();
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("pi pending map poisoned"))?
            .insert(id.clone(), tx);
        // The EOF drain only fails requests that were in the map when it ran. An
        // insert landing after it would never be drained, and a write to a
        // not-yet-closed pipe can still succeed — leaving this parked for the
        // full timeout. Re-check after inserting so a dead pi always fails fast.
        if !self.is_alive() {
            self.pending.lock().ok().and_then(|mut p| p.remove(&id));
            let tail = self.stderr_tail();
            let tail = tail.trim();
            return Err(if tail.is_empty() {
                anyhow!("pi --mode rpc exited")
            } else {
                anyhow!("pi --mode rpc exited. Stderr: {tail}")
            });
        }
        if let Err(e) = self.write_cmd(&cmd) {
            self.pending.lock().ok().and_then(|mut p| p.remove(&id));
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut p| p.remove(&id));
                Err(anyhow!("pi request timed out"))
            }
        }
    }

    /// Send a command without waiting for its response.
    pub fn send(&self, cmd: PiCommand) -> Result<()> {
        self.write_cmd(&cmd)
    }

    fn write_cmd(&self, cmd: &PiCommand) -> Result<()> {
        let line = serde_json::to_string(cmd).context("serialize pi command")?;
        let mut guard = self.stdin.lock().map_err(|_| anyhow!("pi stdin lock poisoned"))?;
        let stdin = guard.as_mut().ok_or_else(|| anyhow!("pi stdin is closed"))?;
        stdin.write_all(line.as_bytes()).context("write pi stdin")?;
        stdin.write_all(b"\n").context("write pi newline")?;
        stdin.flush().context("flush pi stdin")?;
        Ok(())
    }

    /// Close stdin, signalling pi that no more commands are coming. Idempotent;
    /// any later write fails rather than silently going nowhere.
    pub fn close_stdin(&self) {
        if let Ok(mut s) = self.stdin.lock()
            && let Some(mut h) = s.take()
        {
            let _ = h.flush();
            // `h` drops here — that is what actually closes the pipe.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_correlates_by_string_id_and_events_broadcast() {
        // A fake pi: read the command, answer with a matching id, then emit an event.
        let script = r#"
read line
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"abc"}}\n'
printf '{"type":"agent_settled"}\n'
sleep 0.2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let r = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("response");
        assert_eq!(r.command, "get_state");
        let data = r.into_data().expect("data");
        assert_eq!(data["sessionId"], "abc");
        match inbound.recv_timeout(Duration::from_secs(5)) {
            Ok(Inbound::Event(v)) => assert_eq!(v["type"], "agent_settled"),
            _ => panic!("expected the agent_settled event"),
        }
    }

    #[test]
    fn child_exit_fails_pending_fast_and_carries_stderr() {
        // Read the command, complain on stderr, exit without responding. The
        // pending request must fail via the EOF drain (fast), and the error must
        // carry pi's stderr — "exited" alone can't distinguish bad auth from a
        // bad flag.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("read line; echo 'boom: bad auth' >&2; exit 1");
        let (rpc, _inbound, mut child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        assert!(rpc.is_alive());
        let start = std::time::Instant::now();
        let err = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(30))
            .expect_err("must fail once the child exits");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must fail fast via the EOF drain, not wait out the 30s timeout"
        );
        assert!(err.to_string().contains("bad auth"), "stderr must reach the error: {err}");
        let _ = child.wait();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!rpc.is_alive(), "must be marked dead after exit");
    }

    #[test]
    fn framing_splits_on_lf_only_and_preserves_u2028() {
        // pi warns payload strings may contain U+2028/U+2029 and that clients
        // must split on \n alone. The separator must survive the round trip.
        let sep = "\u{2028}";
        let payload = json!({
            "id": "s1", "type": "response", "command": "get_state", "success": true,
            "data": {"sessionId": format!("a{sep}b")}
        });
        // serde_json emits U+2028 raw (it is valid inside a JSON string).
        let line = serde_json::to_string(&payload).unwrap();
        assert!(line.contains(sep), "the fixture must actually contain U+2028");
        let script = format!("read line\nprintf '%s\\n' '{line}'\nsleep 0.2\n");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let data = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("response")
            .into_data()
            .expect("data");
        assert_eq!(
            data["sessionId"], format!("a{sep}b"),
            "U+2028 must not be treated as a line break"
        );
    }

    #[test]
    fn non_json_and_unknown_events_are_not_fatal() {
        let script = r#"
read line
printf 'pi: some human diagnostic\n'
printf '{"type":"some_future_event","x":1}\n'
printf '{"id":"s1","type":"response","command":"get_state","success":true,"data":{"sessionId":"ok"}}\n'
sleep 0.2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let data = rpc
            .request(PiCommand::GetState { id: "s1".into() }, Duration::from_secs(5))
            .expect("a non-JSON line and an unknown event must not break the stream")
            .into_data()
            .expect("data");
        assert_eq!(data["sessionId"], "ok");
    }

    #[test]
    fn a_failed_response_surfaces_pis_error_text() {
        let script = r#"
read line
printf '{"id":"c1","type":"response","command":"compact","success":false,"error":"Nothing to compact (session too small)"}\n'
sleep 0.2
"#;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        let (rpc, _inbound, _child) = PiRpcClient::spawn_command(cmd).expect("spawn fake");
        let r = rpc
            .request(PiCommand::Abort { id: "c1".into() }, Duration::from_secs(5))
            .expect("the response itself arrives");
        assert!(!r.success);
        let err = r.into_data().expect_err("a failed response is an Err");
        assert!(err.to_string().contains("session too small"), "got {err}");
    }
}
