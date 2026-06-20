//! Opt-in OSC-9999 status hooks for Claude Code.
//!
//! When `OXIMUX_STATUS_HOOKS=1`, a Claude Code agent is launched with a
//! `--settings` block that wires three hooks to a small shell script which
//! emits an OSC-9999 status packet to the PTY:
//!
//! - `PreToolUse`        → `{"state":"working","tool":<name>}`
//! - `PermissionRequest` → `{"state":"needs_approval","tool":<name>}`
//! - `Stop`              → `{"state":"idle"}`
//!
//! OxiMux's scanner (`oximux-agents` `osc_sideband`) reads those packets from
//! the raw PTY stream and drives structured agent status. This closes the gap
//! where the regex status machine can't see the agent's internal tool steps.
//!
//! Design notes:
//! - **App-owned, non-destructive.** The hook script lives in the app data
//!   dir; the hooks are passed as a `--settings` JSON STRING at spawn, never
//!   written into the user's `~/.claude` config. Because `--settings` replaces
//!   (not deep-merges) the `hooks` key, we read the user's existing global
//!   hooks and merge ours in, so their hooks keep firing.
//! - **Opt-in.** Off unless `OXIMUX_STATUS_HOOKS=1` — the agent's `--settings`
//!   is only touched when the user explicitly turns this on.
//! - **`Stop` → `idle`, not `done`.** A finished turn is not a dead process;
//!   the terminal `Done` state comes from the PTY exit event, not a hook.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The hook script, bundled at compile time and written to disk on first use.
const HOOK_SCRIPT: &str = include_str!("../assets/hooks/oximux-status-emit.sh");
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const SCRIPT_NAME: &str = "oximux-status-emit.sh";
const ENABLE_ENV: &str = "OXIMUX_STATUS_HOOKS";

/// True when the user opted into status hooks (`OXIMUX_STATUS_HOOKS=1`).
pub fn enabled() -> bool {
    std::env::var(ENABLE_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn hooks_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR).join("hooks"))
}

/// Write the bundled hook script to the app data dir (idempotent; rewrites
/// only when the on-disk copy differs) and mark it executable. Returns the
/// absolute path, or `None` when there is no data dir or IO fails.
pub fn install_script() -> Option<PathBuf> {
    let dir = hooks_dir()?;
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%err, "status-hooks: create hooks dir failed");
        return None;
    }
    let path = dir.join(SCRIPT_NAME);
    let needs_write = std::fs::read_to_string(&path)
        .map(|cur| cur != HOOK_SCRIPT)
        .unwrap_or(true);
    if needs_write && let Err(err) = std::fs::write(&path, HOOK_SCRIPT) {
        tracing::warn!(%err, "status-hooks: write script failed");
        return None;
    }
    // Fail closed: a non-executable script makes every hook invocation error,
    // so don't hand back a path we couldn't mark runnable.
    if !set_executable(&path) {
        return None;
    }
    Some(path)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mut perm = meta.permissions();
            if perm.mode() & 0o111 == 0 {
                perm.set_mode(0o755);
                if let Err(err) = std::fs::set_permissions(path, perm) {
                    tracing::warn!(%err, "status-hooks: chmod +x failed");
                    return false;
                }
            }
            true
        }
        Err(err) => {
            tracing::warn!(%err, "status-hooks: stat for chmod failed");
            false
        }
    }
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> bool {
    true
}

/// Build the `--settings` JSON string wiring the three status hooks to
/// `script_path`, merging the user's existing global hooks so the key-replace
/// semantics of `--settings` don't disable them.
pub fn build_settings_json(script_path: &Path) -> String {
    build_settings_json_with(read_user_hooks(), script_path)
}

fn build_settings_json_with(user_hooks: Option<Value>, script_path: &Path) -> String {
    let mut hooks = user_hooks
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    // The command is the single-quoted script path (the app-data path contains
    // a space — "Application Support") followed by the state argument. Escape
    // any embedded single quote (`'` → `'\''`) so a home dir like `/Users/O'X`
    // can't break out of the quoting into shell injection.
    let quoted_path = script_path.display().to_string().replace('\'', "'\\''");
    let cmd = |state: &str| format!("'{quoted_path}' {state}");
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

/// If status hooks are enabled, install the script and prepend
/// `--settings <json>` to `extra_args` for a Claude Code launch. No-op when
/// disabled or the script can't be installed.
pub fn maybe_inject(extra_args: &mut Vec<String>) {
    if !enabled() {
        return;
    }
    let Some(script) = install_script() else {
        tracing::warn!("status-hooks: enabled but script install failed; skipping injection");
        return;
    };
    if extra_args.iter().any(|a| a == "--settings") {
        // The user already configured a --settings flag in agent_launch.toml.
        // claude's behavior with two is undocumented; surface the conflict.
        tracing::warn!(
            "status-hooks: a --settings flag is already configured; a second one may be ignored by claude"
        );
    }
    let json = build_settings_json(&script);
    // Prepend so it precedes any positional prompt build_command appends; order
    // relative to the user's own flags does not matter to `claude`.
    extra_args.insert(0, "--settings".to_string());
    extra_args.insert(1, json);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::AgentOscScanner;
    use oximux_core::AgentSidebandState;

    fn script_path() -> &'static Path {
        Path::new("/tmp/Application Support/oximux/oximux-status-emit.sh")
    }

    #[test]
    fn settings_json_wires_three_events_with_quoted_path() {
        let json = build_settings_json_with(None, script_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let hooks = &v["hooks"];
        // All three events present.
        assert!(hooks["PreToolUse"].is_array());
        assert!(hooks["PermissionRequest"].is_array());
        assert!(hooks["Stop"].is_array());
        // PreToolUse: matcher "*", async true, command single-quotes the path
        // (so the space in "Application Support" survives) and passes "working".
        let pre = &hooks["PreToolUse"][0];
        assert_eq!(pre["matcher"], "*");
        let cmd = pre["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.starts_with('\''), "path must be single-quoted: {cmd}");
        assert!(cmd.ends_with(" working"));
        assert_eq!(pre["hooks"][0]["async"], true);
        // Stop has no matcher field (Stop has no matcher support).
        assert!(hooks["Stop"][0].get("matcher").is_none());
        assert!(
            hooks["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" idle")
        );
    }

    #[test]
    fn user_hooks_are_preserved_not_replaced() {
        // A user PreToolUse hook must survive alongside ours.
        let user = json!({
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-thing" }] }
            ]
        });
        let json = build_settings_json_with(Some(user), script_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let pre = v["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "user hook + ours");
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        assert!(
            pre[1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" working")
        );
    }

    #[test]
    fn non_object_user_hooks_falls_back_to_ours_only() {
        // Malformed `hooks` (an array, not an object) is ignored, not merged.
        let json = build_settings_json_with(Some(json!([1, 2, 3])), script_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v["hooks"]["PreToolUse"].is_array());
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn malformed_event_value_is_coerced_not_dropped() {
        // A valid top-level `hooks` object but a non-array `PreToolUse` (here a
        // JSON object) must not silently swallow our hook.
        let user = json!({ "PreToolUse": { "oops": "not an array" } });
        let json = build_settings_json_with(Some(user), script_path());
        let v: Value = serde_json::from_str(&json).unwrap();
        let pre = v["hooks"]["PreToolUse"]
            .as_array()
            .expect("coerced to array");
        assert_eq!(pre.len(), 1, "our hook survives the malformed entry");
        assert!(
            pre[0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with(" working")
        );
    }

    /// End-to-end format proof (minus live Claude): the bundled hook script's
    /// output is exactly what the Phase-1 scanner decodes. Runs the script in a
    /// temp dir, captures its tty write, and feeds it through `AgentOscScanner`.
    #[test]
    fn hook_output_round_trips_through_scanner() {
        let dir = std::env::temp_dir().join(format!("oximux-hook-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join(SCRIPT_NAME);
        std::fs::write(&script, HOOK_SCRIPT).unwrap();
        set_executable(&script);
        let tty = dir.join("tty.out");

        // Emulate a Claude PreToolUse invocation: JSON on stdin, state arg.
        let mut child = std::process::Command::new("/bin/sh")
            .arg(&script)
            .arg("working")
            .env("OXIMUX_HOOK_TTY", &tty)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn hook");
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/a"}}"#)
            .unwrap();
        assert!(child.wait().unwrap().success());

        let bytes = std::fs::read(&tty).expect("tty output");
        let mut scanner = AgentOscScanner::new();
        let scan = scanner.feed(&bytes);
        let ev = scan.event.expect("scanner decoded a sideband event");
        assert_eq!(ev.state, AgentSidebandState::Working);
        assert_eq!(ev.detail.tool_name.as_deref(), Some("Edit"));
        // The OSC-9999 bytes are fully consumed (stripped) from cleaned output.
        assert!(scan.cleaned.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
