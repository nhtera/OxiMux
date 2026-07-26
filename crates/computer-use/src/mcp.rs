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
pub const SERVER_NAME: &str = "computer-use";

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

    #[test]
    fn recognises_its_own_namespaced_tools() {
        assert!(is_computer_use_tool("mcp__computer-use__click"));
        assert!(is_computer_use_tool("mcp__computer-use__type_text"));
    }

    #[test]
    fn does_not_claim_other_servers_tools() {
        // A policy check keying off this must not capture unrelated tools, nor
        // miss one because a different server merely resembles the name.
        assert!(!is_computer_use_tool("Bash"));
        assert!(!is_computer_use_tool("mcp__other__click"));
        assert!(!is_computer_use_tool("mcp__computer-use-extra__click"));
        assert!(!is_computer_use_tool("computer-use__click"));
    }

    #[test]
    fn extracts_the_bare_tool_name() {
        assert_eq!(
            bare_tool_name("mcp__computer-use__type_text"),
            Some("type_text")
        );
        assert_eq!(bare_tool_name("mcp__other__click"), None);
        assert_eq!(bare_tool_name("Bash"), None);
        assert_eq!(bare_tool_name("mcp__computer-use__"), None);
    }

    #[test]
    fn prefix_matches_the_declared_server_name() {
        assert_eq!(tool_prefix(), format!("mcp__{SERVER_NAME}__"));
    }
}
