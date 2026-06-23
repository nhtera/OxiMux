//! Global Claude hooks install/remove in the user's `~/.claude/settings.json`.
//!
//! The per-spawn `--settings` injection ([`crate::agent_status_hooks`]) only
//! reaches agents OxiMux launches itself. A `claude` the user types BY HAND in a
//! plain terminal pane carries no `--settings`, so it would never report status.
//! To track it too — the way the reference cockpit does — we install the same
//! status hooks into the user's GLOBAL settings file, which every `claude`
//! invocation reads. Every PTY already carries `OXIMUX_PTY_ID` (the relay sets
//! it on spawn), so a hand-typed agent self-attributes to its pane.
//!
//! Safety + correctness:
//! - **Same command strings** as the `--settings` path ([`status_hook_specs`]),
//!   so Claude's command-string hook dedup makes a picker agent (which sees the
//!   file hook AND the `--settings` hook) fire each one exactly once.
//! - **Managed marker.** Each entry we add carries `"_oximux_managed": true`, so
//!   we can find + replace + remove only our own entries and never touch the
//!   user's hooks (or another tool's). Re-installing first drops our prior
//!   entries, which also refreshes a stale binary path after a rebuild.
//! - **Non-destructive merge.** We append to the per-event arrays; the user's
//!   existing hooks are preserved. A missing file starts from `{}`; a malformed
//!   file aborts (we never clobber an unparseable user file).
//! - **Atomic + backed up.** First modification copies the file to
//!   `settings.json.oximux-bak`; writes go through a temp file + rename.
//! - **Best-effort.** Every failure is logged, never propagated — a hand-typed
//!   agent simply won't self-report if the file can't be written.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent_status_hooks::status_hook_specs;

/// Marks an entry as OxiMux-owned so re-install/remove touches only our hooks.
const MANAGED_MARKER: &str = "_oximux_managed";

fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Install (`on = true`) or remove (`on = false`) OxiMux's managed status hooks
/// in `~/.claude/settings.json`. Best-effort: logs and returns on any error.
/// Called at boot and whenever the Status-hooks toggle changes, with the same
/// resolved value that gates the per-spawn `--settings` injection.
pub fn sync_global_status_hooks(on: bool) {
    let outcome = if on {
        match std::env::current_exe() {
            Ok(exe) => rewrite_settings(|root| install_managed(root, &exe)),
            Err(err) => {
                tracing::warn!(%err, "global hooks: current_exe failed; not installing");
                return;
            }
        }
    } else {
        rewrite_settings(remove_managed)
    };
    match outcome {
        Ok(true) => tracing::info!(on, "global Claude status hooks synced"),
        Ok(false) => {} // already in the desired state — no write
        Err(err) => tracing::warn!(%err, on, "global Claude status hooks sync failed"),
    }
}

/// Read the settings object, apply `mutate` (returns whether it changed
/// anything), and atomically write back only when something changed. Absent or
/// empty file starts from `{}`; a present-but-unparseable file aborts so a user
/// typo is never overwritten.
fn rewrite_settings(mutate: impl FnOnce(&mut Value) -> bool) -> io::Result<bool> {
    let path = settings_path().ok_or_else(|| io::Error::other("no home dir for ~/.claude"))?;
    rewrite_settings_at(&path, mutate)
}

/// File-I/O core of [`rewrite_settings`], parameterized on `path` for testing.
fn rewrite_settings_at(path: &Path, mutate: impl FnOnce(&mut Value) -> bool) -> io::Result<bool> {
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => return Err(io::Error::other("~/.claude/settings.json is not a JSON object")),
            Err(err) => {
                return Err(io::Error::other(format!(
                    "~/.claude/settings.json parse failed: {err}"
                )));
            }
        },
        // Empty or absent file: start from an empty object.
        _ => json!({}),
    };

    if !mutate(&mut root) {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // One-time safety copy before our first edit.
    if path.exists() {
        let backup = path.with_extension("json.oximux-bak");
        if !backup.exists() {
            let _ = std::fs::copy(path, &backup);
        }
    }
    let pretty = serde_json::to_string_pretty(&root)?;
    let tmp = path.with_extension("json.oximux-tmp");
    std::fs::write(&tmp, pretty.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
}

/// Replace OxiMux's managed entries with fresh ones for `binary`. Returns true
/// when the `hooks` object changed (a re-run with the same binary is a no-op).
fn install_managed(root: &mut Value, binary: &Path) -> bool {
    let obj = root
        .as_object_mut()
        .expect("rewrite_settings guarantees an object");
    let hooks_val = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks_val.is_object() {
        *hooks_val = json!({});
    }
    let hooks = hooks_val.as_object_mut().expect("coerced to object");
    let before = Value::Object(hooks.clone());

    // Drop our prior entries (refreshes a stale binary path), keep everyone
    // else's.
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_managed(e));
        }
    }
    // Append the fresh, marked entries.
    for spec in status_hook_specs(binary) {
        let mut entry = serde_json::Map::new();
        if let Some(m) = spec.matcher {
            entry.insert("matcher".into(), json!(m));
        }
        entry.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": spec.command, "async": true }]),
        );
        entry.insert(MANAGED_MARKER.into(), json!(true));
        let arr = hooks
            .entry(spec.event.to_string())
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        if let Some(a) = arr.as_array_mut() {
            a.push(Value::Object(entry));
        }
    }
    Value::Object(hooks.clone()) != before
}

/// Remove OxiMux's managed entries and prune any event arrays they emptied.
/// Returns true when the `hooks` object changed.
fn remove_managed(root: &mut Value) -> bool {
    let Some(hooks) = root
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let before = Value::Object(hooks.clone());
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_managed(e));
        }
    }
    // Drop event arrays we emptied so we leave no `"Stop": []` litter behind.
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    Value::Object(hooks.clone()) != before
}

fn is_managed(entry: &Value) -> bool {
    entry.get(MANAGED_MARKER).and_then(Value::as_bool) == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    #[test]
    fn install_adds_four_marked_events_and_preserves_user_hooks() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-thing" }] }
                ]
            }
        });
        assert!(install_managed(&mut root, binary()));
        let hooks = &root["hooks"];
        // Unrelated key untouched.
        assert_eq!(root["model"], "opus");
        // User's PreToolUse hook kept, ours appended (marked).
        let pre = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        assert!(is_managed(&pre[1]));
        // All four events present and marked.
        for ev in ["PreToolUse", "UserPromptSubmit", "Notification", "Stop"] {
            let arr = hooks[ev].as_array().unwrap();
            assert!(arr.iter().any(is_managed), "{ev} has a managed entry");
        }
    }

    #[test]
    fn install_is_idempotent() {
        let mut root = json!({});
        assert!(install_managed(&mut root, binary()), "first install changes");
        let after_first = root.clone();
        assert!(
            !install_managed(&mut root, binary()),
            "re-install with same binary is a no-op"
        );
        assert_eq!(root, after_first, "no duplicate entries accrue");
    }

    #[test]
    fn install_refreshes_a_stale_binary_path() {
        let mut root = json!({});
        install_managed(&mut root, Path::new("/old/path/oximux"));
        let changed = install_managed(&mut root, Path::new("/new/path/oximux"));
        assert!(changed, "a different binary path rewrites our entries");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        // Exactly one managed Stop entry, pointing at the new path.
        assert_eq!(stop.iter().filter(|e| is_managed(e)).count(), 1);
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("/new/path/oximux"), "{cmd}");
        assert!(!cmd.contains("/old/path"), "{cmd}");
    }

    #[test]
    fn remove_strips_only_our_entries() {
        let mut root = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-thing" }] }
                ]
            }
        });
        install_managed(&mut root, binary());
        assert!(remove_managed(&mut root), "remove reports a change");
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "only the user's hook remains");
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        // Event arrays that held only our hooks are pruned entirely.
        assert!(root["hooks"].get("Stop").is_none());
        // Remove again is a no-op.
        assert!(!remove_managed(&mut root));
    }

    #[test]
    fn rewrite_writes_installs_preserves_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}"#,
        )
        .unwrap();

        let changed = rewrite_settings_at(&path, |root| install_managed(root, binary())).unwrap();
        assert!(changed);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Unrelated key + the user's Stop hook survive; ours is appended.
        assert_eq!(written["model"], "opus");
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[0]["hooks"][0]["command"], "keep-me");
        assert!(stop.iter().any(is_managed));
        // A backup of the original was taken.
        assert!(path.with_extension("json.oximux-bak").exists());
        // No temp file left behind.
        assert!(!path.with_extension("json.oximux-tmp").exists());

        // Re-running is a no-op (no spurious rewrite).
        assert!(!rewrite_settings_at(&path, |root| install_managed(root, binary())).unwrap());
    }

    #[test]
    fn rewrite_creates_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(rewrite_settings_at(&path, |root| install_managed(root, binary())).unwrap());
        assert!(path.exists());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().iter().any(is_managed));
    }

    #[test]
    fn rewrite_aborts_on_malformed_file_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "{ this is not valid json ";
        std::fs::write(&path, original).unwrap();
        let result = rewrite_settings_at(&path, |root| install_managed(root, binary()));
        assert!(result.is_err(), "a malformed file must abort, not be overwritten");
        // The user's file is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn command_strings_match_the_settings_specs() {
        // The global install MUST use the exact command strings the --settings
        // path emits, or Claude's dedup won't collapse the picker agent's pair.
        let mut root = json!({});
        install_managed(&mut root, binary());
        let specs = status_hook_specs(binary());
        for spec in specs {
            let arr = root["hooks"][spec.event].as_array().unwrap();
            let got = arr
                .iter()
                .filter(|e| is_managed(e))
                .find_map(|e| e["hooks"][0]["command"].as_str())
                .unwrap();
            assert_eq!(got, spec.command, "{} command matches", spec.event);
        }
    }
}
