//! `ClaudeStreamJsonConnection` — drives a `claude` subprocess in the
//! persistent stream-json mode and surfaces decoded `ThreadEvent`s on a channel.
//!
//! Transport (confirmed in the Phase-1 spike, see `spike-findings.md`): a plain
//! piped subprocess, NOT the relay PTY — stdout is clean newline-delimited JSON
//! so a background reader thread can `decode_line` it directly. stdin carries
//! user messages and `control_response`s. Relaunch-survival is provided later by
//! transcript persistence + `--resume`, not the relay.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::connection::{
    control_response_json, interrupt_json, question_answer_json, set_permission_mode_json,
    user_message_json, user_message_json_with_images, AgentCapabilities, AgentConnection,
    EffortChoice, ModeChoice, ModelChoice,
};
use super::entry::ChatImage;
use super::mcp_server_spec::{to_claude_mcp_config, McpServerSpec};
use super::question::{AskQuestion, QuestionAnswers};
use super::event::ThreadEvent;
use super::stream_json::decode_line;
use super::tool_call::PermissionDecision;

// --- In-chat picker vocabulary -------------------------------------------
// The model/mode/effort options Claude offers, owned by the backend that
// speaks them (not the view) so the composer can render whatever the live
// connection advertises. Surfaced through the `AgentConnection` accessors below.

/// Claude model aliases offered in the model picker, as `(wire, label, blurb)`.
/// The CLI accepts the `wire` alias directly as `--model`; the `label` is the
/// capitalized name shown in the picker; the `blurb` is a one-line capability
/// hint rendered muted beneath the name (and matched by the model search).
///
/// **The alias is the wire value, and the version lives in the blurb** — the
/// shape the CLI's own `/model` picker uses ("Opus" → "Opus 5 · Best for
/// everyday, complex tasks"). An alias means "the latest of this family", so
/// pinning a full id (`claude-opus-5`) instead would quietly keep running last
/// year's model after the CLI moved on. The cost is that a new release dates the
/// blurb until this list is refreshed — a stale *description*, never a stale
/// pick, which is the right way round.
///
/// Wording and versions are taken from the installed CLI's own picker rather
/// than written here, so the two agree on which model is "most capable" — a
/// judgement that moves with each release, and did: Fable, not Opus, holds it now.
const CLAUDE_MODELS: &[(&str, &str, &str)] = &[
    ("opus", "Opus", "Opus 5 · Best for everyday, complex tasks"),
    ("fable", "Fable", "Fable 5 · Most capable for your hardest and longest-running tasks"),
    ("sonnet", "Sonnet", "Sonnet 5 · Efficient for routine tasks"),
    ("haiku", "Haiku", "Haiku 4.5 · Fastest for quick answers"),
];

/// The static Claude chat-model vocabulary as [`ModelChoice`]s (pretty label +
/// capability blurb). Shared by the live connection's `models()` **and** the
/// pre-bind roster, so the unbound "New Agent" draft shows the same names and
/// descriptions a bound Claude session does — one source, no drift.
pub fn claude_model_choices() -> Vec<ModelChoice> {
    CLAUDE_MODELS
        .iter()
        .map(|(wire, label, blurb)| ModelChoice {
            wire: (*wire).to_string(),
            label: (*label).to_string(),
            description: Some((*blurb).to_string()),
        })
        .collect()
}

/// Permission modes as `(wire, label)`: `wire` → `--permission-mode`, `label`
/// is shown to the user.
/// - **default** — prompt before each tool.
/// - **acceptEdits** — auto-approve file edits; still prompt for other tools.
/// - **plan** — read-only planning; no tools execute.
/// - **bypassPermissions** — never prompt.
const CLAUDE_PERMISSION_MODES: &[(&str, &str)] = &[
    ("default", "Ask each time"),
    ("acceptEdits", "Accept edits"),
    ("plan", "Plan mode"),
    ("bypassPermissions", "Bypass all"),
];

/// The permission mode treated as baseline (no `--permission-mode` flag), also
/// the value shown as current when the user hasn't picked one.
const DEFAULT_PERMISSION_MODE: &str = "default";

/// Reasoning-effort levels as `(wire, label)`: `wire` → `--effort`.
const CLAUDE_EFFORTS: &[(&str, &str)] = &[
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "Extra high"),
    ("max", "Max"),
];

/// The effort shown as current when none is chosen — the CLI's own default.
const DEFAULT_EFFORT: &str = "high";

/// The model shown as current when none is chosen — Claude's mid alias
/// (`CLAUDE_MODELS[1].0`, "sonnet").
const DEFAULT_MODEL: &str = "sonnet";

/// Flags for the persistent, structured, interactive Claude session. Pure so
/// it can be unit-tested. `--permission-prompt-tool stdio` routes approvals to
/// us as `can_use_tool` control requests. `--setting-sources user,project,local`
/// loads the user's global skills/commands (so the slash-command palette offers
/// everything installed, like a full Claude Code session) — at the cost of also
/// loading the global `CLAUDE.md` + hooks (context per turn). An earlier spike
/// flagged that global hooks could corrupt the permission round-trip; the
/// current CLI emits clean `system` hook events instead, and a lean-vs-full
/// comparison showed identical permission behavior — but if Allow/Reject or
/// AskUserQuestion ever regresses, narrow this back to `project`.
pub fn build_args(model: Option<&str>) -> Vec<String> {
    build_args_with_resume(model, None, None, None, &HostInjection::default())
}

/// What the *host* adds to a launch, on top of the session's own flags.
///
/// One struct rather than three more parameters because the three are one
/// decision: OxiMux declares a sidecar server, registers the hook that polices
/// its tools, and removes the ones no policy can make safe. Passing the first
/// without the others hands an agent a capability with nothing watching it, so
/// they are built together by whoever decides to grant it and travel from there
/// as a unit.
///
/// Empty is the overwhelmingly common case and stays byte-identical to the
/// pre-seam invocation: no field set emits no flag.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostInjection<'a> {
    /// Servers OxiMux supplies and supervises, rendered into `--mcp-config`.
    pub mcp_servers: &'a [McpServerSpec],
    /// Inline JSON for `--settings`. Additive to `--setting-sources`, which
    /// keeps loading the user's own settings; this is the per-session layer.
    ///
    /// Exactly one may be passed — the CLI's behaviour with two `--settings`
    /// flags is undocumented — so this is a single value rather than a list, and
    /// anything else wanting to inject settings has to merge into it.
    pub settings: Option<&'a str>,
    /// Tool names for `--disallowedTools`, which removes them from the agent's
    /// surface in every permission mode and outranks a user's `permissions.allow`.
    pub disallowed_tools: &'a [String],
}

/// Same as [`build_args`], plus `--resume <session_id>` when restoring a
/// persisted chat, `--permission-mode <mode>` for a non-default mode, and
/// `--effort <level>` for a chosen reasoning effort. Resuming reuses the original
/// session id (no `--fork-session`) so the continued conversation keeps its
/// server-side context; the UI already rehydrated the visible transcript from
/// disk, so this only needs to reconnect the *next* turn to the prior history.
/// Permission mode and effort are both fixed at spawn (like `--model`), so a live
/// switch of either respawns via this same path.
///
/// `host` is what OxiMux itself adds — see [`HostInjection`]. An empty one emits
/// no extra flags at all, so that invocation stays byte-identical to the
/// pre-seam one — asserted by `no_host_injection_emits_no_flags`. Host servers
/// are additive, not exclusive: `--strict-mcp-config` is deliberately NOT
/// passed, because `--setting-sources user,project,local` above exists precisely
/// so the chat still sees the user's own MCP servers.
pub fn build_args_with_resume(
    model: Option<&str>,
    resume_session_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    host: &HostInjection<'_>,
) -> Vec<String> {
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
        // Load user + project + local settings so the chat sees the user's
        // global skills/commands/hooks (and CLAUDE.md), matching a full Claude
        // Code session — the slash-command palette then offers everything the
        // user has installed, not just project-scoped commands. (Trade-off:
        // the global CLAUDE.md + skill catalog cost context per turn.)
        "--setting-sources",
        "user,project,local",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    if let Some(m) = model.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    // Only a *non-default* mode is passed. "default" (or none) is the CLI's own
    // default, so omitting the flag keeps the invocation clean — and because
    // every mode change respawns a fresh process, omitting genuinely resets to
    // default rather than inheriting a prior mode.
    if let Some(pm) = permission_mode
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default")
    {
        args.push("--permission-mode".to_string());
        args.push(pm.to_string());
    }
    // Reasoning effort (low/medium/high/xhigh/max). Only passed when explicitly
    // chosen; omitting it lets the CLI use its own configured default.
    if let Some(ef) = effort.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--effort".to_string());
        args.push(ef.to_string());
    }
    if let Some(sid) = resume_session_id.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    // Host-declared MCP servers, passed as an inline JSON string (the CLI takes
    // "JSON files or strings"). `None` for the empty case, so no flag is added.
    if let Some(cfg) = to_claude_mcp_config(host.mcp_servers) {
        args.push("--mcp-config".to_string());
        args.push(cfg);
    }
    // Same inline form (`--settings <file-or-json>`), and the same reason: a
    // temp file would have to outlive the spawn and be cleaned up after it.
    if let Some(settings) = host.settings.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--settings".to_string());
        args.push(settings.to_string());
    }
    // Variadic (`--disallowedTools <tools...>`), so it goes last: it swallows
    // every following argument that does not begin with a dash. Nothing is
    // appended after this today — the prompt travels on stdin, not as a
    // positional — and `disallowed_tools_stays_last` keeps it that way.
    if !host.disallowed_tools.is_empty() {
        args.push("--disallowedTools".to_string());
        args.extend(host.disallowed_tools.iter().cloned());
    }
    args
}

pub struct ClaudeStreamJsonConnection {
    // `Option` so `cancel_and_wait` can close the pipe: `claude` reads its input
    // as a stream and exits at EOF, which is how a session is asked to end
    // gracefully on both platforms. `None` means the session is on its way out
    // and further writes are refused rather than panicking.
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Child>,
    // Windows stand-in for the process group `claude` would otherwise be killed
    // through. `claude` runs tools as its own children — a `bash` tool can be
    // holding a build or a dev server — and `Child::kill` ends only `claude`
    // itself, leaving those with no parent to account for them. Held for the
    // connection's life: the job's kill-on-close limit means an app crash reaps
    // the tree instead of stranding it.
    #[cfg(windows)]
    job: Option<oximux_job_object::JobObject>,
}

impl ClaudeStreamJsonConnection {
    /// Spawn `claude` in `cwd` and start streaming decoded events.
    pub fn spawn(cwd: &Path, model: Option<&str>) -> Result<(Self, Receiver<ThreadEvent>)> {
        let mut cmd = Command::new(crate::cli::program_for_spawn("claude"));
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
        permission_mode: Option<&str>,
        effort: Option<&str>,
        host: &HostInjection<'_>,
    ) -> Result<(Self, Receiver<ThreadEvent>)> {
        let mut cmd = Command::new(crate::cli::program_for_spawn("claude"));
        cmd.args(build_args_with_resume(
            model,
            session_id,
            permission_mode,
            effort,
            host,
        ))
        .current_dir(cwd);
        Self::spawn_command(cmd)
    }

    /// Spawn an already-built command (the real `claude` command, or a fake in
    /// tests) and wire stdout → `decode_line` → the returned receiver.
    pub fn spawn_command(mut cmd: Command) -> Result<(Self, Receiver<ThreadEvent>)> {
        use oximux_no_window::NoWindow as _;
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // All I/O is these pipes — never let Windows conjure a console
            // window for the CLI's whole (long) lifetime.
            .no_window();
        let mut child = cmd.spawn().context("spawn agent process")?;
        let stdout = child.stdout.take().context("agent stdout missing")?;
        let stdin = child.stdin.take().context("agent stdin missing")?;
        let stderr = child.stderr.take().context("agent stderr missing")?;

        // A bounded ring the stderr thread continuously drains into. Draining is
        // what removes a latent deadlock: an unread stderr pipe fills its ~64 KiB
        // OS buffer and blocks the child the moment it writes enough. The stdout
        // thread snapshots this ring to attach a diagnostic on error paths.
        let ring: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
        let ring_err = ring.clone();
        thread::spawn(move || drain_stderr(stderr, &ring_err));

        let (tx, rx) = mpsc::channel();
        let ring_out = ring.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break }; // read error — treat as EOF
                for ev in decode_line(&line) {
                    // Attach a best-effort stderr diagnostic just BEFORE an error
                    // turn-end (state.rs folds it into that turn's error text),
                    // de-duped against the turn's own error string + redacted.
                    if let ThreadEvent::TurnEnded { is_error: true, result, .. } = &ev
                        && let Some(diag) = take_stderr_diagnostic(&ring_out, result.as_deref())
                        && tx.send(ThreadEvent::Diagnostic(diag)).is_err()
                    {
                        return;
                    }
                    if tx.send(ev).is_err() {
                        return; // consumer gone — stop reading
                    }
                }
            }
            // stdout closed. If the child died WITHOUT emitting an error result
            // (a crash / failed exec / unexpected early exit), its stderr is the
            // only diagnostic there is — surface it directly as an Error (not a
            // stashed Diagnostic, which would have no TurnEnded to fold it). A
            // clean exit leaves stderr empty, so nothing spurious shows.
            if let Some(diag) = take_stderr_diagnostic(&ring_out, None) {
                let _ = tx.send(ThreadEvent::Error(diag));
            }
            // The sender drops here, so the consumer observes a disconnect. The
            // app treats a disconnect with a pending permission as a fail-closed
            // Reject.
        });

        // Adopted before the connection is handed out, so every later kill path
        // has a tree to end. A failure here is logged rather than fatal: the
        // agent still works, and what is lost is the guarantee about its tool
        // children, not the session.
        #[cfg(windows)]
        let job = match oximux_job_object::JobObject::adopt(&child) {
            Ok(job) => Some(job),
            Err(e) => {
                tracing::warn!(?e, "could not put claude in a job object");
                None
            }
        };

        Ok((
            Self {
                stdin: Mutex::new(Some(stdin)),
                child: Mutex::new(child),
                #[cfg(windows)]
                job,
            },
            rx,
        ))
    }

    fn write_line(&self, v: &Value) -> Result<()> {
        // Avoid `.expect()` on the lock (poison would panic the caller); map to
        // a recoverable error instead.
        let mut guard = self
            .stdin
            .lock()
            .map_err(|_| anyhow!("agent stdin lock poisoned"))?;
        let stdin = guard
            .as_mut()
            .ok_or_else(|| anyhow!("agent session is shutting down"))?;
        writeln!(stdin, "{v}").context("write to agent stdin")?;
        stdin.flush().context("flush agent stdin")?;
        Ok(())
    }
}

/// Last N bytes of child stderr retained in the diagnostic ring. Bounded so the
/// memory stays flat regardless of how noisy the child is; 8 KiB comfortably
/// holds a panic message or an API-error dump.
const STDERR_RING_CAP: usize = 8 * 1024;

/// Continuously drain the child's stderr into the bounded ring, dropping the
/// oldest bytes past the cap. Runs on its own thread for the child's lifetime;
/// exits on EOF or a read error. Draining (not the ring itself) is what prevents
/// a full-pipe deadlock.
fn drain_stderr(stderr: ChildStderr, ring: &Arc<Mutex<VecDeque<u8>>>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Ok(mut guard) = ring.lock() {
                    for &b in &buf[..n] {
                        if guard.len() >= STDERR_RING_CAP {
                            guard.pop_front();
                        }
                        guard.push_back(b);
                    }
                }
            }
        }
    }
}

/// Snapshot-and-clear the stderr ring, returning a redacted diagnostic ONLY when
/// it adds new information. `error_text` (the turn's own error string) drives the
/// de-dupe: the stale-resume stderr is byte-identical to the `errors[]`-derived
/// text, so appending both would print the same sentence twice. Returns `None`
/// when the ring is empty, whitespace-only, or a duplicate of `error_text`.
/// Best-effort: stdout and stderr are independent pipes, so the snapshot may
/// include bytes from a nearby turn (accepted — attribution is not turn-precise).
fn take_stderr_diagnostic(ring: &Arc<Mutex<VecDeque<u8>>>, error_text: Option<&str>) -> Option<String> {
    let raw: Vec<u8> = {
        let mut guard = ring.lock().ok()?;
        if guard.is_empty() {
            return None;
        }
        guard.drain(..).collect()
    };
    let text = String::from_utf8_lossy(&raw).trim().to_string();
    if text.is_empty() {
        return None;
    }
    if let Some(err) = error_text {
        let err = err.trim();
        if err == text || err.contains(&text) {
            return None; // stderr duplicates the turn's own error text
        }
    }
    Some(redact_secrets(&text))
}

/// Strip secret-shaped values from a diagnostic before it reaches any UI or
/// persisted surface. The child inherits OxiMux's full environment (no
/// `env_clear`), so an auth/config error could echo a live token. Redacts (a)
/// the value of every currently-set `*_API_KEY` / `*_TOKEN` / `*_SECRET` env var
/// and (b) known secret-shaped tokens (`sk-ant-…`, `sk-…`).
fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for (name, value) in std::env::vars() {
        let upper = name.to_ascii_uppercase();
        let secretish = upper.ends_with("_API_KEY")
            || upper.ends_with("_TOKEN")
            || upper.ends_with("_SECRET");
        // Guard on a minimum length so a short/empty value (e.g. `TOKEN=""` or a
        // one-char flag) can't blanket-replace common substrings.
        if secretish && value.len() >= 8 && out.contains(&value) {
            out = out.replace(&value, "[redacted]");
        }
    }
    redact_prefixed(&mut out, "sk-ant-");
    redact_prefixed(&mut out, "sk-");
    out
}

/// Replace every `prefix`-led token (the prefix plus the following run of
/// non-whitespace) with `[redacted]`. Terminates because the replacement never
/// contains `prefix`.
fn redact_prefixed(text: &mut String, prefix: &str) {
    while let Some(pos) = text.find(prefix) {
        let rest = &text[pos + prefix.len()..];
        let tok_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let end = pos + prefix.len() + tok_len;
        text.replace_range(pos..end, "[redacted]");
    }
}

impl AgentConnection for ClaudeStreamJsonConnection {
    fn send_user_message(&self, text: &str) -> Result<()> {
        self.write_line(&user_message_json(text))
    }

    fn send_user_message_with_images(&self, text: &str, images: &[ChatImage]) -> Result<()> {
        self.write_line(&user_message_json_with_images(text, images))
    }

    fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        self.write_line(&control_response_json(request_id, &decision))
    }

    /// Switch permission mode in place via a `set_permission_mode` control request
    /// on stdin (the SDK's wire) — so a composer mode pick applies to the SAME
    /// session/PID with no `--resume` respawn. `Ok` tells the app to skip the
    /// respawn fallback.
    fn set_mode(&self, mode: &str) -> Result<()> {
        self.write_line(&set_permission_mode_json(mode))
    }

    fn answer_question(
        &self,
        request_id: &str,
        questions: &[AskQuestion],
        answers: &QuestionAnswers,
    ) -> Result<()> {
        self.write_line(&question_answer_json(request_id, questions, answers))
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            supports_modes: true,   // permission modes (acceptEdits, …)
            supports_slash: true,   // system/init advertises slash_commands
            supports_config: true,  // reasoning effort via `--effort <level>`
            emits_usage: true,      // result + stream_event carry token/cost usage
            supports_rewind: true,  // keeps ~/.claude/projects/*.jsonl for truncate-fork
            supports_steer: false,  // stdin mid-turn has no queue to land in
        }
    }

    fn models(&self) -> Vec<ModelChoice> {
        claude_model_choices()
    }

    fn permission_modes(&self) -> Vec<ModeChoice> {
        CLAUDE_PERMISSION_MODES
            .iter()
            .map(|(w, l)| ModeChoice { wire: (*w).to_string(), label: (*l).to_string() })
            .collect()
    }

    fn efforts(&self) -> Vec<EffortChoice> {
        CLAUDE_EFFORTS
            .iter()
            .map(|(w, l)| EffortChoice { wire: (*w).to_string(), label: (*l).to_string() })
            .collect()
    }

    fn default_model(&self) -> Option<String> {
        Some(DEFAULT_MODEL.to_string())
    }

    fn default_mode(&self) -> Option<String> {
        Some(DEFAULT_PERMISSION_MODE.to_string())
    }

    fn default_effort(&self) -> Option<String> {
        Some(DEFAULT_EFFORT.to_string())
    }

    /// Interrupt the in-flight turn with a stdin `control_request`.
    ///
    /// `claude` ends the turn, checkpoints the session, and — unlike the SIGINT
    /// this replaced — **stays alive**, so the next send continues the same
    /// process instead of respawning it with `--resume`. One code path on every
    /// platform: the signal version had no Windows counterpart, and the hard
    /// kill standing in for it there destroyed the very checkpoint that makes a
    /// session resumable.
    fn cancel(&self) -> Result<()> {
        self.write_line(&interrupt_json())
    }

    /// Interrupt, then block until the child is actually reaped.
    ///
    /// Callers use this before reading the agent's on-disk session file
    /// (rewind's truncate-fork), so "the turn ended" is not enough — the
    /// transcript is only certainly flushed once the process is gone.
    ///
    /// Which is why closing stdin is part of this and not of `cancel`: the
    /// interrupt deliberately leaves the process running, so something has to
    /// ask it to leave. `claude` reads its input as a stream and exits at EOF,
    /// so dropping the pipe is the graceful way to say so — and it works on
    /// both platforms, where a signal did not.
    fn cancel_and_wait(&self) -> Result<()> {
        // Best-effort: if the turn already ended, or the pipe is already gone,
        // the reap below still does its job — so neither step bails.
        let _ = self.cancel();
        {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| anyhow!("agent stdin lock poisoned"))?;
            // Dropping the writer closes the pipe; the child sees EOF and exits.
            let _ = stdin.take();
        }
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow!("agent child lock poisoned"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait().context("reap agent process")?.is_some() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                // `kill` ends `claude` alone; on Windows the tools it launched
                // are separate processes that outlive it, so the tree has to be
                // ended explicitly.
                #[cfg(windows)]
                if let Some(job) = &self.job {
                    let _ = job.kill();
                }
                return Ok(());
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
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
        // Same reason as the escalation above: killing `claude` says nothing
        // about the tools it started.
        #[cfg(windows)]
        if let Some(job) = &self.job {
            let _ = job.kill();
        }
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
            "user,project,local",
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
        let a = build_args_with_resume(None, Some("sid-123"), None, None, &HostInjection::default());
        let i = a.iter().position(|x| x == "--resume").expect("--resume present");
        assert_eq!(a[i + 1], "sid-123");
        // blank / absent session id → no --resume
        assert!(!build_args_with_resume(None, Some("  "), None, None, &HostInjection::default()).iter().any(|x| x == "--resume"));
        assert!(!build_args_with_resume(None, None, None, None, &HostInjection::default()).iter().any(|x| x == "--resume"));
        // plain build_args never resumes
        assert!(!build_args(None).iter().any(|x| x == "--resume"));
    }

    #[test]
    fn build_args_appends_permission_mode_only_when_non_default() {
        let a = build_args_with_resume(None, None, Some("acceptEdits"), None, &HostInjection::default());
        let i = a.iter().position(|x| x == "--permission-mode").expect("--permission-mode present");
        assert_eq!(a[i + 1], "acceptEdits");
        // "default", blank, and none all omit the flag (a fresh spawn IS default).
        for pm in [Some("default"), Some("  "), None] {
            assert!(
                !build_args_with_resume(None, None, pm, None, &HostInjection::default()).iter().any(|x| x == "--permission-mode"),
                "{pm:?} must not emit --permission-mode"
            );
        }
        // plain build_args never sets a mode
        assert!(!build_args(None).iter().any(|x| x == "--permission-mode"));
    }

    #[test]
    fn no_host_injection_emits_no_flags() {
        // The load-bearing invariant of the host seam: a launch that injects
        // nothing must produce EXACTLY the argv it produced before the seam
        // existed. Anything else silently changes every existing session.
        let bare = build_args_with_resume(None, None, None, None, &HostInjection::default());
        for flag in ["--mcp-config", "--settings", "--disallowedTools"] {
            assert!(!bare.iter().any(|x| x == flag), "{flag} in {bare:?}");
        }
        assert_eq!(bare, build_args(None));
    }

    #[test]
    fn settings_are_passed_inline_and_only_once() {
        // Inline JSON rather than a temp file, and exactly one flag: the CLI's
        // behaviour with two `--settings` is undocumented, so anything else
        // wanting to inject settings has to merge into this one.
        let json = r#"{"hooks":{"PreToolUse":[]}}"#;
        let a = build_args_with_resume(
            None,
            None,
            None,
            None,
            &HostInjection {
                settings: Some(json),
                ..Default::default()
            },
        );
        let i = a.iter().position(|x| x == "--settings").expect("--settings present");
        assert_eq!(a.iter().filter(|x| *x == "--settings").count(), 1);
        assert_eq!(a[i + 1], json);
        // Still additive: the user's own settings keep loading.
        let j = a.iter().position(|x| x == "--setting-sources").expect("sources");
        assert_eq!(a[j + 1], "user,project,local");

        // Blank is the same as absent, so an empty declaration cannot emit a
        // flag with nothing after it.
        assert!(
            !build_args_with_resume(
                None,
                None,
                None,
                None,
                &HostInjection { settings: Some("  "), ..Default::default() },
            )
            .iter()
            .any(|x| x == "--settings")
        );
    }

    #[test]
    fn disallowed_tools_stays_last() {
        // `--disallowedTools <tools...>` is variadic: it swallows every
        // following argument that does not start with a dash. A flag appended
        // after it would be read as another tool name and silently lost.
        let tools = vec![
            "mcp__oximux-computer-use__replay_trajectory".to_string(),
            "mcp__oximux-computer-use__get_desktop_state".to_string(),
        ];
        let spec = McpServerSpec::new("srv", "bin");
        let a = build_args_with_resume(
            Some("opus"),
            Some("sid-1"),
            Some("acceptEdits"),
            Some("high"),
            &HostInjection {
                mcp_servers: std::slice::from_ref(&spec),
                settings: Some("{}"),
                disallowed_tools: &tools,
            },
        );
        let i = a
            .iter()
            .position(|x| x == "--disallowedTools")
            .expect("--disallowedTools present");
        assert_eq!(&a[i + 1..], &tools[..], "every name, and nothing after them");
    }

    /// Payload shape verified end-to-end against claude-code 2.1.220: a config
    /// of this exact form spawned a stdio server, its tools were discovered,
    /// and a call round-tripped. If a CLI bump breaks `--mcp-config`, this test
    /// still passes (it only checks what we emit) — re-run the live probe.
    #[test]
    fn mcp_servers_emit_one_config_flag() {
        let spec = McpServerSpec::new("oximux-computer-use", "cua-driver")
            .args(vec!["mcp".into(), "--socket".into(), "/tmp/s.sock".into()]);
        let a = build_args_with_resume(None, None, None, None, &HostInjection { mcp_servers: std::slice::from_ref(&spec), ..Default::default() });

        let i = a.iter().position(|x| x == "--mcp-config").expect("--mcp-config present");
        // One flag, one JSON payload — not a file path, not repeated per server.
        assert_eq!(a.iter().filter(|x| *x == "--mcp-config").count(), 1);
        let cfg: Value = serde_json::from_str(&a[i + 1]).expect("payload is valid json");
        assert_eq!(cfg["mcpServers"]["oximux-computer-use"]["command"], "cua-driver");

        // Never paired with --strict-mcp-config: that would suppress the user's
        // own servers, which `--setting-sources user,project,local` exists to load.
        assert!(!a.iter().any(|x| x == "--strict-mcp-config"));
    }

    #[test]
    fn mcp_servers_survive_a_resume() {
        // A restored chat respawns through this same builder, so the servers
        // must ride the resume path too — otherwise computer use silently
        // disappears the first time a tab is reopened.
        let spec = McpServerSpec::new("srv", "bin");
        let a = build_args_with_resume(None, Some("sid-9"), None, None, &HostInjection { mcp_servers: std::slice::from_ref(&spec), ..Default::default() });
        assert!(a.iter().any(|x| x == "--resume"));
        assert!(a.iter().any(|x| x == "--mcp-config"));
    }

    #[test]
    fn build_args_appends_effort_when_set() {
        let a = build_args_with_resume(None, None, None, Some("xhigh"), &HostInjection::default());
        let i = a.iter().position(|x| x == "--effort").expect("--effort present");
        assert_eq!(a[i + 1], "xhigh");
        // blank / none omit the flag (CLI uses its configured default)
        assert!(!build_args_with_resume(None, None, None, Some("  "), &HostInjection::default()).iter().any(|x| x == "--effort"));
        assert!(!build_args_with_resume(None, None, None, None, &HostInjection::default()).iter().any(|x| x == "--effort"));
        assert!(!build_args(None).iter().any(|x| x == "--effort"));
    }

    /// The Claude connection advertises exactly the vocab the pickers expect
    /// (moved here from the app crate). Accessors ignore `self`, so a trivially
    /// spawned connection exercises them without a real `claude`.
    #[test]
    fn claude_vocab_matches_expected() {
        let (conn, _rx) =
            ClaudeStreamJsonConnection::spawn_command(Command::new("true")).expect("spawn");
        let models: Vec<String> = conn.models().into_iter().map(|m| m.wire).collect();
        assert_eq!(models, vec!["opus", "fable", "sonnet", "haiku"]);
        // Every blurb leads with the versioned name, which is the only place the
        // picker can show *which* Opus an alias resolves to.
        for choice in conn.models() {
            let blurb = choice.description.expect("every model carries a blurb");
            assert!(
                blurb.starts_with(&choice.label),
                "{} leads its blurb with the versioned name, got {blurb:?}",
                choice.label,
            );
            assert!(
                blurb.contains(" · "),
                "{} separates version from capability, got {blurb:?}",
                choice.label,
            );
        }
        assert_eq!(conn.default_model().as_deref(), Some("sonnet"));
        let modes: Vec<(String, String)> =
            conn.permission_modes().into_iter().map(|m| (m.wire, m.label)).collect();
        assert_eq!(
            modes,
            vec![
                ("default".to_string(), "Ask each time".to_string()),
                ("acceptEdits".to_string(), "Accept edits".to_string()),
                ("plan".to_string(), "Plan mode".to_string()),
                ("bypassPermissions".to_string(), "Bypass all".to_string()),
            ]
        );
        assert_eq!(conn.default_mode().as_deref(), Some("default"));
        let efforts: Vec<String> = conn.efforts().into_iter().map(|e| e.wire).collect();
        assert_eq!(efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(conn.default_effort().as_deref(), Some("high"));
        assert!(conn.capabilities().supports_rewind);
    }

    #[test]
    fn redact_secrets_strips_env_values_and_sk_tokens() {
        // A planted secret-suffixed env var value must be stripped from a
        // diagnostic; so must a bare sk-ant token that isn't sourced from env.
        // SAFETY: single-threaded test, restored immediately after.
        unsafe { std::env::set_var("OXIMUX_TEST_API_KEY", "supersecretvalue12345") };
        let dirty = "auth failed for key supersecretvalue12345 and token sk-ant-abc123XYZ.";
        let clean = redact_secrets(dirty);
        unsafe { std::env::remove_var("OXIMUX_TEST_API_KEY") };
        assert!(!clean.contains("supersecretvalue12345"), "env secret leaked: {clean}");
        assert!(!clean.contains("sk-ant-abc123XYZ"), "sk-ant token leaked: {clean}");
        assert!(clean.contains("[redacted]"));
        // A too-short env value must not blanket-replace common substrings.
        unsafe { std::env::set_var("OXIMUX_TEST_TOKEN", "ab") };
        let benign = redact_secrets("about the abstract abbey");
        unsafe { std::env::remove_var("OXIMUX_TEST_TOKEN") };
        assert_eq!(benign, "about the abstract abbey", "short value must not redact");
    }

    #[test]
    fn take_stderr_diagnostic_dedupes_against_error_text() {
        // stderr byte-identical to the turn's error text → no diagnostic (the
        // stale-resume case, where appending both prints the sentence twice).
        let err = "No conversation found with session ID: x";
        let ring: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(err.bytes().collect()));
        assert_eq!(take_stderr_diagnostic(&ring, Some(err)), None, "duplicate stderr suppressed");
        // Distinct stderr → returned (redacted). Ring re-filled since drain clears.
        let ring2: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(b"unrelated panic: boom".to_vec().into()));
        assert_eq!(take_stderr_diagnostic(&ring2, Some(err)).as_deref(), Some("unrelated panic: boom"));
        // Empty ring → None.
        let empty: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
        assert_eq!(take_stderr_diagnostic(&empty, None), None);
    }

    /// A child that floods stderr with far more than the OS pipe buffer (~64 KiB)
    /// before exiting must NOT deadlock — proves the drain thread keeps the pipe
    /// clear. The ring keeps only a bounded tail.
    #[test]
    fn large_stderr_does_not_block_child() {
        let mut cmd = Command::new("sh");
        // 200 KiB to stderr, one stream-json result line to stdout, then exit.
        cmd.arg("-c").arg(
            "yes ERRORLINE | head -c 200000 1>&2; \
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"ok\"}'",
        );
        let (_conn, rx) = ClaudeStreamJsonConnection::spawn_command(cmd).expect("spawn");
        let mut saw_turn_end = false;
        while let Ok(ev) = rx.recv_timeout(Duration::from_secs(10)) {
            if matches!(ev, ThreadEvent::TurnEnded { .. }) {
                saw_turn_end = true;
            }
        }
        assert!(saw_turn_end, "child that floods stderr still reaches TurnEnded (no deadlock)");
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
