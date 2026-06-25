//! Agent status hooks for Claude Code (on by default).
//!
//! On by default (`status_hooks_enabled` in `agent_launch.toml` defaults to
//! `true`); disabled via the **Settings → Agents** "Status hooks" toggle. The
//! env var `OXIMUX_STATUS_HOOKS=1` force-enables regardless of the flag (a
//! debug escape hatch). When on, a Claude Code agent is launched with a
//! `--settings` block that wires four hooks to the `oximux agent-status` CLI:
//!
//! - `UserPromptSubmit` → `--state working` (`{"state":"working","prompt":<text>}`)
//!   — fires the instant the user submits, carrying the prompt that becomes the
//!   agent's rail title and flipping the dot to working right away.
//! - `PreToolUse`   → `--state working` (`{"state":"working","tool":<name>}`)
//! - `Notification` → `--state needs_approval --filter-notification` — Claude
//!   fires `Notification` (NOT `PermissionRequest`, which never fires) when it
//!   needs the user to answer a tool-permission prompt. The `--filter-notification`
//!   flag makes the CLI emit only when the payload's `notification_type` is a
//!   permission prompt, ignoring the benign "waiting for your input" nudge.
//! - `Stop`         → `--state idle` (`{"state":"idle"}`)
//!
//! The CLI reads the hook event JSON on stdin (for the tool name / prompt),
//! reads `OXIMUX_PTY_ID` (injected by the relay at spawn), and asks the relay to
//! emit an OSC-9999 status packet on that PTY's output stream. OxiMux's scanner
//! (`oximux-agents` `osc_sideband`) decodes it into structured agent status.
//!
//! Why a relay round-trip and not a `/dev/tty` write: Claude runs hook commands
//! detached (new session, no controlling terminal), so `/dev/tty` is `ENXIO`.
//! Routing the status back through the relay — keyed by the env-injected pane
//! id — is the only path that works for a hook. (the reference UX and the reference cockpit solve the same
//! constraint the same way: a callback to the app over an IPC channel.)
//!
//! Design notes:
//! - **App-owned, non-destructive.** The hooks are passed as a `--settings`
//!   JSON STRING at spawn, never written into the user's `~/.claude` config.
//!   Because `--settings` replaces (not deep-merges) the `hooks` key, we read
//!   the user's existing global hooks and merge ours in, so theirs keep firing.
//! - **On by default.** The cockpit's status sideband; the Settings toggle (or
//!   an explicit `status_hooks_enabled = false`) opts out.
//! - **`Stop` → `idle`, not `done`.** A finished turn is not a dead process;
//!   the terminal `Done` state comes from the PTY exit event, not a hook.

use std::path::Path;

use serde_json::{Value, json};

const ENABLE_ENV: &str = "OXIMUX_STATUS_HOOKS";

/// Cap the reported tool name so a pathological hook payload can't bloat the
/// OSC-9999 packet. The scanner caps again, but trimming at the source is free.
const MAX_TOOL_LEN: usize = 64;

/// Cap the captured prompt at the source. The scanner caps again (256 bytes);
/// trimming here keeps the OSC-9999 packet small and bounds a giant paste.
const MAX_PROMPT_LEN: usize = 200;

/// Cap the captured last-assistant message (the row's secondary text for a
/// finished turn). The scanner caps again (512 bytes); the rail truncates the
/// rendered line, so a tight source cap keeps the OSC-9999 packet small.
const MAX_MSG_LEN: usize = 200;

/// True when the env override forces status hooks on (`OXIMUX_STATUS_HOOKS=1`),
/// independent of the persisted Settings toggle. A debug escape hatch — the
/// primary control is the `status_hooks_enabled` setting, OR-combined with this
/// at the injection site.
pub fn env_forced() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Build the `--settings` JSON string wiring the three status hooks to
/// `oximux agent-status`, merging the user's existing global hooks so the
/// key-replace semantics of `--settings` don't disable them. `binary_path` is
/// the absolute path to the running `oximux` binary (resolved via
/// `current_exe`) — the hook invokes it as a short-lived CLI.
pub fn build_settings_json(binary_path: &Path) -> String {
    build_settings_json_with(read_user_hooks(), binary_path)
}

fn build_settings_json_with(user_hooks: Option<Value>, binary_path: &Path) -> String {
    let mut hooks = user_hooks
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    for spec in status_hook_specs(binary_path) {
        append_hook(&mut hooks, spec.event, spec.matcher, &spec.command);
    }
    json!({ "hooks": hooks }).to_string()
}

/// One status hook: which Claude event drives it, an optional tool matcher, and
/// the `oximux agent-status` command line it runs.
pub(crate) struct HookSpec {
    pub event: &'static str,
    pub matcher: Option<&'static str>,
    pub command: String,
}

/// The four status hooks wiring Claude events to the `oximux agent-status` CLI.
///
/// The single source of truth for both the per-spawn `--settings` JSON and the
/// global `~/.claude/settings.json` install — so the COMMAND STRINGS are
/// byte-identical and Claude's command-string hook dedup makes a picker-launched
/// agent (which sees both) fire each hook exactly once.
pub(crate) fn status_hook_specs(binary_path: &Path) -> Vec<HookSpec> {
    // The command single-quotes the binary path (an installed app bundle path
    // can contain spaces, e.g. "Application Support") then appends the CLI
    // subcommand. Escape any embedded single quote (`'` → `'\''`) so a home dir
    // like `/Users/O'X` can't break out of the quoting into shell injection.
    let quoted = binary_path.display().to_string().replace('\'', "'\\''");
    let cmd = |state: &str| format!("'{quoted}' agent-status --state {state}");
    vec![
        HookSpec {
            event: "PreToolUse",
            matcher: Some("*"),
            command: cmd("working"),
        },
        // `UserPromptSubmit` fires the instant the user submits a prompt —
        // whether typed into the agent's own TUI or sent from OxiMux. It carries
        // the prompt text, captured as the agent's rail title, and flips the dot
        // to working immediately (a text-only reply that calls no tool would
        // otherwise look idle for its whole turn). No matcher (like `Stop`).
        HookSpec {
            event: "UserPromptSubmit",
            matcher: None,
            command: cmd("working"),
        },
        // `Notification` (no matcher — like `Stop`) is the event Claude actually
        // fires for a tool-permission prompt; `PermissionRequest` is a dead name
        // in current Claude. `--filter-notification` gates the emit on the
        // payload's `notification_type` so only a real permission ask reports
        // needs_approval.
        HookSpec {
            event: "Notification",
            matcher: None,
            command: format!("'{quoted}' agent-status --state needs_approval --filter-notification"),
        },
        HookSpec {
            event: "Stop",
            matcher: None,
            command: cmd("idle"),
        },
    ]
}

/// Append one `{matcher?, hooks:[{type:command, command, async}]}` entry to the
/// `event` array inside the `hooks` object, creating the array if absent. Any
/// existing entries (the user's own hooks) are preserved.
fn append_hook(hooks: &mut Value, event: &str, matcher: Option<&str>, command: &str) {
    let mut entry = serde_json::Map::new();
    if let Some(m) = matcher {
        entry.insert("matcher".into(), json!(m));
    }
    entry.insert(
        "hooks".into(),
        json!([{ "type": "command", "command": command, "async": true }]),
    );
    // `hooks` is always an object here (filtered/defaulted by the caller).
    if let Some(obj) = hooks.as_object_mut() {
        let arr = obj.entry(event.to_string()).or_insert_with(|| json!([]));
        // A user `hooks.<event>` that isn't an array (malformed settings) would
        // otherwise swallow our entry silently — coerce it to a fresh array so
        // our hook still runs. The malformed value is dropped (Claude would
        // reject it anyway); the user's well-formed entries are unaffected.
        if !arr.is_array() {
            *arr = json!([]);
        }
        if let Some(a) = arr.as_array_mut() {
            a.push(Value::Object(entry));
        }
    }
}

/// Read the user's global Claude hooks (`~/.claude/settings.json` → `hooks`),
/// or `None` when absent/unparseable. Best-effort: a missing or malformed file
/// just means we ship only our hooks (logged at debug).
fn read_user_hooks() -> Option<Value> {
    let path = dirs::home_dir()?.join(".claude").join("settings.json");
    // Absent file is the common case (no global settings) — silent.
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => value.get("hooks").cloned().filter(Value::is_object),
        Err(err) => {
            // Present but unparseable: ship only our hooks, but say so — a
            // silent drop of the user's hooks would be hard to diagnose.
            tracing::debug!(%err, "status-hooks: ~/.claude/settings.json parse failed; shipping our hooks only");
            None
        }
    }
}

/// If status hooks are enabled, prepend `--settings <json>` to `extra_args` for
/// a Claude Code launch. `settings_enabled` is the persisted Settings toggle;
/// the env override (`env_forced`) turns hooks on regardless. No-op when both
/// are off or the binary can't be resolved.
pub fn maybe_inject(settings_enabled: bool, extra_args: &mut Vec<String>) {
    if !(settings_enabled || env_forced()) {
        return;
    }
    let binary_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(%err, "status-hooks: enabled but current_exe failed; skipping injection");
            return;
        }
    };
    if extra_args.iter().any(|a| a == "--settings") {
        // The user already configured a --settings flag in agent_launch.toml.
        // claude's behavior with two is undocumented; surface the conflict.
        tracing::warn!(
            "status-hooks: a --settings flag is already configured; a second one may be ignored by claude"
        );
    }
    let json = build_settings_json(&binary_path);
    // Prepend so it precedes any positional prompt build_command appends; order
    // relative to the user's own flags does not matter to `claude`.
    extra_args.insert(0, "--settings".to_string());
    extra_args.insert(1, json);
}

/// Extract `tool_name` from a Claude hook event JSON document. Best-effort: a
/// parse failure or missing/empty field yields `None` (status still reports,
/// just without a tool). The result is capped at [`MAX_TOOL_LEN`].
pub fn tool_name_from_hook_json(stdin_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdin_json).ok()?;
    let name = value.get("tool_name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    Some(name.chars().take(MAX_TOOL_LEN).collect())
}

/// True when a Claude `Notification` hook payload is a tool-permission prompt
/// (the agent is blocked, waiting for the user to approve), as opposed to a
/// benign notification such as the idle "Claude is waiting for your input"
/// nudge. The `--filter-notification` CLI path calls this to decide whether to
/// emit `needs_approval` at all.
///
/// Primary signal: the stable typed `notification_type` field
/// (`"permission_prompt"` for tool asks — captured from Claude 2.1.x). Fallback:
/// a narrow scan of the human `message` for permission/trust wording, in case
/// the typed field changes. A parse failure or no match yields `false` so a
/// non-permission notification never spuriously flips the dot to amber.
pub fn notification_is_permission(stdin_json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(stdin_json) else {
        return false;
    };
    if value.get("notification_type").and_then(Value::as_str) == Some("permission_prompt") {
        return true;
    }
    value
        .get("message")
        .and_then(Value::as_str)
        .map(|m| {
            let m = m.to_ascii_lowercase();
            // "permission" covers "Claude needs your permission"; "do you trust"
            // covers the workspace-trust dialog. The idle nudge ("waiting for
            // your input") contains neither, so it is correctly ignored.
            m.contains("permission") || m.contains("do you trust")
        })
        .unwrap_or(false)
}

/// Extract the user's `prompt` from a Claude `UserPromptSubmit` hook event
/// JSON. Best-effort: a parse failure or missing/empty field yields `None`.
/// Whitespace-trimmed and capped at [`MAX_PROMPT_LEN`] chars (the scanner caps
/// again in bytes) so a giant paste can't bloat the OSC-9999 packet.
pub fn prompt_from_hook_json(stdin_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdin_json).ok()?;
    let prompt = value.get("prompt")?.as_str()?.trim();
    if prompt.is_empty() {
        return None;
    }
    Some(prompt.chars().take(MAX_PROMPT_LEN).collect())
}

/// Extract the agent's most recent assistant text reply from a `Stop` hook
/// event. Two sources, in the reference cockpit's order. First, the
/// `last_assistant_message` field Claude puts directly on the Stop event: it is
/// populated synchronously with the hook, so it avoids both a file read and the
/// transcript-flush RACE — the Stop hook can fire before the turn's final
/// assistant line is written to the JSONL, which left the row blank. Second, as
/// a fallback (older Claude builds, or an event without the direct field), the
/// `transcript_path` JSONL tail: the last `type:"assistant"` line whose
/// `message.content` carries a text part.
///
/// Best-effort: any IO/parse failure yields `None`. Whitespace is collapsed to
/// one line and capped at [`MAX_MSG_LEN`] chars (the scanner caps again in
/// bytes). This becomes the row's secondary text for a finished turn — the
/// reference cockpit's `lastAssistantMessage`.
pub fn last_assistant_message_from_hook_json(stdin_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdin_json).ok()?;
    // 1. The reply handed to us directly — preferred, race-free.
    if let Some(msg) = value
        .get("last_assistant_message")
        .and_then(Value::as_str)
        .and_then(normalize_message)
    {
        return Some(msg);
    }
    // 2. Fallback: scan the transcript tail. `str::lines()` is double-ended, so
    // `.rev()` walks newest-first without allocating a Vec; we stop at the first
    // assistant line that has text.
    let path = value.get("transcript_path")?.as_str()?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let content = entry.get("message").and_then(|m| m.get("content"));
        if let Some(msg) = assistant_text_from_content(content) {
            return Some(msg);
        }
    }
    None
}

/// Collapse whitespace to a single line and cap at [`MAX_MSG_LEN`] chars.
/// `None` when nothing is left after trimming.
fn normalize_message(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(collapsed.chars().take(MAX_MSG_LEN).collect())
}

/// Join the `text` parts of an assistant message's `content` array into one
/// collapsed, capped line. Assistant turns interleave `text` and `tool_use`
/// parts; only the text is human-facing. `None` when there is no text part.
fn assistant_text_from_content(content: Option<&Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let mut out = String::new();
    for part in arr {
        if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = part.get("text").and_then(Value::as_str)
        {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(t);
        }
    }
    normalize_message(&out)
}

/// Build the OSC-9999 inner JSON payload (`{"v":1,"state":..,"tool":..}`) the
/// relay wraps and the scanner decodes. Serialized via `serde_json` so control
/// characters in `tool`/`prompt`/`msg` are escaped — the relay treats the
/// result as opaque. `prompt` is present only on `UserPromptSubmit`; `msg` (the
/// last assistant reply) only on `Stop`.
pub fn build_status_payload(
    state: &str,
    tool: Option<&str>,
    prompt: Option<&str>,
    message: Option<&str>,
) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("v".into(), json!(1));
    obj.insert("state".into(), json!(state));
    if let Some(t) = tool {
        obj.insert("tool".into(), json!(t));
    }
    if let Some(p) = prompt {
        obj.insert("prompt".into(), json!(p));
    }
    if let Some(m) = message {
        obj.insert("msg".into(), json!(m));
    }
    Value::Object(obj).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::AgentOscScanner;
    use oximux_core::AgentSidebandState;

    fn binary_path() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    #[test]
    fn settings_json_wires_four_events_to_agent_status_cli() {
        let json = build_settings_json_with(None, binary_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let hooks = &v["hooks"];
        assert!(hooks["PreToolUse"].is_array());
        assert!(hooks["Notification"].is_array());
        assert!(hooks["Stop"].is_array());
        // UserPromptSubmit (no matcher, like Stop) reports working and carries
        // the prompt the CLI reads from stdin.
        assert!(hooks["UserPromptSubmit"].is_array());
        assert!(hooks["UserPromptSubmit"][0].get("matcher").is_none());
        assert!(
            hooks["UserPromptSubmit"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" agent-status --state working")
        );
        // PreToolUse: matcher "*", async true, command single-quotes the binary
        // path and calls `agent-status --state working`.
        let pre = &hooks["PreToolUse"][0];
        assert_eq!(pre["matcher"], "*");
        let cmd = pre["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.starts_with('\''), "path must be single-quoted: {cmd}");
        assert!(cmd.ends_with(" agent-status --state working"), "{cmd}");
        assert_eq!(pre["hooks"][0]["async"], true);
        // Stop has no matcher (Stop has no matcher support) and reports idle.
        assert!(hooks["Stop"][0].get("matcher").is_none());
        assert!(
            hooks["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" agent-status --state idle")
        );
        // Notification (no matcher, like Stop) reports needs_approval and is
        // gated by --filter-notification so only permission asks emit.
        assert!(hooks["Notification"][0].get("matcher").is_none());
        assert!(
            hooks["Notification"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" agent-status --state needs_approval --filter-notification")
        );
    }

    #[test]
    fn notification_permission_type_is_detected() {
        // The exact payload captured from a live Claude 2.1.x permission prompt.
        let payload = r#"{"hook_event_name":"Notification","message":"Claude needs your permission","notification_type":"permission_prompt"}"#;
        assert!(notification_is_permission(payload));
    }

    #[test]
    fn notification_message_fallback_matches_permission_wording() {
        // No typed field — the message-keyword fallback still fires.
        assert!(notification_is_permission(
            r#"{"message":"Claude needs your permission to use Bash"}"#
        ));
        assert!(notification_is_permission(
            r#"{"message":"Do you trust the files in this folder?"}"#
        ));
    }

    #[test]
    fn notification_idle_nudge_is_not_permission() {
        // The benign "waiting for input" notification must NOT flip to amber.
        assert!(!notification_is_permission(
            r#"{"message":"Claude is waiting for your input","notification_type":"idle"}"#
        ));
        assert!(!notification_is_permission("not json"));
        assert!(!notification_is_permission(r#"{"hook_event_name":"Notification"}"#));
    }

    #[test]
    fn user_hooks_are_preserved_not_replaced() {
        let user = json!({
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-thing" }] }
            ]
        });
        let json = build_settings_json_with(Some(user), binary_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user hook + ours");
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        assert!(
            pre[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" agent-status --state working")
        );
    }

    #[test]
    fn non_object_user_hooks_falls_back_to_ours_only() {
        let json = build_settings_json_with(Some(json!([1, 2, 3])), binary_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["hooks"]["PreToolUse"].is_array());
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_event_value_is_coerced_not_dropped() {
        let user = json!({ "PreToolUse": { "oops": "not an array" } });
        let json = build_settings_json_with(Some(user), binary_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let pre = v["hooks"]["PreToolUse"]
            .as_array()
            .expect("coerced to array");
        assert_eq!(pre.len(), 1, "our hook survives the malformed entry");
    }

    #[test]
    fn embedded_single_quote_in_path_is_escaped() {
        let json = build_settings_json_with(None, Path::new("/Users/O'X/oximux"));
        let v: Value = serde_json::from_str(&json).unwrap();
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("'\\''"), "single quote must be shell-escaped: {cmd}");
    }

    #[test]
    fn tool_name_extracted_from_pre_tool_use_json() {
        let stdin = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        assert_eq!(tool_name_from_hook_json(stdin).as_deref(), Some("Bash"));
    }

    #[test]
    fn tool_name_absent_or_malformed_yields_none() {
        assert_eq!(tool_name_from_hook_json(r#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(tool_name_from_hook_json("not json"), None);
        assert_eq!(tool_name_from_hook_json(r#"{"tool_name":""}"#), None);
    }

    /// End-to-end format proof (minus live Claude + relay): the payload the CLI
    /// builds, once wrapped in the OSC-9999 envelope the relay adds, is exactly
    /// what the Phase-1 scanner decodes.
    #[test]
    fn payload_round_trips_through_scanner() {
        let payload = build_status_payload("working", Some("Bash"), None, None);
        // Relay envelope: ESC ] 9999 ; <payload> BEL.
        let mut bytes = b"\x1b]9999;".to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.push(0x07);

        let mut scanner = AgentOscScanner::new();
        let scan = scanner.feed(&bytes);
        let ev = scan.event.expect("scanner decoded a sideband event");
        assert_eq!(ev.state, AgentSidebandState::Working);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Bash"));
        assert!(scan.cleaned.is_empty(), "OSC bytes fully stripped");
    }

    /// End-to-end proof for the prompt path: the payload the CLI builds from a
    /// `UserPromptSubmit` hook, wrapped in the relay's OSC-9999 envelope, is
    /// decoded by the scanner with the prompt intact.
    #[test]
    fn prompt_payload_round_trips_through_scanner() {
        let prompt = prompt_from_hook_json(
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"refactor the auth module"}"#,
        );
        let payload = build_status_payload("working", None, prompt.as_deref(), None);
        let mut bytes = b"\x1b]9999;".to_vec();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.push(0x07);

        let mut scanner = AgentOscScanner::new();
        let ev = scanner.feed(&bytes).event.expect("scanner decoded event");
        assert_eq!(ev.state, AgentSidebandState::Working);
        assert_eq!(ev.detail.prompt.as_deref(), Some("refactor the auth module"));
    }

    #[test]
    fn idle_payload_has_no_tool() {
        let payload = build_status_payload("idle", None, None, None);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["state"], "idle");
        assert!(v.get("tool").is_none());
        assert!(v.get("prompt").is_none());
        assert!(v.get("msg").is_none());
    }

    #[test]
    fn prompt_is_extracted_trimmed_and_capped() {
        let stdin =
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"  fix the parser bug  "}"#;
        assert_eq!(
            prompt_from_hook_json(stdin).as_deref(),
            Some("fix the parser bug")
        );
        // Absent / empty / non-JSON → None (the hook still no-ops cleanly).
        assert_eq!(prompt_from_hook_json(r#"{"prompt":"   "}"#), None);
        assert_eq!(prompt_from_hook_json(r#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(prompt_from_hook_json("not json"), None);
        // Cap.
        let long = "x".repeat(MAX_PROMPT_LEN + 50);
        let payload = format!(r#"{{"prompt":"{long}"}}"#);
        assert_eq!(
            prompt_from_hook_json(&payload).unwrap().chars().count(),
            MAX_PROMPT_LEN
        );
    }

    #[test]
    fn payload_carries_prompt_when_present() {
        let payload = build_status_payload("working", None, Some("hello there"), None);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["state"], "working");
        assert_eq!(v["prompt"], "hello there");
    }

    #[test]
    fn stop_payload_carries_last_assistant_message_as_msg() {
        let payload = build_status_payload("idle", None, None, Some("All set — tests pass."));
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["state"], "idle");
        assert_eq!(v["msg"], "All set — tests pass.");
    }

    #[test]
    fn last_assistant_message_reads_the_transcript_tail() {
        use std::io::Write;
        // A JSONL transcript: a user line, an assistant turn with a tool_use
        // (no text), then the final assistant turn with the text reply. We must
        // skip the tool-only turn and return the last TEXT reply, collapsed.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oximux-transcript-test-{}.jsonl", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"working on it"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Edit"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"  Done!\nShipped the   fix.  "}}]}}}}"#
        )
        .unwrap();
        drop(f);

        let stdin = format!(r#"{{"hook_event_name":"Stop","transcript_path":"{}"}}"#, path.display());
        let msg = last_assistant_message_from_hook_json(&stdin);
        let _ = std::fs::remove_file(&path);
        assert_eq!(msg.as_deref(), Some("Done! Shipped the fix."));

        // No transcript_path / bad JSON → None (hook still no-ops cleanly).
        assert_eq!(last_assistant_message_from_hook_json(r#"{"hook_event_name":"Stop"}"#), None);
        assert_eq!(last_assistant_message_from_hook_json("not json"), None);
    }

    #[test]
    fn last_assistant_message_prefers_the_direct_stop_field() {
        // Claude puts the reply directly on the Stop event. We must use it
        // (collapsed/capped) WITHOUT touching the transcript — it is race-free
        // (the JSONL may not be flushed yet) and points at a path here that does
        // not exist, proving the field wins over the file.
        let stdin = r#"{"hook_event_name":"Stop","transcript_path":"/no/such/file.jsonl","last_assistant_message":"  hello   there  "}"#;
        assert_eq!(
            last_assistant_message_from_hook_json(stdin).as_deref(),
            Some("hello there")
        );

        // An empty/blank direct field falls back to the transcript path (here
        // absent → None), never returning a blank string.
        let blank = r#"{"hook_event_name":"Stop","last_assistant_message":"   "}"#;
        assert_eq!(last_assistant_message_from_hook_json(blank), None);
    }
}
