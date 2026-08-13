//! Global status-hook install/remove in an agent's own hooks file —
//! `~/.claude/settings.json` for Claude, `$CODEX_HOME/hooks.json` for Codex.
//! Both read the same `{hooks:{Event:[{matcher?, hooks:[…]}]}}` shape, so the
//! merge, the managed marker and the atomic write are shared; only the path
//! and the event names differ ([`sync_hooks_file`]).
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

/// What one agent's hooks file tolerates, and how our own entries are found in
/// it again.
///
/// Claude ignores unknown keys, so our entries carry a bookkeeping marker and
/// are found by it — unambiguous, and it never claims a hook the user wrote by
/// hand that happens to call the same CLI. Codex rejects unknown fields, so
/// nothing may be stamped on its entries at all; there they are recognised by
/// their command instead. Writing a marker into a Codex hooks file would risk
/// the file being rejected whole — taking the user's own hooks down with ours.
#[derive(Clone, Copy)]
struct Dialect {
    /// Bookkeeping key stamped on our entries, or `None` where the file
    /// rejects unknown fields.
    marker: Option<&'static str>,
    /// Whether the command entry may carry `"async": true`.
    async_command: bool,
    /// Seconds after which the agent abandons our hook, where the file
    /// supports it. Only meaningful for a hook the agent WAITS on: an async
    /// one cannot hold anything up, so Claude needs none.
    timeout_secs: Option<u64>,
}

const CLAUDE: Dialect = Dialect {
    marker: Some(MANAGED_MARKER),
    async_command: true,
    timeout_secs: None,
};

/// Codex runs its hooks synchronously, so ours sits in front of the user's
/// turn. The timeout bounds the worst case — an unreachable relay, a socket
/// that never answers — to a pause rather than a stall. Generous relative to
/// the real cost (a local socket round-trip) so a loaded machine does not lose
/// a status update to a missed deadline.
const CODEX: Dialect = Dialect {
    marker: None,
    async_command: false,
    timeout_secs: Some(5),
};

fn settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Install (`on = true`) or remove (`on = false`) OxiMux's managed status hooks
/// in `~/.claude/settings.json`. Best-effort: logs and returns on any error.
/// Called at boot and whenever the Status-hooks toggle changes, with the same
/// resolved value that gates the per-spawn `--settings` injection.
pub fn sync_global_status_hooks(on: bool) {
    let path = match settings_path() {
        Some(p) => p,
        None => return,
    };
    sync_hooks_file(on, &path, "Claude", status_hook_specs, CLAUDE);
}

/// The same install for Codex, in `$CODEX_HOME/hooks.json`.
///
/// This is what gives a Codex row the agent's actual reply instead of a bare
/// status verb: the process tree names the agent and the title says roughly
/// what it is doing, but only the agent itself can say what it SAID.
///
/// Codex will not run a newly installed hook until the user approves it in
/// Codex's own trust prompt, so writing the file is a request, not a
/// side-effect — until they say yes, the rail behaves exactly as before.
pub fn sync_global_codex_hooks(on: bool) {
    let path = match crate::codex_status_hooks::hooks_path() {
        Some(p) => p,
        None => return,
    };
    sync_hooks_file(
        on,
        &path,
        "Codex",
        crate::codex_status_hooks::codex_hook_specs,
        CODEX,
    );
}

/// Install (`on`) or remove OxiMux's managed hooks in one agent's hooks file.
/// Best-effort throughout: a hook that cannot be written costs a row its
/// detail, and must never cost the user an error they did not ask for.
fn sync_hooks_file(
    on: bool,
    path: &Path,
    agent: &str,
    specs: fn(&Path) -> Vec<crate::agent_status_hooks::HookSpec>,
    dialect: Dialect,
) {
    let outcome = if on {
        match std::env::current_exe() {
            Ok(exe) => {
                rewrite_settings_at(path, |root| install_managed(root, specs(&exe), dialect))
            }
            Err(err) => {
                tracing::warn!(%err, agent, "global hooks: current_exe failed; not installing");
                return;
            }
        }
    } else {
        rewrite_settings_at(path, |root| remove_managed(root, dialect))
    };
    match outcome {
        Ok(true) => tracing::info!(on, agent, "global status hooks synced"),
        Ok(false) => {} // already in the desired state — no write
        Err(err) => tracing::warn!(%err, on, agent, "global status hooks sync failed"),
    }
}

/// Read the hooks object, apply `mutate` (returns whether it changed
/// anything), and atomically write back only when something changed. Absent or
/// empty file starts from `{}`; a present-but-unparseable file aborts so a user
/// typo is never overwritten.
fn rewrite_settings_at(path: &Path, mutate: impl FnOnce(&mut Value) -> bool) -> io::Result<bool> {
    let shown = path.display();
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => return Err(io::Error::other(format!("{shown} is not a JSON object"))),
            Err(err) => {
                return Err(io::Error::other(format!("{shown} parse failed: {err}")));
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

/// Replace OxiMux's managed entries with `specs`. Returns true when the `hooks`
/// object changed (a re-run with the same binary is a no-op).
///
/// Takes the specs rather than the binary because Claude and Codex read the
/// same `{hooks:{Event:[{matcher?, hooks:[…]}]}}` shape out of different files
/// — only the event names and the command flags differ, so everything here
/// (the managed marker, the merge, the pruning) is shared.
fn install_managed(
    root: &mut Value,
    specs: Vec<crate::agent_status_hooks::HookSpec>,
    dialect: Dialect,
) -> bool {
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
            a.retain(|e| !is_managed(e, dialect));
        }
    }
    // Append the fresh entries.
    for spec in specs {
        let mut entry = serde_json::Map::new();
        if let Some(m) = spec.matcher {
            entry.insert("matcher".into(), json!(m));
        }
        let mut command = serde_json::Map::new();
        command.insert("type".into(), json!("command"));
        command.insert("command".into(), json!(spec.command));
        if dialect.async_command {
            command.insert("async".into(), json!(true));
        }
        if let Some(secs) = dialect.timeout_secs {
            command.insert("timeout".into(), json!(secs));
        }
        entry.insert("hooks".into(), json!([Value::Object(command)]));
        if let Some(marker) = dialect.marker {
            entry.insert(marker.into(), json!(true));
        }
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
fn remove_managed(root: &mut Value, dialect: Dialect) -> bool {
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
            a.retain(|e| !is_managed(e, dialect));
        }
    }
    // Drop event arrays we emptied so we leave no `"Stop": []` litter behind.
    hooks.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    Value::Object(hooks.clone()) != before
}

/// True when `entry` is one OxiMux wrote, by whichever means this file allows:
/// the bookkeeping marker where one can be stamped, otherwise the command it
/// runs. The command test deliberately matches only our own CLI invocation, so
/// a re-install replaces our entries and leaves every other hook in the file
/// exactly where it was.
fn is_managed(entry: &Value, dialect: Dialect) -> bool {
    match dialect.marker {
        Some(marker) => entry.get(marker).and_then(Value::as_bool) == Some(true),
        None => entry
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_our_command)
                })
            }),
    }
}

/// Our own status CLI, however the binary path is spelled. Both fragments are
/// required so a user's unrelated hook that merely mentions one of them is
/// never mistaken for ours and removed.
fn is_our_command(command: &str) -> bool {
    command.contains("agent-status") && command.contains("--state")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude-dialect install, the shape every test below exercises.
    fn install_managed_claude(
        root: &mut Value,
        specs: Vec<crate::agent_status_hooks::HookSpec>,
    ) -> bool {
        install_managed(root, specs, CLAUDE)
    }

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
        assert!(install_managed_claude(&mut root, status_hook_specs(binary())));
        let hooks = &root["hooks"];
        // Unrelated key untouched.
        assert_eq!(root["model"], "opus");
        // User's PreToolUse hook kept, ours appended (marked).
        let pre = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        assert!(is_managed(&pre[1], CLAUDE));
        // All four events present and marked.
        for ev in ["PreToolUse", "UserPromptSubmit", "Notification", "Stop"] {
            let arr = hooks[ev].as_array().unwrap();
            assert!(arr.iter().any(|e| is_managed(e, CLAUDE)), "{ev} has a managed entry");
        }
    }

    #[test]
    fn install_is_idempotent() {
        let mut root = json!({});
        assert!(install_managed_claude(&mut root, status_hook_specs(binary())), "first install changes");
        let after_first = root.clone();
        assert!(
            !install_managed_claude(&mut root, status_hook_specs(binary())),
            "re-install with same binary is a no-op"
        );
        assert_eq!(root, after_first, "no duplicate entries accrue");
    }

    #[test]
    fn install_refreshes_a_stale_binary_path() {
        let mut root = json!({});
        install_managed_claude(&mut root, status_hook_specs(Path::new("/old/path/oximux")));
        let changed = install_managed_claude(&mut root, status_hook_specs(Path::new("/new/path/oximux")));
        assert!(changed, "a different binary path rewrites our entries");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        // Exactly one managed Stop entry, pointing at the new path.
        assert_eq!(stop.iter().filter(|e| is_managed(e, CLAUDE)).count(), 1);
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
        install_managed_claude(&mut root, status_hook_specs(binary()));
        assert!(remove_managed(&mut root, CLAUDE), "remove reports a change");
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "only the user's hook remains");
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        // Event arrays that held only our hooks are pruned entirely.
        assert!(root["hooks"].get("Stop").is_none());
        // Remove again is a no-op.
        assert!(!remove_managed(&mut root, CLAUDE));
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

        let changed = rewrite_settings_at(&path, |root| install_managed(root, status_hook_specs(binary()), CLAUDE)).unwrap();
        assert!(changed);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Unrelated key + the user's Stop hook survive; ours is appended.
        assert_eq!(written["model"], "opus");
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[0]["hooks"][0]["command"], "keep-me");
        assert!(stop.iter().any(|e| is_managed(e, CLAUDE)));
        // A backup of the original was taken.
        assert!(path.with_extension("json.oximux-bak").exists());
        // No temp file left behind.
        assert!(!path.with_extension("json.oximux-tmp").exists());

        // Re-running is a no-op (no spurious rewrite).
        assert!(!rewrite_settings_at(&path, |root| install_managed(root, status_hook_specs(binary()), CLAUDE)).unwrap());
    }

    #[test]
    fn rewrite_creates_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(rewrite_settings_at(&path, |root| install_managed(root, status_hook_specs(binary()), CLAUDE)).unwrap());
        assert!(path.exists());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["hooks"]["UserPromptSubmit"].as_array().unwrap().iter().any(|e| is_managed(e, CLAUDE)));
    }

    #[test]
    fn rewrite_aborts_on_malformed_file_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "{ this is not valid json ";
        std::fs::write(&path, original).unwrap();
        let result = rewrite_settings_at(&path, |root| install_managed(root, status_hook_specs(binary()), CLAUDE));
        assert!(result.is_err(), "a malformed file must abort, not be overwritten");
        // The user's file is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn command_strings_match_the_settings_specs() {
        // The global install MUST use the exact command strings the --settings
        // path emits, or Claude's dedup won't collapse the picker agent's pair.
        let mut root = json!({});
        install_managed_claude(&mut root, status_hook_specs(binary()));
        let specs = status_hook_specs(binary());
        for spec in specs {
            let arr = root["hooks"][spec.event].as_array().unwrap();
            let got = arr
                .iter()
                .filter(|e| is_managed(e, CLAUDE))
                .find_map(|e| e["hooks"][0]["command"].as_str())
                .unwrap();
            assert_eq!(got, spec.command, "{} command matches", spec.event);
        }
    }
}

#[cfg(test)]
mod codex_dialect_tests {
    use super::*;
    use crate::codex_status_hooks::codex_hook_specs;

    fn binary() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    fn install(root: &mut Value) -> bool {
        install_managed(root, codex_hook_specs(binary()), CODEX)
    }

    /// Codex rejects a hooks file carrying fields it does not know, so ours
    /// must be spelled in exactly the vocabulary it defines. A marker or an
    /// `async` flag here would risk the whole file being refused — silencing
    /// the user's own hooks along with ours.
    #[test]
    fn a_codex_entry_carries_no_field_codex_does_not_define() {
        let mut root = json!({});
        assert!(install(&mut root));
        let hooks = root["hooks"].as_object().expect("hooks object");
        for (event, arr) in hooks {
            for entry in arr.as_array().expect("event array") {
                let obj = entry.as_object().expect("entry object");
                for key in obj.keys() {
                    assert!(
                        matches!(key.as_str(), "matcher" | "hooks"),
                        "{event} entry carries unknown key {key:?}"
                    );
                }
                for command in entry["hooks"].as_array().expect("hooks array") {
                    let obj = command.as_object().expect("command object");
                    for key in obj.keys() {
                        assert!(
                            matches!(key.as_str(), "type" | "command" | "timeout"),
                            "{event} command carries unknown key {key:?}"
                        );
                    }
                    assert!(
                        obj.contains_key("timeout"),
                        "{event} runs synchronously and must be bounded"
                    );
                }
            }
        }
    }

    /// With no marker to find them by, our entries are recognised by their
    /// command — so a re-install must still replace rather than accumulate.
    #[test]
    fn reinstalling_replaces_our_entries_instead_of_stacking_them() {
        let mut root = json!({});
        install(&mut root);
        let first = root.clone();
        assert!(
            !install(&mut root),
            "an unchanged re-install must not rewrite the file"
        );
        assert_eq!(root, first);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "exactly one Stop entry, not two");
    }

    #[test]
    fn a_stale_binary_path_is_refreshed_not_duplicated() {
        let mut root = json!({});
        install_managed(&mut root, codex_hook_specs(Path::new("/old/oximux")), CODEX);
        install_managed(&mut root, codex_hook_specs(Path::new("/new/oximux")), CODEX);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("/new/oximux"), "{cmd}");
        assert!(!cmd.contains("/old/oximux"), "{cmd}");
    }

    #[test]
    fn the_users_own_codex_hooks_survive_install_and_remove() {
        let mut root = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "my-own-notifier" }] }
                ]
            }
        });
        install(&mut root);
        assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(remove_managed(&mut root, CODEX));
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "only the user's hook remains");
        assert_eq!(stop[0]["hooks"][0]["command"], "my-own-notifier");
    }

    /// The command test must be specific enough that an unrelated hook merely
    /// mentioning one of its fragments is never adopted and deleted.
    #[test]
    fn a_hook_that_only_resembles_ours_is_left_alone() {
        assert!(is_our_command("'/x/oximux' agent-status --state idle --format codex"));
        assert!(!is_our_command("echo agent-status"));
        assert!(!is_our_command("my-tool --state idle"));
        assert!(!is_our_command("unrelated"));
    }

    /// Claude's file keeps its marker: it tolerates unknown keys, and a marker
    /// never mistakes a hand-written hook calling our CLI for one of ours.
    #[test]
    fn the_claude_dialect_still_marks_its_entries() {
        let mut root = json!({});
        install_managed(&mut root, status_hook_specs(binary()), CLAUDE);
        let stop = &root["hooks"]["Stop"][0];
        assert_eq!(stop[MANAGED_MARKER], json!(true));
        assert_eq!(stop["hooks"][0]["async"], json!(true));
    }
}
