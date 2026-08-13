//! Codex status hooks: the payload readers and the `~/.codex/hooks.json`
//! install.
//!
//! Detecting a Codex agent tells the rail *that* it is there and, from its
//! window title, roughly what it is doing. It cannot tell the rail what the
//! agent SAID — so a Codex row showed `Codex · Idle` where a Claude row showed
//! the actual reply. The difference was never the rail; it was that only
//! Claude had hooks installed.
//!
//! Codex has the same kind of lifecycle hooks, in `$CODEX_HOME/hooks.json`
//! (default `~/.codex`), in the same `{matcher?, hooks:[{type, command}]}`
//! shape Claude uses, delivering the event JSON on **stdin**. So the existing
//! `oximux agent-status` CLI works unchanged as the hook command; only the
//! field names differ, which is what the readers below are for.
//!
//! Two deliberate non-goals:
//!
//! * **`notify` is not used.** Codex's `notify` config is a single program, not
//!   a list, so writing it would silently replace whatever the user already has
//!   there — a real cost, since chaining it is an established convention. It
//!   also only fires at turn end. `hooks.json` is a separate file with a
//!   per-event array, so ours can be merged alongside anything else.
//! * **Hook trust is left to Codex.** Codex holds a trusted-hash ledger and
//!   asks the user to approve a new hook in its own TUI. Forging an entry there
//!   would be both fragile and a consent bypass — the user should be the one to
//!   say yes. Until they do, these hooks simply never fire, and the rail falls
//!   back to process + title exactly as before.

use std::path::Path;

use serde_json::Value;

/// Codex event names carrying the state OxiMux reports. Chosen to mirror
/// [`crate::agent_status_hooks::status_hook_specs`] so a Codex row and a Claude
/// row are driven by the same three transitions.
///
/// `PermissionRequest` is Codex's real approval event (Claude's equivalent is
/// `Notification`), and it needs no payload filter: unlike Claude's, it fires
/// only for an actual approval ask.
pub(crate) fn codex_hook_specs(binary_path: &Path) -> Vec<crate::agent_status_hooks::HookSpec> {
    use crate::agent_status_hooks::HookSpec;
    // Same quoting rule as the Claude spec: an installed bundle path can
    // contain spaces, and an embedded quote must not break out into injection.
    let quoted = binary_path.display().to_string().replace('\'', "'\\''");
    let cmd = |state: &str| format!("'{quoted}' agent-status --state {state} --format codex");
    vec![
        HookSpec {
            event: "UserPromptSubmit",
            matcher: None,
            command: cmd("working"),
        },
        HookSpec {
            event: "PreToolUse",
            matcher: Some("*"),
            command: cmd("working"),
        },
        HookSpec {
            event: "PermissionRequest",
            matcher: None,
            command: cmd("needs_approval"),
        },
        // The one that closes the reported gap: Codex puts the turn's reply on
        // `last_assistant_message` here, the same field Claude's transcript
        // read produces.
        HookSpec {
            event: "Stop",
            matcher: None,
            command: cmd("idle"),
        },
    ]
}

/// The agent's last reply, from a Codex `Stop` payload.
///
/// Codex hands this over directly, where Claude's hook carries only a
/// transcript path that has to be read and folded. Same destination, so the
/// rail cannot tell which agent produced a row.
pub fn codex_last_message(stdin_json: &str) -> Option<String> {
    read_str(&parse(stdin_json)?, &["last_assistant_message"])
}

/// The tool a Codex `PreToolUse`/`PostToolUse` payload is about.
pub fn codex_tool_name(stdin_json: &str) -> Option<String> {
    read_str(&parse(stdin_json)?, &["tool_name", "name"])
}

/// The user's prompt from a Codex payload.
///
/// Several spellings are accepted because this key is the least stable part of
/// the payload across agent CLIs, and a missed prompt costs the row its title
/// while a wrong one is impossible — none of these names carries anything else.
pub fn codex_prompt(stdin_json: &str) -> Option<String> {
    read_str(
        &parse(stdin_json)?,
        &[
            "prompt",
            "user_prompt",
            "userPrompt",
            "initial_prompt",
            "initialPrompt",
            "user_message",
            "message",
        ],
    )
}

fn parse(stdin_json: &str) -> Option<Value> {
    serde_json::from_str(stdin_json).ok()
}

/// First key present as a non-empty string, trimmed.
fn read_str(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| value.get(*k)?.as_str())
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Where Codex reads its hooks from. Honours `CODEX_HOME` so a user who
/// relocated it still gets hooks installed in the home Codex actually reads.
pub(crate) fn hooks_path() -> Option<std::path::PathBuf> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => dirs::home_dir()?.join(".codex"),
    };
    Some(home.join("hooks.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stop_payload_yields_the_agents_reply() {
        let json = r#"{"model":"gpt-5.6-sol","last_assistant_message":"Hi! What would you like to work on?"}"#;
        assert_eq!(
            codex_last_message(json).as_deref(),
            Some("Hi! What would you like to work on?")
        );
    }

    #[test]
    fn a_payload_without_a_reply_yields_nothing() {
        // A turn that produced no assistant text must leave the row's previous
        // reading alone rather than blanking it with an empty string.
        assert_eq!(codex_last_message(r#"{"model":"gpt-5.6-sol"}"#), None);
        assert_eq!(codex_last_message(r#"{"last_assistant_message":""}"#), None);
        assert_eq!(codex_last_message(r#"{"last_assistant_message":"   "}"#), None);
    }

    #[test]
    fn malformed_json_is_not_an_error() {
        // A hook must never fail the agent it is attached to.
        assert_eq!(codex_last_message("not json"), None);
        assert_eq!(codex_tool_name(""), None);
        assert_eq!(codex_prompt("[]"), None);
    }

    #[test]
    fn the_tool_name_is_read_from_either_spelling() {
        assert_eq!(codex_tool_name(r#"{"tool_name":"shell"}"#).as_deref(), Some("shell"));
        assert_eq!(codex_tool_name(r#"{"name":"apply_patch"}"#).as_deref(), Some("apply_patch"));
        // The specific spelling wins over the generic one.
        assert_eq!(
            codex_tool_name(r#"{"name":"generic","tool_name":"shell"}"#).as_deref(),
            Some("shell")
        );
    }

    #[test]
    fn the_prompt_is_read_from_any_accepted_spelling() {
        assert_eq!(codex_prompt(r#"{"prompt":"fix the parser"}"#).as_deref(), Some("fix the parser"));
        assert_eq!(codex_prompt(r#"{"user_message":"hi"}"#).as_deref(), Some("hi"));
        assert_eq!(codex_prompt(r#"{"message":"hi"}"#).as_deref(), Some("hi"));
        // Whitespace-only is absent, not a title.
        assert_eq!(codex_prompt(r#"{"prompt":"  "}"#), None);
    }

    #[test]
    fn every_hook_command_names_the_binary_and_the_codex_format() {
        let specs = codex_hook_specs(Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"));
        assert_eq!(specs.len(), 4);
        for spec in &specs {
            assert!(spec.command.contains("agent-status"));
            assert!(
                spec.command.contains("--format codex"),
                "a Codex hook must select the Codex payload reader, got {:?}",
                spec.command
            );
        }
        // Stop is the one that carries the reply.
        let stop = specs.iter().find(|s| s.event == "Stop").expect("Stop hook");
        assert!(stop.command.contains("--state idle"));
        assert_eq!(stop.matcher, None);
    }

    #[test]
    fn a_path_with_a_quote_cannot_break_out_of_the_command() {
        let specs = codex_hook_specs(Path::new("/Users/O'X/oximux"));
        for spec in &specs {
            assert!(
                spec.command.contains(r"/Users/O'\''X/oximux"),
                "an embedded quote must be escaped, got {:?}",
                spec.command
            );
        }
    }

    #[test]
    fn the_hooks_file_sits_in_the_codex_home() {
        // Guards the default; CODEX_HOME is env-dependent and not asserted here
        // because tests share a process environment.
        let path = hooks_path().expect("a home dir");
        assert!(path.ends_with("hooks.json"));
    }
}
