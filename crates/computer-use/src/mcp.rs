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

/// OxiMux's bundle identifier, passed as the driver's advisory host label so
/// `check_permissions` output names who asked. Must track `CFBundleIdentifier`
/// in `assets/Info.plist`.
const HOST_BUNDLE_ID: &str = "dev.nhtera.oximux";

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
/// All three parts travel together deliberately. Handing an agent the server
/// without the other two is the failure this type exists to prevent: the
/// runtime policy would then run only in the permission modes that actually
/// prompt, and measurement showed two ordinary situations where none does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    pub server: McpServerSpec,
    /// Namespaced tool names for `--disallowedTools`. The CLI removes these
    /// from the agent's surface entirely, in every permission mode, and a deny
    /// outranks a user's own `permissions.allow` rule.
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
    /// The chat's worktree and start time, for build provenance. `None` leaves
    /// the hook asking about every target rather than trusting its own builds.
    pub worktree: Option<&'a Path>,
    pub started_at: Option<std::time::SystemTime>,
}

/// Build the full declaration for a verified driver.
///
/// The single way to obtain the server spec, so no call site can take one part
/// and leave the rest.
pub fn declaration(driver: &Path, hook: &HookSpec<'_>) -> Declaration {
    let mut command = format!(
        "{} --chat {} --grants {}",
        shell_quote(&hook.command.display().to_string()),
        shell_quote(hook.chat),
        shell_quote(&hook.grants.display().to_string()),
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
                // Scoped to this server's tools so the gate is never consulted
                // about a tool it does not own.
                "matcher": format!("{}.*", tool_prefix()),
                "hooks": [{ "type": "command", "command": command }],
            }]
        }
    })
    .to_string();

    Declaration {
        server: server_spec(driver),
        disallowed_tools: crate::tools::forbidden_names()
            .map(|tool| format!("{}{tool}", tool_prefix()))
            .collect(),
        hook_settings,
    }
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
            Path::new("/bin/cua-driver"),
            &HookSpec {
                command: Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux-screen-gate"),
                chat: "chat-7",
                grants: Path::new("/data/grants.json"),
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
        assert_eq!(entry["matcher"], "mcp__oximux-computer-use__.*");
        let command = entry["hooks"][0]["command"].as_str().expect("command");
        assert!(command.contains("oximux-screen-gate"), "{command}");
        assert!(command.contains("--chat 'chat-7'"), "{command}");
        assert!(command.contains("--grants '/data/grants.json'"), "{command}");
        assert!(command.contains("--since 1700000000"), "{command}");
    }

    #[test]
    fn hook_paths_survive_a_space_or_an_apostrophe() {
        // A worktree under "~/My Projects" would otherwise split into extra
        // arguments, the gate would reject its own command line, and the chat
        // would run unenforced with nothing to show for it.
        let declared = declaration(
            Path::new("/bin/cua-driver"),
            &HookSpec {
                command: Path::new("/Apps/Oxi Mux.app/gate"),
                chat: "chat-1",
                grants: Path::new("/data/grants.json"),
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
