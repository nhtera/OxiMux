//! `ClaudeStreamJsonConnection` — drives a `claude` subprocess in the
//! persistent stream-json mode and surfaces decoded `ThreadEvent`s on a channel.
//!
//! Transport (confirmed in the Phase-1 spike, see `spike-findings.md`): a plain
//! piped subprocess, NOT the relay PTY — stdout is clean newline-delimited JSON
//! so a background reader thread can `decode_line` it directly. stdin carries
//! user messages and `control_response`s. Relaunch-survival is provided later by
//! transcript persistence + `--resume`, not the relay.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::thread;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::connection::{control_response_json, user_message_json, AgentConnection};
use super::event::ThreadEvent;
use super::stream_json::decode_line;
use super::tool_call::PermissionDecision;

/// Flags for the persistent, structured, interactive Claude session. Pure so
/// it can be unit-tested. `--permission-prompt-tool stdio` routes approvals to
/// us as `can_use_tool` control requests; `--setting-sources project` isolates
/// the session from the user's global `~/.claude` hooks (which otherwise
/// corrupt the permission round-trip — a spike finding).
pub fn build_args(model: Option<&str>) -> Vec<String> {
    build_args_with_resume(model, None)
}

/// Same as [`build_args`], plus `--resume <session_id>` when restoring a
/// persisted chat. Resuming reuses the original session id (no `--fork-session`)
/// so the continued conversation keeps its server-side context; the UI already
/// rehydrated the visible transcript from disk, so this only needs to reconnect
/// the *next* turn to the prior history.
pub fn build_args_with_resume(model: Option<&str>, resume_session_id: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
        "--setting-sources",
        "project",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(sid) = resume_session_id.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    args
}

pub struct ClaudeStreamJsonConnection {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
}

impl ClaudeStreamJsonConnection {
    /// Spawn `claude` in `cwd` and start streaming decoded events.
    pub fn spawn(cwd: &Path, model: Option<&str>) -> Result<(Self, Receiver<ThreadEvent>)> {
        let mut cmd = Command::new("claude");
        cmd.args(build_args(model)).current_dir(cwd);
        Self::spawn_command(cmd)
    }

    /// Spawn `claude` resuming a persisted session (`--resume <session_id>`) so a
    /// restored chat tab continues the same conversation. Falls back to a fresh
    /// session when `session_id` is `None` (a chat tab that never completed a
    /// turn has no id to resume).
    pub fn spawn_resumed(
        cwd: &Path,
        model: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let mut cmd = Command::new("claude");
        cmd.args(build_args_with_resume(model, session_id))
            .current_dir(cwd);
        Self::spawn_command(cmd)
    }

    /// Spawn an already-built command (the real `claude` command, or a fake in
    /// tests) and wire stdout → `decode_line` → the returned receiver.
    pub fn spawn_command(mut cmd: Command) -> Result<(Self, Receiver<ThreadEvent>)> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawn agent process")?;
        let stdout = child.stdout.take().context("agent stdout missing")?;
        let stdin = child.stdin.take().context("agent stdin missing")?;

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break }; // read error — treat as EOF
                for ev in decode_line(&line) {
                    if tx.send(ev).is_err() {
                        return; // consumer gone — stop reading
                    }
                }
            }
            // stdout closed: the sender drops here, so the consumer observes a
            // disconnect. The app treats a disconnect with a pending permission
            // as a fail-closed Reject.
        });

        Ok((
            Self {
                stdin: Mutex::new(stdin),
                child: Mutex::new(child),
            },
            rx,
        ))
    }

    fn write_line(&self, v: &Value) -> Result<()> {
        // Avoid `.expect()` on the lock (poison would panic the caller); map to
        // a recoverable error instead.
        let mut stdin = self
            .stdin
            .lock()
            .map_err(|_| anyhow!("agent stdin lock poisoned"))?;
        writeln!(stdin, "{v}").context("write to agent stdin")?;
        stdin.flush().context("flush agent stdin")?;
        Ok(())
    }
}

impl AgentConnection for ClaudeStreamJsonConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.write_line(&user_message_json(text))
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        self.write_line(&control_response_json(request_id, &decision))
    }

    fn shutdown(&self) {
        // Even if the lock is poisoned (a prior panic while holding it), still
        // kill+reap the child so a `claude` process isn't leaked.
        let mut child = match self.child.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn build_args_has_required_flags() {
        let a = build_args(None);
        for flag in [
            "--input-format",
            "stream-json",
            "--output-format",
            "--permission-prompt-tool",
            "stdio",
            "--setting-sources",
            "project",
        ] {
            assert!(a.iter().any(|x| x == flag), "missing {flag} in {a:?}");
        }
        assert!(!a.iter().any(|x| x == "--model"), "no model → no --model flag");
    }

    #[test]
    fn build_args_appends_model_when_set() {
        let a = build_args(Some("opus"));
        let i = a.iter().position(|x| x == "--model").expect("--model present");
        assert_eq!(a[i + 1], "opus");
        // blank model is skipped
        assert!(!build_args(Some("  ")).iter().any(|x| x == "--model"));
    }

    #[test]
    fn build_args_appends_resume_when_session_id_set() {
        let a = build_args_with_resume(None, Some("sid-123"));
        let i = a.iter().position(|x| x == "--resume").expect("--resume present");
        assert_eq!(a[i + 1], "sid-123");
        // blank / absent session id → no --resume
        assert!(!build_args_with_resume(None, Some("  ")).iter().any(|x| x == "--resume"));
        assert!(!build_args_with_resume(None, None).iter().any(|x| x == "--resume"));
        // plain build_args never resumes
        assert!(!build_args(None).iter().any(|x| x == "--resume"));
    }

    /// Spawn a FAKE program that prints two stream-json lines; the reader
    /// thread must decode them to the receiver. Proves the spawn/read/decode
    /// wiring without a real `claude`.
    #[test]
    fn reader_thread_decodes_stdout_lines() {
        let l1 = serde_json::json!({"type":"system","subtype":"init",
            "session_id":"s","model":"m","permissionMode":"default"})
        .to_string();
        let l2 = serde_json::json!({"type":"result","subtype":"success",
            "result":"done","total_cost_usd":0.0})
        .to_string();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("printf '%s\\n' '{l1}' '{l2}'"));

        let (_conn, rx) = ClaudeStreamJsonConnection::spawn_command(cmd).expect("spawn fake");
        let mut evs = Vec::new();
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(5)) {
            evs.push(ev);
        }
        assert!(
            matches!(evs.first(), Some(ThreadEvent::SessionInit { .. })),
            "first event should be SessionInit, got {evs:?}"
        );
        assert!(
            matches!(evs.last(), Some(ThreadEvent::TurnEnded { .. })),
            "last event should be TurnEnded, got {evs:?}"
        );
    }
}
