//! The MCP server OxiMux declares for an agent that may drive the screen.
//!
//! This is where the injection seam built previously meets the driver: the
//! returned [`McpServerSpec`] is what gets handed to a spawned agent, so
//! `claude` launches `cua-driver mcp` itself and talks to it directly.
//!
//! That out-of-process hop is why OxiMux's Rust is not in the tool-dispatch
//! path, and why the enforcement point is the permission round-trip rather
//! than anything in this crate.

use std::path::Path;

use oximux_agent_core::thread::McpServerSpec;

/// Name the server is declared under. It is not cosmetic: agents namespace MCP
/// tools as `mcp__<server>__<tool>`, so this string is the prefix every later
/// policy check matches on. Changing it silently unhooks those checks.
///
/// Prefixed with the app name rather than the bare `computer-use`, which a live
/// run showed is **silently dropped** — declared under that name the server
/// never appears in the session's server list at all, with no error, and the
/// agent simply reports the tools missing. A namespaced name also avoids
/// colliding with a server the user has configured themselves.
pub const SERVER_NAME: &str = "oximux-computer-use";

use crate::HOST_BUNDLE_ID;

/// Build the server declaration for a verified driver.
///
/// Two flags are deliberately *not* passed:
///
/// - `--socket`: the daemon is a machine-wide singleton on a fixed path shared
///   with every other MCP client. Overriding it would fragment that into a
///   second daemon needing its own TCC grants.
/// - `--claude-code-computer-use-compat`: that swaps the standard tool surface
///   for a screenshot-shaped one matching an agent's *native* computer-use
///   tools. Agent Chat cannot use those anyway, and the standard surface is the
///   one with the accessibility-tree paths that work on background windows.
pub fn server_spec(driver: &Path) -> McpServerSpec {
    McpServerSpec::new(SERVER_NAME, driver.display().to_string()).args(vec![
        "mcp".to_string(),
        "--host-bundle-id".to_string(),
        HOST_BUNDLE_ID.to_string(),
    ])
}

/// Everything a chat must be given in order to have screen control.
///
/// The parts travel together deliberately, and in one direction: handing an
/// agent the server without the hook is the failure this type exists to
/// prevent, because the runtime policy would then run only in the permission
/// modes that actually prompt, and measurement showed two ordinary situations
/// where none does.
///
/// The reverse is not a failure but the ordinary case — see [`server`].
///
/// [`server`]: Declaration::server
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The driver, when this chat is allowed to reach it.
    ///
    /// `None` for every chat the user has not opted in, which is nearly all of
    /// them — and those chats are still given the hook. The reason is that the
    /// thing being gated is not really the tools: the macOS Accessibility grant
    /// behind them belongs to the OxiMux *process*, every child inherits it, and
    /// an agent's shell is a child. So a chat with no screen-control tools can
    /// still drive the screen through `osascript`, and the gate is what refuses
    /// that. Registering it only where the tools are declared would put the
    /// check exactly where it is least needed.
    pub server: Option<McpServerSpec>,
    /// Namespaced tool names for `--disallowedTools`. The CLI removes these
    /// from the agent's surface entirely, in every permission mode, and a deny
    /// outranks a user's own `permissions.allow` rule.
    ///
    /// Empty when there is no server: these names would then match nothing, and
    /// a deny list full of tools that do not exist reads as protection it is
    /// not providing.
    pub disallowed_tools: Vec<String>,
    /// Inline JSON for `--settings`, registering the `PreToolUse` hook that
    /// decides everything the deny list cannot express — anything depending on
    /// a call's *arguments*, or on which chat holds which target.
    pub hook_settings: String,
}

/// Where the enforcing hook lives and what it needs told.
pub struct HookSpec<'a> {
    /// The gate binary, shipped alongside the app.
    pub command: &'a Path,
    /// This chat's screen-control session id.
    pub chat: &'a str,
    /// The shared grant store. Passed explicitly so the app and the hook cannot
    /// resolve different files.
    pub grants: &'a Path,
    /// OxiMux's own executable — normally `std::env::current_exe()`.
    ///
    /// Passed for the same reason as `grants`, and it matters more than it
    /// looks: the gate is a *separate binary*, so it cannot ask what process it
    /// is and get a useful answer. Without this, "an agent may never drive
    /// OxiMux" holds only for a shipped build, which is identifiable by bundle
    /// id — a development build is ad-hoc signed with none, and that is the
    /// build this feature is written in.
    pub host: &'a Path,
    /// The chat's worktree and start time, for build provenance. `None` leaves
    /// the hook asking about every target rather than trusting its own builds.
    pub worktree: Option<&'a Path>,
    pub started_at: Option<std::time::SystemTime>,
}

/// Build the full declaration for a chat.
///
/// The single way to obtain the server spec, so no call site can take one part
/// and leave the rest. `driver` is `None` for a chat that gets no
/// screen-control tools — it still gets the hook, for the reason on
/// [`Declaration::server`].
pub fn declaration(driver: Option<&Path>, hook: &HookSpec<'_>) -> Declaration {
    let mut command = format!(
        "{} --chat {} --grants {} --host-exe {}",
        shell_quote(&hook.command.display().to_string()),
        shell_quote(hook.chat),
        shell_quote(&hook.grants.display().to_string()),
        shell_quote(&hook.host.display().to_string()),
    );
    if let Some(worktree) = hook.worktree {
        command.push_str(&format!(" --worktree {}", shell_quote(&worktree.display().to_string())));
    }
    if let Some(started) = hook.started_at {
        let secs = started
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        command.push_str(&format!(" --since {secs}"));
    }

    let hook_settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": hook_matcher(),
                "hooks": [{ "type": "command", "command": command }],
            }]
        }
    })
    .to_string();

    Declaration {
        server: driver.map(server_spec),
        disallowed_tools: driver
            .map(|_| {
                crate::tools::forbidden_names()
                    .map(|tool| format!("{}{tool}", tool_prefix()))
                    .collect()
            })
            .unwrap_or_default(),
        hook_settings,
    }
}

/// Which tool names the gate is consulted about.
///
/// Two families, and the second is the one an earlier draft missed: this
/// server's own tools, and the **shell**. A command that drives the GUI is a
/// screen-control call by another road — it reaches the same APIs with the same
/// inherited grant — and [`crate::policy`] already refuses those. Scoped to the
/// MCP prefix, that refusal would never have run, because the gate would never
/// have been asked about a `Bash` call.
///
/// Built from [`crate::policy::SHELL_TOOLS`] so the two cannot drift: a shell
/// tool added there is matched here without anyone remembering to.
///
/// Anchored, and the trailing `.*` is deliberate rather than redundant. The
/// matcher is a regex whose anchoring the CLI does not document, and this shape
/// is correct under either reading — a bare `^…prefix` would match nothing at
/// all if the CLI wraps the pattern in a full-string match.
pub fn hook_matcher() -> String {
    let mut alternatives: Vec<String> = crate::policy::SHELL_TOOLS
        .iter()
        .map(|tool| (*tool).to_string())
        .collect();
    alternatives.push(format!("{}.*", tool_prefix()));
    format!("^({})$", alternatives.join("|"))
}

/// Single-quote a value for the shell the CLI runs hook commands through.
///
/// Paths here come from the user's filesystem — a worktree with a space or an
/// apostrophe would otherwise split into extra arguments and the gate would
/// reject its own command line, silently leaving the chat unenforced.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The namespaced prefix agents give this server's tools.
pub fn tool_prefix() -> String {
    format!("mcp__{SERVER_NAME}__")
}

/// Is `tool_name` one of this server's tools?
///
/// Lives next to [`SERVER_NAME`] so the prefix format is derived in exactly one
/// place — a policy check that hardcodes its own copy is a check that silently
/// stops matching when the name changes.
pub fn is_computer_use_tool(tool_name: &str) -> bool {
    tool_name.starts_with(&tool_prefix())
}

/// The bare driver tool behind a namespaced name, e.g. `click`.
pub fn bare_tool_name(tool_name: &str) -> Option<&str> {
    tool_name.strip_prefix("mcp__").and_then(|rest| {
        rest.strip_prefix(SERVER_NAME)
            .and_then(|rest| rest.strip_prefix("__"))
            .filter(|bare| !bare.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agent_core::thread::to_claude_mcp_config;

    #[test]
    fn spec_invokes_the_driver_in_mcp_mode() {
        let spec = server_spec(Path::new("/Applications/CuaDriver.app/Contents/MacOS/cua-driver"));
        assert_eq!(spec.name, SERVER_NAME);
        assert_eq!(
            spec.command,
            "/Applications/CuaDriver.app/Contents/MacOS/cua-driver"
        );
        assert_eq!(spec.args[0], "mcp");
    }

    #[test]
    fn spec_omits_socket_and_compat_flags() {
        // Both would change which daemon or which tool surface the agent gets.
        let spec = server_spec(Path::new("/bin/cua-driver"));
        assert!(!spec.args.iter().any(|a| a == "--socket"));
        assert!(
            !spec
                .args
                .iter()
                .any(|a| a == "--claude-code-computer-use-compat")
        );
    }

    #[test]
    fn spec_renders_into_the_agent_config_payload() {
        // End-to-end through the injection seam: what an agent is handed.
        let spec = server_spec(Path::new("/bin/cua-driver"));
        let raw = to_claude_mcp_config(std::slice::from_ref(&spec)).expect("some config");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let entry = &v["mcpServers"][SERVER_NAME];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "/bin/cua-driver");
        assert_eq!(entry["args"][0], "mcp");
    }

    fn declared() -> Declaration {
        declaration(
            Some(Path::new("/bin/cua-driver")),
            &HookSpec {
                command: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate"),
                chat: "chat-7",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"),
                worktree: Some(Path::new("/repo")),
                started_at: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
            },
        )
    }

    #[test]
    fn the_declaration_registers_the_enforcing_hook() {
        // The part the deny list cannot do: argument-level and per-chat
        // decisions, in every permission mode.
        let v: serde_json::Value =
            serde_json::from_str(&declared().hook_settings).expect("valid json");
        let entry = &v["hooks"]["PreToolUse"][0];
        assert_eq!(entry["matcher"], hook_matcher());
        let command = entry["hooks"][0]["command"].as_str().expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(command.contains("--chat 'chat-7'"), "{command}");
        assert!(command.contains("--grants '/data/grants.json'"), "{command}");
        assert!(command.contains("--since 1700000000"), "{command}");
        // Without this the gate cannot tell that a call is aimed at OxiMux,
        // because it is a different binary and `current_exe()` names itself.
        assert!(
            command.contains("--host-exe '/Applications/OxiMux.app/Contents/MacOS/oximux'"),
            "{command}"
        );
    }

    #[test]
    fn hook_paths_survive_a_space_or_an_apostrophe() {
        // A worktree under "~/My Projects" would otherwise split into extra
        // arguments, the gate would reject its own command line, and the chat
        // would run unenforced with nothing to show for it.
        let declared = declaration(
            Some(Path::new("/bin/cua-driver")),
            &HookSpec {
                command: Path::new("/Apps/Oxi Mux.app/gate"),
                chat: "chat-1",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Apps/Oxi Mux.app/oximux"),
                worktree: Some(Path::new("/Users/x/it's mine")),
                started_at: None,
            },
        );
        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains(r"'/Apps/Oxi Mux.app/gate'"), "{command}");
        // The POSIX way to get a literal quote inside single quotes: close,
        // emit an escaped quote, reopen.
        assert!(command.contains(r"'/Users/x/it'\''s mine'"), "{command}");
    }

    #[test]
    fn the_declaration_denies_the_tools_the_policy_forbids() {
        let declared = declared();
        // Namespaced, because that is the form the CLI matches on — a bare
        // `replay_trajectory` would silently match nothing.
        assert!(
            declared
                .disallowed_tools
                .contains(&"mcp__oximux-computer-use__replay_trajectory".to_string()),
            "{:?}",
            declared.disallowed_tools
        );
        assert!(declared.disallowed_tools.iter().all(|t| t.starts_with("mcp__oximux-computer-use__")));
        // And every denied name must round-trip back to a forbidden class.
        for name in &declared.disallowed_tools {
            let bare = bare_tool_name(name).expect("namespaced");
            assert!(matches!(
                crate::tools::classify(bare),
                crate::tools::ToolClass::Forbidden(_)
            ));
        }
    }

    #[test]
    fn the_declaration_leaves_the_working_tools_alone() {
        // Denying these would break the feature rather than protect it.
        let declared = declared();
        for tool in ["click", "type_text", "get_window_state", "move_cursor"] {
            assert!(
                !declared
                    .disallowed_tools
                    .contains(&format!("mcp__oximux-computer-use__{tool}")),
                "{tool} must stay available"
            );
        }
    }

    /// The case nearly every chat is in: no screen-control tools, and the gate
    /// registered anyway.
    ///
    /// Not a degenerate configuration — it is what protects the road around the
    /// tools. OxiMux holds Accessibility process-wide so an agent's shell
    /// inherits it, which means a chat with no server can still drive the screen
    /// and the gate is the only thing that says no.
    #[test]
    fn a_chat_with_no_driver_still_gets_the_gate() {
        let declared = declaration(
            None,
            &HookSpec {
                command: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate"),
                chat: "chat-7",
                grants: Path::new("/data/grants.json"),
                host: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux"),
                worktree: None,
                started_at: None,
            },
        );
        assert!(declared.server.is_none(), "no tools without an opt-in");
        // Names that would match nothing: the tools do not exist here.
        assert!(declared.disallowed_tools.is_empty());

        let v: serde_json::Value =
            serde_json::from_str(&declared.hook_settings).expect("valid json");
        let command = v["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(command.contains("--chat 'chat-7'"), "{command}");
    }

    #[test]
    fn the_matcher_is_the_exact_pattern_measured_against_the_cli() {
        // Pinned, not because the text is precious but because the CLI's matcher
        // semantics are undocumented and this specific string is the one a live
        // run confirmed fires for a `Bash` call. Changing it means re-running
        // `probes/matcher.py`, not just re-reading this file.
        assert_eq!(
            hook_matcher(),
            r"^(Bash|bash|shell|local_shell|run_terminal_cmd|mcp__oximux-computer-use__.*)$"
        );
    }

    #[test]
    fn the_matcher_covers_the_shell_as_well_as_the_tools() {
        // The half that is easy to lose: the policy refuses `osascript` inside a
        // Bash call, and that refusal only ever runs if the gate is asked about
        // Bash in the first place.
        let matcher = regex::Regex::new(&hook_matcher()).expect("a valid regex");
        for shell in crate::policy::SHELL_TOOLS {
            assert!(matcher.is_match(shell), "{shell} must reach the gate");
        }
        for tool in ["click", "type_text", "get_window_state"] {
            let name = format!("mcp__oximux-computer-use__{tool}");
            assert!(matcher.is_match(&name), "{name} must reach the gate");
        }
    }

    #[test]
    fn the_matcher_leaves_every_other_tool_alone() {
        // Each spawns a process per call, so a matcher that over-reaches is a
        // tax on every turn — and puts this in the path of tools it does not own.
        let matcher = regex::Regex::new(&hook_matcher()).expect("a valid regex");
        for tool in [
            "Read",
            "Edit",
            "WebFetch",
            "mcp__github__create_issue",
            // Near misses in both families.
            "BashOutput",
            "mcp__oximux-computer-use-extra__click",
        ] {
            assert!(!matcher.is_match(tool), "{tool} must not reach the gate");
        }
    }

    #[test]
    fn recognises_its_own_namespaced_tools() {
        assert!(is_computer_use_tool("mcp__oximux-computer-use__click"));
        assert!(is_computer_use_tool("mcp__oximux-computer-use__type_text"));
    }

    #[test]
    fn does_not_claim_other_servers_tools() {
        // A policy check keying off this must not capture unrelated tools, nor
        // miss one because a different server merely resembles the name.
        assert!(!is_computer_use_tool("Bash"));
        assert!(!is_computer_use_tool("mcp__other__click"));
        assert!(!is_computer_use_tool("mcp__oximux-computer-use-extra__click"));
        assert!(!is_computer_use_tool("computer-use__click"));
    }

    #[test]
    fn extracts_the_bare_tool_name() {
        assert_eq!(
            bare_tool_name("mcp__oximux-computer-use__type_text"),
            Some("type_text")
        );
        assert_eq!(bare_tool_name("mcp__other__click"), None);
        assert_eq!(bare_tool_name("Bash"), None);
        assert_eq!(bare_tool_name("mcp__oximux-computer-use__"), None);
    }

    #[test]
    fn prefix_matches_the_declared_server_name() {
        assert_eq!(tool_prefix(), format!("mcp__{SERVER_NAME}__"));
    }
}
