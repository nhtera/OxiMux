//! Naming contract for the screen-control MCP server.
//!
//! Just four items, and they live here rather than beside the driver they
//! describe for one reason: the transcript scrubber in [`crate::redact`] keys
//! on these names, and it has to keep working on platforms that have no driver
//! at all (a synced session store carries a macOS transcript onto any machine).
//! Splitting the *name* from the *implementation* is what lets the redaction
//! path stay unconditional.
//!
//! Everything here is a pure string predicate. `oximux-computer-use` re-exports
//! these under its own paths, so there is still exactly one definition.

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

    #[test]
    fn namespaced_tools_are_recognised() {
        assert!(is_computer_use_tool("mcp__oximux-computer-use__click"));
        assert!(!is_computer_use_tool("mcp__other-server__click"));
        assert!(!is_computer_use_tool("Bash"));
    }

    #[test]
    fn bare_name_strips_the_namespace() {
        assert_eq!(
            bare_tool_name("mcp__oximux-computer-use__screenshot"),
            Some("screenshot")
        );
        // A prefix with nothing after it names no tool.
        assert_eq!(bare_tool_name("mcp__oximux-computer-use__"), None);
        assert_eq!(bare_tool_name("Bash"), None);
    }
}
