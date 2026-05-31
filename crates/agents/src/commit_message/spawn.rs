//! Spawn the planned agent CLI and harvest its commit-message output.
//!
//! Translates [`super::plan::CommitMessagePlan`] into a `tokio::process`
//! call: pipes the prompt over stdin (when the plan says so), drains
//! stdout + stderr, applies a 60-second timeout, caps total output at
//! 4 MB, and supports cooperative cancellation via a shared
//! `Arc<AtomicBool>`.
//!
//! Cancellation contract:
//!
//! - The Stop button on the UI overlay calls
//!   [`super::super::ai_generation::CommitArea::cancel_ai_generation`],
//!   which signals the flag AND drops the held `Task<()>`. Dropping
//!   the task drops this future, which drops the
//!   `wait_with_output` future, which drops the `Child` — Tokio's
//!   `kill_on_drop(true)` then sends `SIGKILL` to the agent CLI.
//! - The user-typing path (`InputEvent::Change` observer) only sets
//!   the flag. This module's polling future picks it up and exits
//!   the `select!` arm, dropping the wait future the same way.
//!
//! Both paths converge on dropping the Child handle, which is the
//! mechanism that actually kills the CLI. The flag exists so the
//! task can report `Err(GenerationError::Canceled)` to its caller
//! instead of just disappearing.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::plan::CommitMessagePlan;
use super::prompt::{clean_message, extract_agent_error, format_message, split_message};

/// 60s ceiling on a single generation; agents that haven't responded
/// by then are almost always wedged on a network call or a model
/// error. Surface a timeout rather than wait indefinitely so the
/// user can cancel and retry.
pub const GENERATION_TIMEOUT: Duration = Duration::from_secs(60);

/// 4 MB output cap. Real commit messages are KB-scale; this is a
/// runaway-output guardrail (a misconfigured agent could stream
/// tokens forever), not an expected budget.
pub const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// How often the cancel-poll future wakes to check the flag. 50 ms
/// is below human perception of "stop didn't work" without burning
/// the CPU on a tight loop.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Error)]
pub enum GenerationError {
    #[error("{label} binary `{binary}` not found on PATH")]
    BinaryNotFound { label: String, binary: String },
    #[error("{label} failed to start: {message}")]
    SpawnFailed { label: String, message: String },
    #[error("{label} generation canceled")]
    Canceled { label: String },
    #[error("{label} generation timed out after {seconds}s")]
    Timeout { label: String, seconds: u64 },
    #[error("{label} returned too much data")]
    OutputTooLarge { label: String },
    #[error("{label} returned an empty message")]
    EmptyOutput { label: String },
    #[error("{label} failed: {detail}")]
    AgentFailed { label: String, detail: String },
    #[error("I/O error while running {label}: {message}")]
    Io { label: String, message: String },
}

/// Run the planned agent CLI to completion (or cancellation /
/// timeout) and return its parsed commit message ready to drop into
/// the textarea.
///
/// `cancel` is checked at three points: before spawning, in the
/// polling future racing the wait, and inside the post-wait branch
/// before the caller would use the result. Calling code that flips
/// the flag must also drop the parent `Task<()>` to actually kill
/// the child (see module-level docs).
pub async fn run_plan(
    plan: CommitMessagePlan,
    cwd: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<String, GenerationError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(GenerationError::Canceled { label: plan.label });
    }

    let mut cmd = Command::new(&plan.binary);
    cmd.args(&plan.args)
        .current_dir(cwd)
        .stdin(if plan.stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Sandboxed env hygiene — same hardening pattern as
        // `oximux_git::process::GitCmd`. Don't let a user pager or
        // credential helper hijack the agent CLI; force ASCII C
        // locale so the model receives deterministic byte sequences.
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(GenerationError::BinaryNotFound {
                label: plan.label,
                binary: plan.binary,
            });
        }
        Err(e) => {
            return Err(GenerationError::SpawnFailed {
                label: plan.label,
                message: e.to_string(),
            });
        }
    };

    if let Some(payload) = plan.stdin_payload.as_deref()
        && let Some(mut stdin) = child.stdin.take()
    {
        if let Err(e) = stdin.write_all(payload.as_bytes()).await {
            // Don't leak the half-spawned child if stdin write fails.
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(GenerationError::Io {
                label: plan.label,
                message: e.to_string(),
            });
        }
        // Drop the stdin handle so the agent sees EOF and starts
        // producing output. Without this, agents that wait for
        // EOF before generating hang indefinitely.
        drop(stdin);
    }

    // Race the child's exit against the timeout and cancel-poll.
    // Whichever future wins, the losers are dropped — including the
    // `wait_with_output` future, which owns the Child handle.
    // `kill_on_drop(true)` on the spawn ensures the child gets
    // SIGKILL when we abandon the wait future.
    let label = plan.label.clone();
    let cancel_check = async {
        loop {
            tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
            if cancel.load(Ordering::SeqCst) {
                return;
            }
        }
    };

    let output = tokio::select! {
        // Tokio's `wait_with_output` takes ownership of the child;
        // it returns `Output { status, stdout, stderr }` on completion.
        result = timeout(GENERATION_TIMEOUT, child.wait_with_output()) => {
            match result {
                Ok(Ok(o)) => o,
                Ok(Err(e)) => return Err(GenerationError::Io {
                    label,
                    message: e.to_string(),
                }),
                Err(_) => return Err(GenerationError::Timeout {
                    label,
                    seconds: GENERATION_TIMEOUT.as_secs(),
                }),
            }
        }
        () = cancel_check => {
            return Err(GenerationError::Canceled { label });
        }
    };

    if output.stdout.len() > MAX_OUTPUT_BYTES {
        return Err(GenerationError::OutputTooLarge { label: plan.label });
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        let detail = extract_agent_error(&stdout_str, &stderr_str)
            .unwrap_or_else(|| "exited with non-zero status".to_string());
        return Err(GenerationError::AgentFailed {
            label: plan.label,
            detail,
        });
    }

    let cleaned = clean_message(&stdout_str);
    if cleaned.is_empty() {
        // Some CLIs (e.g. claude on auth failure) exit 0 with an
        // error message in stderr but empty stdout. Surface it.
        if let Some(detail) = extract_agent_error(&stdout_str, &stderr_str) {
            return Err(GenerationError::AgentFailed {
                label: plan.label,
                detail,
            });
        }
        return Err(GenerationError::EmptyOutput { label: plan.label });
    }

    let split = split_message(&cleaned);
    Ok(format_message(&split))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_cwd() -> PathBuf {
        std::env::temp_dir()
    }

    fn plan_for(binary: &str, args: Vec<&str>, stdin: Option<&str>) -> CommitMessagePlan {
        CommitMessagePlan {
            binary: binary.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            stdin_payload: stdin.map(str::to_string),
            label: binary.to_string(),
        }
    }

    #[tokio::test]
    async fn run_plan_returns_binary_not_found_for_missing_command() {
        let plan = plan_for("oximux-non-existent-binary-xyz", vec![], None);
        let result = run_plan(plan, &tmp_cwd(), Arc::new(AtomicBool::new(false))).await;
        match result {
            Err(GenerationError::BinaryNotFound { .. }) => {}
            other => panic!("expected BinaryNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_plan_returns_empty_when_echo_returns_blank() {
        // /bin/echo of an empty arg → "" on stdout → cleaned → empty.
        let plan = plan_for("/bin/echo", vec!["-n"], None);
        let result = run_plan(plan, &tmp_cwd(), Arc::new(AtomicBool::new(false))).await;
        match result {
            Err(GenerationError::EmptyOutput { .. }) => {}
            other => panic!("expected EmptyOutput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_plan_returns_message_when_echo_returns_text() {
        let plan = plan_for("/bin/echo", vec!["feat: hello"], None);
        let message = run_plan(plan, &tmp_cwd(), Arc::new(AtomicBool::new(false)))
            .await
            .expect("echo should succeed");
        assert_eq!(message, "feat: hello");
    }

    #[tokio::test]
    async fn run_plan_pipes_stdin_payload() {
        // cat reads stdin and writes to stdout — round-trip the payload.
        let plan = plan_for("/bin/cat", vec![], Some("feat: from stdin"));
        let message = run_plan(plan, &tmp_cwd(), Arc::new(AtomicBool::new(false)))
            .await
            .expect("cat should succeed");
        assert_eq!(message, "feat: from stdin");
    }

    #[tokio::test]
    async fn run_plan_surfaces_non_zero_exit_as_agent_failed() {
        // `false` exits 1 with no stdout/stderr → AgentFailed with
        // generic detail (no ERROR: line to extract).
        let plan = plan_for("/usr/bin/false", vec![], None);
        let result = run_plan(plan, &tmp_cwd(), Arc::new(AtomicBool::new(false))).await;
        match result {
            Err(GenerationError::AgentFailed { .. }) => {}
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_plan_honours_cancel_flag_set_before_start() {
        let cancel = Arc::new(AtomicBool::new(true));
        let plan = plan_for("/bin/echo", vec!["hello"], None);
        let result = run_plan(plan, &tmp_cwd(), cancel).await;
        match result {
            Err(GenerationError::Canceled { .. }) => {}
            other => panic!("expected Canceled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_plan_honours_cancel_flag_set_mid_flight() {
        // `sleep 5` would normally run 5s; flip cancel after 100ms
        // and verify we get Canceled instead of waiting it out.
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_setter = cancel.clone();
        let plan = plan_for("/bin/sleep", vec!["5"], None);
        let cwd = tmp_cwd();

        let setter = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_setter.store(true, Ordering::SeqCst);
        });

        let result = run_plan(plan, &cwd, cancel).await;
        setter.await.expect("setter task");
        match result {
            Err(GenerationError::Canceled { .. }) => {}
            other => panic!("expected Canceled, got {other:?}"),
        }
    }
}
