//! Opt-in agent status hooks for Claude Code.
//!
//! Enabled by the **Settings → Agents** "Status hooks" toggle (persisted as
//! `status_hooks_enabled` in `agent_launch.toml`), or by the env var
//! `OXIMUX_STATUS_HOOKS=1` which force-enables regardless (a debug escape
//! hatch). When on, a Claude Code agent is launched with a `--settings` block
//! that wires three hooks to the `oximux agent-status` CLI:
//!
//! - `PreToolUse`        → `--state working`        (`{"state":"working","tool":<name>}`)
//! - `PermissionRequest` → `--state needs_approval` (`{"state":"needs_approval","tool":<name>}`)
//! - `Stop`              → `--state idle`           (`{"state":"idle"}`)
//!
//! The CLI reads the hook event JSON on stdin (for the tool name), reads
//! `OXIMUX_PTY_ID` (injected by the relay at spawn), and asks the relay to
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
//! - **Opt-in.** Off unless the Settings toggle is on (or the env override).
//! - **`Stop` → `idle`, not `done`.** A finished turn is not a dead process;
//!   the terminal `Done` state comes from the PTY exit event, not a hook.

use std::path::Path;

use serde_json::{Value, json};

const ENABLE_ENV: &str = "OXIMUX_STATUS_HOOKS";

/// Cap the reported tool name so a pathological hook payload can't bloat the
/// OSC-9999 packet. The scanner caps again, but trimming at the source is free.
const MAX_TOOL_LEN: usize = 64;

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
    // The command single-quotes the binary path (an installed app bundle path
    // can contain spaces, e.g. "Application Support") then appends the CLI
    // subcommand. Escape any embedded single quote (`'` → `'\''`) so a home dir
    // like `/Users/O'X` can't break out of the quoting into shell injection.
    let quoted = binary_path.display().to_string().replace('\'', "'\\''");
    let cmd = |state: &str| format!("'{quoted}' agent-status --state {state}");
    append_hook(&mut hooks, "PreToolUse", Some("*"), &cmd("working"));
    append_hook(
        &mut hooks,
        "PermissionRequest",
        Some("*"),
        &cmd("needs_approval"),
    );
    append_hook(&mut hooks, "Stop", None, &cmd("idle"));
    json!({ "hooks": hooks }).to_string()
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

/// Build the OSC-9999 inner JSON payload (`{"v":1,"state":..,"tool":..}`) the
/// relay wraps and the scanner decodes. Serialized via `serde_json` so control
/// characters in `tool` are escaped — the relay treats the result as opaque.
pub fn build_status_payload(state: &str, tool: Option<&str>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("v".into(), json!(1));
    obj.insert("state".into(), json!(state));
    if let Some(t) = tool {
        obj.insert("tool".into(), json!(t));
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
    fn settings_json_wires_three_events_to_agent_status_cli() {
        let json = build_settings_json_with(None, binary_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let hooks = &v["hooks"];
        assert!(hooks["PreToolUse"].is_array());
        assert!(hooks["PermissionRequest"].is_array());
        assert!(hooks["Stop"].is_array());
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
        // PermissionRequest reports needs_approval.
        assert!(
            hooks["PermissionRequest"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" agent-status --state needs_approval")
        );
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
        let payload = build_status_payload("working", Some("Bash"));
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

    #[test]
    fn idle_payload_has_no_tool() {
        let payload = build_status_payload("idle", None);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["state"], "idle");
        assert!(v.get("tool").is_none());
    }
}
