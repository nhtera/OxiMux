//! Global status-hook install/remove in every agent's own configuration.
//!
//! What each agent wants written, and where, is data in
//! [`crate::agent_hook_dialects::DIALECTS`]. This module is the part that is
//! the same whichever agent it is: the merge, the marker, the backup, the
//! atomic write, and the rule that none of it may ever cost the user something
//! they did not ask for ([`sync_dialect`]).
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
//! - **Same command strings** as the `--settings` path, so Claude's
//!   command-string hook dedup makes a picker agent (which sees the file hook
//!   AND the `--settings` hook) fire each one exactly once.
//! - **Only into a home the agent already made.** OxiMux writes into eight
//!   different agents' configuration; creating those directories would leave a
//!   user a dotfile for every agent OxiMux has heard of, most of which they do
//!   not have.
//! - **Ours are found again, one way or another.** Where the file tolerates
//!   unknown keys our entries carry `"_oximux_managed": true`; where it does
//!   not, they are recognised by the command they run. Re-installing drops the
//!   prior ones first, which also refreshes a stale binary path after a
//!   rebuild.
//! - **Non-destructive merge.** We append to the per-event arrays; the user's
//!   existing hooks are preserved. A missing file starts from `{}`; a malformed
//!   file aborts (we never clobber an unparseable user file). A file only
//!   OxiMux writes is the exception — there is nothing to preserve, so it is
//!   written and deleted whole.
//! - **Atomic + backed up.** First modification copies the file to
//!   `settings.json.oximux-bak`; writes go through a temp file + rename.
//! - **Best-effort.** Every failure is logged, never propagated — a hand-typed
//!   agent simply won't self-report if the file can't be written.

use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::agent_hook_dialects::{DIALECTS, EntryShape, HookDialect, Install, hook_specs};

/// Install (`on = true`) or remove (`on = false`) OxiMux's managed status hooks
/// in every agent's hooks file. Best-effort per agent: one that cannot be
/// written is logged and skipped, and the rest still get theirs.
///
/// This is what turns a rail row from a bare status verb into the agent's
/// actual reply: the process tree names the agent and the title says roughly
/// what it is doing, but only the agent itself can say what it SAID.
///
/// Called at boot and whenever the Status-hooks toggle changes, with the same
/// resolved value that gates the per-spawn `--settings` injection.
///
/// Writing a file is a REQUEST, not a side-effect. An agent that holds a
/// trusted-hash ledger will not run a newly installed hook until the user
/// approves it in that agent's own prompt; until they do, the rail behaves
/// exactly as it did before.
pub fn sync_global_status_hooks(on: bool) {
    for dialect in DIALECTS {
        sync_dialect(on, dialect);
    }
}

/// Install (`on`) or remove OxiMux's managed hooks in one agent's hooks file.
/// Best-effort throughout: a hook that cannot be written costs a row its
/// detail, and must never cost the user an error they did not ask for.
fn sync_dialect(on: bool, dialect: &HookDialect) {
    let agent = dialect.agent;
    let Some(path) = dialect.path() else { return };
    // Don't conjure an agent's home. Installing into one that does not exist
    // buys nothing — the agent is not there to read it — and leaves the user a
    // dotfile for a tool they never installed, once per agent OxiMux knows of.
    // Uninstall still runs, so a file left by an agent since removed is
    // cleaned up rather than stranded.
    if on && !dialect.agent_is_installed() {
        return;
    }
    // A file nobody but OxiMux writes is removed rather than pruned: there can
    // be no user content in it to preserve, and leaving an empty husk behind in
    // a directory the agent scans is litter it still has to parse.
    let ours_alone = match &dialect.install {
        Install::HooksFile { owns_file, .. } => *owns_file,
        Install::Extension { .. } => true,
    };
    if !on && ours_alone {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!(on, agent, "global status hooks removed"),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(%err, agent, "global status hooks removal failed"),
        }
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            tracing::warn!(%err, agent, "global hooks: current_exe failed; not installing");
            return;
        }
    };
    let outcome = match &dialect.install {
        Install::Extension { source } => write_if_changed(&path, &source(&exe)),
        Install::HooksFile { .. } if on => {
            rewrite_settings_at(&path, |root| install_managed(root, hook_specs(dialect, &exe), dialect))
        }
        Install::HooksFile { .. } => rewrite_settings_at(&path, |root| remove_managed(root, dialect)),
    };
    match outcome {
        Ok(true) => tracing::info!(on, agent, "global status hooks synced"),
        Ok(false) => {} // already in the desired state — no write
        Err(err) => tracing::warn!(%err, on, agent, "global status hooks sync failed"),
    }
}

/// Write `contents` to `path`, reporting whether anything changed.
///
/// The read-first check is what keeps an extension from being rewritten on
/// every boot: the agent that loads it watches its directory, and a file whose
/// mtime moves for no reason is a reload for no reason.
fn write_if_changed(path: &Path, contents: &str) -> io::Result<bool> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Same temp-then-rename as the hooks files: the agent may be scanning this
    // directory while we write, and must never load half a file.
    let tmp = path.with_extension("oximux-tmp");
    std::fs::write(&tmp, contents.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(true)
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

/// Replace OxiMux's managed entries with `specs`. Returns true when the file
/// changed (a re-run with the same binary is a no-op).
///
/// Takes the specs rather than the binary because every agent's file is the
/// same `{hooks:{Event:[…]}}` object — only the entry spelling and the event
/// names differ, so everything here (the managed marker, the merge, the
/// pruning) is shared.
fn install_managed(
    root: &mut Value,
    specs: Vec<crate::agent_status_hooks::HookSpec>,
    dialect: &HookDialect,
) -> bool {
    let Install::HooksFile {
        marker,
        root_version,
        entry: shape,
        ..
    } = &dialect.install
    else {
        // An extension dispatches its own events; there are no entries to merge.
        return false;
    };
    let obj = root
        .as_object_mut()
        .expect("rewrite_settings guarantees an object");
    let before = Value::Object(obj.clone());
    // A file that declares its schema version must keep declaring it, or the
    // agent rejects the file we just wrote.
    if let Some(version) = root_version {
        obj.insert("version".into(), json!(version));
    }
    let hooks_val = obj.entry("hooks").or_insert_with(|| json!({}));
    if !hooks_val.is_object() {
        *hooks_val = json!({});
    }
    let hooks = hooks_val.as_object_mut().expect("coerced to object");

    // Drop our prior entries (refreshes a stale binary path), keep everyone
    // else's.
    for arr in hooks.values_mut() {
        if let Some(a) = arr.as_array_mut() {
            a.retain(|e| !is_managed(e, *marker));
        }
    }
    // Append the fresh entries.
    for spec in specs {
        let entry = build_entry(&spec, shape, *marker);
        let arr = hooks
            .entry(spec.event.to_string())
            .or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        if let Some(a) = arr.as_array_mut() {
            a.push(entry);
        }
    }
    Value::Object(obj.clone()) != before
}

/// One hook entry, spelled the way this agent's file spells one.
///
/// The two shapes are not variations on a theme: in one an entry is a *group*
/// carrying an array of commands, in the other the entry IS the command. A
/// group written into a flat file parses as an entry with no command at all —
/// installed, silent, and indistinguishable from a hook that simply never
/// fires.
fn build_entry(
    spec: &crate::agent_status_hooks::HookSpec,
    shape: &EntryShape,
    marker: Option<&'static str>,
) -> Value {
    let mut entry = serde_json::Map::new();
    match shape {
        EntryShape::Nested {
            async_command,
            timeout,
        } => {
            if let Some(m) = spec.matcher {
                entry.insert("matcher".into(), json!(m));
            }
            let mut command = serde_json::Map::new();
            command.insert("type".into(), json!("command"));
            command.insert("command".into(), json!(spec.command));
            if *async_command {
                command.insert("async".into(), json!(true));
            }
            if let Some(t) = timeout {
                command.insert(t.key.into(), json!(t.value));
            }
            entry.insert("hooks".into(), json!([Value::Object(command)]));
        }
        EntryShape::Flat {
            command_key,
            typed,
            timeout,
        } => {
            if *typed {
                entry.insert("type".into(), json!("command"));
            }
            entry.insert((*command_key).into(), json!(spec.command));
            if let Some(t) = timeout {
                entry.insert(t.key.into(), json!(t.value));
            }
        }
    }
    if let Some(marker) = marker {
        entry.insert(marker.into(), json!(true));
    }
    Value::Object(entry)
}

/// Remove OxiMux's managed entries and prune any event arrays they emptied.
/// Returns true when the `hooks` object changed.
fn remove_managed(root: &mut Value, dialect: &HookDialect) -> bool {
    let Install::HooksFile { marker, .. } = &dialect.install else {
        return false;
    };
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
            a.retain(|e| !is_managed(e, *marker));
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
fn is_managed(entry: &Value, marker: Option<&str>) -> bool {
    if marker.is_some_and(|m| entry.get(m).and_then(Value::as_bool) == Some(true)) {
        return true;
    }
    // The command test runs even where a marker is defined, because an entry
    // OxiMux wrote is still OxiMux's whether or not that version stamped one.
    // Measured: a real `~/.claude/settings.json` held sixteen unmarked entries
    // of ours pointing at four superseded binary paths, invisible to a
    // marker-only test and so never pruned — Claude was running five copies of
    // our hook on every event, and one more would arrive with each new path.
    //
    // Both entry shapes are checked regardless of the dialect's own, so an
    // entry left behind by a version that wrote the other shape is recognised
    // too rather than accumulating beside the new one.
    if entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(is_our_entry))
    {
        return true;
    }
    is_our_entry(entry)
}

/// True when this object's command — under any of the keys an agent file uses
/// for one — is our status CLI.
fn is_our_entry(entry: &Value) -> bool {
    ["command", "bash"].iter().any(|key| {
        entry
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(is_our_command)
    })
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
    use crate::agent_hook_dialects::dialect_for_slug;

    fn binary() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    fn dialect(slug: &str) -> &'static HookDialect {
        dialect_for_slug(slug).expect("a known slug")
    }

    /// The key this dialect stamps on its own entries, if any.
    fn marker_of(dialect: &HookDialect) -> Option<&'static str> {
        match &dialect.install {
            Install::HooksFile { marker, .. } => *marker,
            Install::Extension { .. } => None,
        }
    }

    /// Only the dialects with entries to merge; an extension has none.
    fn hooks_file_dialects() -> impl Iterator<Item = &'static HookDialect> {
        DIALECTS
            .iter()
            .filter(|d| matches!(d.install, Install::HooksFile { .. }))
    }

    fn install(root: &mut Value, slug: &str) -> bool {
        let d = dialect(slug);
        install_managed(root, hook_specs(d, binary()), d)
    }

    #[test]
    fn install_marks_every_event_and_preserves_user_hooks() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "user-thing" }] }
                ]
            }
        });
        assert!(install(&mut root, "claude"));
        let hooks = &root["hooks"];
        // Unrelated key untouched.
        assert_eq!(root["model"], "opus");
        // User's PreToolUse hook kept, ours appended (marked).
        let pre = hooks["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0]["hooks"][0]["command"], "user-thing");
        assert!(is_managed(&pre[1], marker_of(dialect("claude"))));
        for ev in ["PreToolUse", "UserPromptSubmit", "Notification", "Stop"] {
            let arr = hooks[ev].as_array().unwrap();
            assert!(
                arr.iter().any(|e| is_managed(e, marker_of(dialect("claude")))),
                "{ev} has a managed entry"
            );
        }
    }

    #[test]
    fn install_is_idempotent_for_every_dialect() {
        // Re-running the sync must not rewrite the file, or every boot would
        // churn the user's config and stack a second copy of our hooks.
        for d in hooks_file_dialects() {
            let mut root = json!({});
            assert!(install(&mut root, d.slug), "{} first install changes", d.slug);
            let after_first = root.clone();
            assert!(
                !install(&mut root, d.slug),
                "{} re-install with the same binary must be a no-op",
                d.slug
            );
            assert_eq!(root, after_first, "{} accrued a duplicate entry", d.slug);
        }
    }

    #[test]
    fn install_refreshes_a_stale_binary_path_for_every_dialect() {
        // The app moves (a rebuild, an update, a drag to /Applications) and the
        // old path stops existing. Ours must be rewritten, not joined.
        for d in hooks_file_dialects() {
            let mut root = json!({});
            install_managed(&mut root, hook_specs(d, Path::new("/old/path/oximux")), d);
            assert!(
                install_managed(&mut root, hook_specs(d, Path::new("/new/path/oximux")), d),
                "{} did not notice a different binary path",
                d.slug
            );
            let idle = d
                .events()
                .iter()
                .find(|e| e.state == "idle")
                .expect("a turn-end event");
            let arr = root["hooks"][idle.event].as_array().unwrap();
            let ours: Vec<_> = arr.iter().filter(|e| is_managed(e, marker_of(d))).collect();
            assert_eq!(ours.len(), 1, "{} kept {} entries, want 1", d.slug, ours.len());
            let rendered = serde_json::to_string(&ours[0]).unwrap();
            assert!(rendered.contains("/new/path/oximux"), "{}: {rendered}", d.slug);
            assert!(!rendered.contains("/old/path"), "{}: {rendered}", d.slug);
        }
    }

    #[test]
    fn install_then_remove_restores_the_users_file_exactly() {
        // The Settings toggle is two-way. Turning it off must leave the file
        // indistinguishable from one OxiMux never touched — including for the
        // dialects with no marker, which find their entries by command.
        for d in hooks_file_dialects() {
            let mut root = json!({
                "hooks": {
                    "Stop": [
                        { "hooks": [{ "type": "command", "command": "my-own-notifier" }] }
                    ],
                    "stop": [ { "command": "my-own-flat-notifier" } ]
                }
            });
            let original = root.clone();
            install(&mut root, d.slug);
            assert_ne!(root, original, "{} installed nothing", d.slug);
            assert!(remove_managed(&mut root, d), "{} remove reported no change", d.slug);
            // A dialect that declares a schema version leaves it behind; that
            // is the file's own key, not one of our entries.
            if matches!(d.install, Install::HooksFile { root_version: Some(_), .. }) {
                root.as_object_mut().unwrap().remove("version");
            }
            assert_eq!(root, original, "{} did not restore the file", d.slug);
            assert!(!remove_managed(&mut root, d), "{} remove is not idempotent", d.slug);
        }
    }

    #[test]
    fn a_codex_entry_carries_no_field_codex_does_not_define() {
        // Codex rejects a hooks file carrying fields it does not know, so ours
        // must be spelled in exactly the vocabulary it defines. A marker or an
        // `async` flag here would risk the whole file being refused — silencing
        // the user's own hooks along with ours.
        let mut root = json!({});
        assert!(install(&mut root, "codex"));
        for (event, arr) in root["hooks"].as_object().expect("hooks object") {
            for entry in arr.as_array().expect("event array") {
                for key in entry.as_object().expect("entry object").keys() {
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

    #[test]
    fn a_flat_dialect_writes_the_command_on_the_entry_itself() {
        // The failure this guards is silent: a nested group written into a flat
        // file parses as an entry with no command, so the hooks install
        // cleanly, the file stays valid, and nothing ever fires.
        let mut root = json!({});
        install(&mut root, "copilot");
        let stop = &root["hooks"]["Stop"][0];
        assert!(stop.get("hooks").is_none(), "copilot entry must not nest a group");
        assert!(
            stop["bash"].as_str().is_some_and(|c| c.contains("agent-status")),
            "copilot spells its command under `bash`, got {stop:?}"
        );
        assert_eq!(stop["type"], "command");
        assert_eq!(stop["timeoutSec"], 5);
        // Copilot's file declares its schema version, and must keep declaring it.
        assert_eq!(root["version"], 1);

        let mut root = json!({});
        install(&mut root, "cursor");
        let stop = &root["hooks"]["stop"][0];
        assert!(stop.get("hooks").is_none(), "cursor entry must not nest a group");
        assert!(stop.get("type").is_none(), "cursor entries carry no type");
        assert!(stop["command"].as_str().is_some_and(|c| c.contains("agent-status")));
        assert_eq!(stop["timeout"], 10);
    }

    #[test]
    fn an_unmarked_entry_of_ours_is_still_pruned() {
        // The measured leak: entries OxiMux wrote before it stamped a marker
        // stayed invisible to a marker-only test, so every new binary path
        // added four more and Claude ran all of them. One real settings.json
        // had sixteen, across four superseded paths.
        let mut root = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command",
                        "command": "'/old/dist/OxiMux.app/Contents/MacOS/oximux' agent-status --state idle" }] },
                    { "hooks": [{ "type": "command",
                        "command": "'/older/oximux' agent-status --state idle" }] },
                    { "hooks": [{ "type": "command", "command": "the-users-own-notifier" }] }
                ]
            }
        });
        install(&mut root, "claude");
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        let ours: Vec<_> = stop.iter().filter(|e| is_managed(e, marker_of(dialect("claude")))).collect();
        assert_eq!(ours.len(), 1, "both unmarked entries of ours must be replaced, not joined");
        assert!(
            serde_json::to_string(&ours[0]).unwrap().contains("/Applications/OxiMux.app"),
            "the survivor must be the freshly written one"
        );
        // The user's own hook is untouched.
        assert!(
            stop.iter().any(|e| e["hooks"][0]["command"] == "the-users-own-notifier"),
            "an unrelated hook must survive the prune"
        );
    }

    #[test]
    fn a_hook_that_only_resembles_ours_is_left_alone() {
        // The command test must be specific enough that an unrelated hook
        // merely mentioning one of its fragments is never adopted and deleted.
        assert!(is_our_command("'/x/oximux' agent-status --state idle --format codex"));
        assert!(!is_our_command("echo agent-status"));
        assert!(!is_our_command("my-tool --state idle"));
        assert!(!is_our_command("unrelated"));
    }

    #[test]
    fn an_entry_written_in_the_other_shape_is_still_recognised_as_ours() {
        // An OxiMux that wrote the nested shape into a file we now write flat
        // (or the reverse) must have its entry REPLACED on the next sync. Not
        // recognising it would leave both firing, and the row would report the
        // same turn twice.
        let nested = json!({ "hooks": [{ "type": "command", "command": "'/x/oximux' agent-status --state idle" }] });
        let flat = json!({ "command": "'/x/oximux' agent-status --state idle" });
        let bash = json!({ "type": "command", "bash": "'/x/oximux' agent-status --state idle" });
        for d in hooks_file_dialects().filter(|d| marker_of(d).is_none()) {
            assert!(is_managed(&nested, marker_of(d)), "{} missed a nested entry", d.slug);
            assert!(is_managed(&flat, marker_of(d)), "{} missed a flat entry", d.slug);
            assert!(is_managed(&bash, marker_of(d)), "{} missed a bash entry", d.slug);
        }
    }

    #[test]
    fn the_claude_dialect_still_marks_its_entries() {
        // Claude tolerates unknown keys, and a marker never mistakes a
        // hand-written hook calling our CLI for one of ours.
        let mut root = json!({});
        install(&mut root, "claude");
        let stop = &root["hooks"]["Stop"][0];
        assert_eq!(stop[crate::agent_hook_dialects::MANAGED_MARKER], json!(true));
        assert_eq!(stop["hooks"][0]["async"], json!(true));
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
        let d = dialect("claude");

        let changed =
            rewrite_settings_at(&path, |root| install_managed(root, hook_specs(d, binary()), d))
                .unwrap();
        assert!(changed);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Unrelated key + the user's Stop hook survive; ours is appended.
        assert_eq!(written["model"], "opus");
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[0]["hooks"][0]["command"], "keep-me");
        assert!(stop.iter().any(|e| is_managed(e, marker_of(d))));
        // A backup of the original was taken.
        assert!(path.with_extension("json.oximux-bak").exists());
        // No temp file left behind.
        assert!(!path.with_extension("json.oximux-tmp").exists());

        // Re-running is a no-op (no spurious rewrite).
        assert!(
            !rewrite_settings_at(&path, |root| install_managed(root, hook_specs(d, binary()), d))
                .unwrap()
        );
    }

    #[test]
    fn rewrite_creates_an_absent_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let d = dialect("claude");
        assert!(
            rewrite_settings_at(&path, |root| install_managed(root, hook_specs(d, binary()), d))
                .unwrap()
        );
        assert!(path.exists());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            v["hooks"]["UserPromptSubmit"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| is_managed(e, marker_of(d)))
        );
    }

    #[test]
    fn rewrite_aborts_on_malformed_file_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "{ this is not valid json ";
        std::fs::write(&path, original).unwrap();
        let d = dialect("claude");
        let result =
            rewrite_settings_at(&path, |root| install_managed(root, hook_specs(d, binary()), d));
        assert!(result.is_err(), "a malformed file must abort, not be overwritten");
        // The user's file is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn command_strings_match_the_settings_specs() {
        // The global install MUST use the exact command strings the --settings
        // path emits, or Claude's dedup won't collapse the picker agent's pair.
        let mut root = json!({});
        install(&mut root, "claude");
        for spec in crate::agent_status_hooks::status_hook_specs(binary()) {
            let arr = root["hooks"][spec.event].as_array().unwrap();
            let got = arr
                .iter()
                .filter(|e| is_managed(e, marker_of(dialect("claude"))))
                .find_map(|e| e["hooks"][0]["command"].as_str())
                .unwrap();
            assert_eq!(got, spec.command, "{} command matches", spec.event);
        }
    }
}
