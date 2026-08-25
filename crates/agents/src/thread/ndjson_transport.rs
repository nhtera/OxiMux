//! Untyped newline-JSON RPC client core, shared by the Pi-family transports.
//!
//! pi and omp speak the same envelope over a subprocess's stdio: LF-delimited
//! JSON lines where a `{type:"response", id, command, success, …}` frame
//! answers a command that carried the same `id`, and every other frame is an
//! unsolicited event. This core owns the parts that are identical and
//! load-bearing — the LF-only framing, the id correlation, the stderr fold,
//! and the EOF drain that unblocks pending callers when the process dies —
//! while each adapter keeps its own typed command/event layer on top
//! (deliberately NOT generified; see the pi/omp wrappers).
//!
//! **Framing is LF-only, deliberately.** pi's `jsonl.ts` warns that payload
//! strings may contain U+2028/U+2029 and that clients must split on `\n`
//! alone — so this reads via `read_until(b'\n')`, never `BufRead::lines()`
//! (which also strips a trailing `\r`, corrupting a payload byte).
//!
//! `preprocess` is the one seam the adapters differ on at this level: omp's
//! protocol v2 delivers frames >1MiB as `rpc_chunk` sequences that must be
//! reassembled BEFORE classification; pi has no such stage. The hook runs on
//! the reader thread, mapping each raw value to zero-or-more logical frames.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

type Pending = Arc<Mutex<HashMap<String, Sender<Result<Value, String>>>>>;

/// Maps one raw stdout value to zero-or-more logical frames, on the reader
/// thread. Stateful (omp's chunk reassembly buffers across calls).
pub type Preprocess = Box<dyn FnMut(Value) -> Vec<Value> + Send>;

/// How long the exit path waits for stderr to finish after stdout hits EOF.
/// Generous relative to "the process is already exiting", tight enough that a
/// child holding stderr open can't stall the drain that unblocks the UI.
const STDERR_SETTLE: Duration = Duration::from_millis(250);

/// A cloneable handle to a running newline-JSON RPC child's stdin + pending
/// registry. Cloning shares the same child (all fields are `Arc`).
#[derive(Clone)]
pub struct NdjsonRpcClient {
    /// `None` once closed. Held as an `Option` specifically so
    /// [`Self::close_stdin`] can drop the pipe: the Pi family treats stdin EOF
    /// as "no more commands are coming", and a shared handle that merely
    /// flushed would never deliver that.
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    next_id: Arc<AtomicU64>,
    pending: Pending,
    /// Cleared by the reader thread on stdout EOF (the process exited). The
    /// worker polls this so a crash ends the session instead of freezing the
    /// chat.
    alive: Arc<AtomicBool>,
    /// The child's stderr, accumulated (bounded tail). Folded into the error
    /// when the process dies — a bare "exited" is useless for diagnosing bad
    /// auth or a bad flag.
    stderr: Arc<Mutex<String>>,
    /// Diagnostic name for error strings (e.g. `"pi --mode rpc"`).
    name: &'static str,
}

impl NdjsonRpcClient {
    /// Spawn an already-built command (the real CLI, or a fake in tests) and
    /// wire its stdout into the reader/router. Non-response frames (and
    /// responses that can't be correlated) arrive raw on the returned channel.
    pub fn spawn_command(
        mut cmd: Command,
        name: &'static str,
        mut preprocess: Option<Preprocess>,
    ) -> Result<(NdjsonRpcClient, Receiver<Value>, Child)> {
        use oximux_no_window::NoWindow as _;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).no_window();
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group so teardown can reach the whole tree. The bash
            // tool spawns detached children; SIGTERM to the agent runs its
            // handler (which reaps them), and the group is the backstop.
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().with_context(|| format!("spawn {name}"))?;
        let stdout = child.stdout.take().with_context(|| format!("{name} stdout missing"))?;
        let stdin = child.stdin.take().with_context(|| format!("{name} stdin missing"))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (inbound_tx, inbound_rx) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let stderr_buf = Arc::new(Mutex::new(String::new()));

        // Set once stderr hits EOF, so the exit path can tell "the agent
        // printed nothing" apart from "we haven't read it yet".
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
            'read: loop {
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
                    // The agent may print non-JSON diagnostics; skip, don't crash.
                    tracing::debug!(
                        agent = name,
                        line = %String::from_utf8_lossy(&line),
                        "skipping non-JSON stdout line"
                    );
                    continue;
                };
                let frames = match preprocess.as_mut() {
                    Some(f) => f(v),
                    None => vec![v],
                };
                for v in frames {
                    // A response whose id has a waiter is correlated; anything
                    // else — events, unsolicited or late responses, responses
                    // with no id — broadcasts raw for the adapter to classify.
                    let correlated_id = (v.get("type").and_then(Value::as_str) == Some("response"))
                        .then(|| v.get("id").and_then(Value::as_str))
                        .flatten()
                        .map(str::to_string);
                    let tx = correlated_id
                        .and_then(|id| pending_r.lock().ok().and_then(|mut p| p.remove(&id)));
                    match tx {
                        Some(tx) => {
                            let _ = tx.send(Ok(v));
                        }
                        None => {
                            if inbound_tx.send(v).is_err() {
                                break 'read; // consumer gone — still run the exit cleanup
                            }
                        }
                    }
                }
            }
            // stdout closed (EOF / exit) or the consumer went away. Fail every
            // outstanding request so blocked callers unblock now rather than
            // waiting out a timeout — the agent dying during the handshake
            // would otherwise hang a new chat forever.
            //
            // First give stderr a moment to finish: stdout EOF means the
            // process is going away, so its stderr EOF follows almost
            // immediately, and the whole value of this error is *why* it died.
            // Bounded, because a process that closed stdout while holding
            // stderr open must not hang the drain.
            let deadline = std::time::Instant::now() + STDERR_SETTLE;
            while !stderr_done_r.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            // Ordering matters: stderr is complete before `alive` flips, so a
            // racing `request()` that observes "dead" also sees the full
            // message.
            let tail = stderr_r.lock().ok().map(|s| s.trim().to_string()).unwrap_or_default();
            alive_r.store(false, Ordering::SeqCst);
            let msg = if tail.is_empty() {
                format!("{name} exited")
            } else {
                format!("{name} exited. Stderr: {tail}")
            };
            if let Ok(mut p) = pending_r.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(msg.clone()));
                }
            }
        });

        Ok((
            NdjsonRpcClient {
                stdin: Arc::new(Mutex::new(Some(stdin))),
                next_id: Arc::new(AtomicU64::new(1)),
                pending,
                alive,
                stderr: stderr_buf,
                name,
            },
            inbound_rx,
            child,
        ))
    }

    /// Whether the agent is still running (its stdout hasn't hit EOF).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// The agent's captured stderr so far (bounded tail).
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().ok().map(|s| s.clone()).unwrap_or_default()
    }

    /// A fresh correlation id.
    pub fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    /// Send a pre-serialized command line carrying `id` and block (up to
    /// `timeout`) for the raw response value that echoes it.
    pub fn request_value(&self, id: &str, line: &str, timeout: Duration) -> Result<Value> {
        let name = self.name;
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| anyhow!("{name} pending map poisoned"))?
            .insert(id.to_string(), tx);
        // The EOF drain only fails requests that were in the map when it ran.
        // An insert landing after it would never be drained, and a write to a
        // not-yet-closed pipe can still succeed — leaving this parked for the
        // full timeout. Re-check after inserting so a dead agent fails fast.
        if !self.is_alive() {
            self.pending.lock().ok().and_then(|mut p| p.remove(id));
            let tail = self.stderr_tail();
            let tail = tail.trim();
            return Err(if tail.is_empty() {
                anyhow!("{name} exited")
            } else {
                anyhow!("{name} exited. Stderr: {tail}")
            });
        }
        if let Err(e) = self.write_line(line) {
            self.pending.lock().ok().and_then(|mut p| p.remove(id));
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut p| p.remove(id));
                Err(anyhow!("{name} request timed out"))
            }
        }
    }

    /// Send a pre-serialized line without waiting for a response.
    pub fn send_line(&self, line: &str) -> Result<()> {
        self.write_line(line)
    }

    fn write_line(&self, line: &str) -> Result<()> {
        let name = self.name;
        let mut guard = self.stdin.lock().map_err(|_| anyhow!("{name} stdin lock poisoned"))?;
        let stdin = guard.as_mut().ok_or_else(|| anyhow!("{name} stdin is closed"))?;
        stdin.write_all(line.as_bytes()).with_context(|| format!("write {name} stdin"))?;
        stdin.write_all(b"\n").with_context(|| format!("write {name} newline"))?;
        stdin.flush().with_context(|| format!("flush {name} stdin"))?;
        Ok(())
    }

    /// Close stdin, signalling the agent that no more commands are coming.
    /// Idempotent; any later write fails rather than silently going nowhere.
    pub fn close_stdin(&self) {
        if let Ok(mut s) = self.stdin.lock()
            && let Some(mut h) = s.take()
        {
            let _ = h.flush();
            // `h` drops here — that is what actually closes the pipe.
        }
    }
}
