//! MCP servers the *host* declares for a spawned agent.
//!
//! OxiMux historically injected no MCP configuration at all: the Claude arg
//! builder emitted only `--model`/`--effort`/`--resume`, and the ACP arm sent a
//! hardcoded empty `mcpServers`. Whatever servers an agent saw came from the
//! user's own config, discovered via `--setting-sources`. This type is the seam
//! that lets OxiMux add its own — a stdio server it supplies and supervises.
//!
//! Deliberately stdio-only. Every server OxiMux hosts is a local sidecar it
//! spawns; HTTP/SSE transports exist in ACP's vocabulary but have no host-side
//! use case here, and modelling them would be shape without a consumer.
//!
//! Lives in `agent-core` (gpui/tokio-free) so the phone's Rust core still
//! cross-compiles, and so the wire rendering is unit-testable without spawning
//! anything.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// One MCP server the host declares for an agent it spawns.
///
/// `name` is the tool-namespace prefix the agent sees (Claude surfaces the
/// server's tools as `mcp__<name>__<tool>`), so it is part of the contract with
/// anything that later matches on tool names — not a cosmetic label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    /// Executable path or a bare name resolvable on PATH.
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment for the server process, as `(key, value)` pairs. A
    /// `Vec` rather than a map so ordering is stable in tests and on the wire.
    pub env: Vec<(String, String)>,
}

impl McpServerSpec {
    /// A stdio server with no args and no extra env.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// This server as one entry of Claude's `mcpServers` object.
    fn to_claude_entry(&self) -> Value {
        let env: Map<String, Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        json!({
            "type": "stdio",
            "command": self.command,
            "args": self.args,
            "env": Value::Object(env),
        })
    }
}

/// Render `specs` as the JSON payload for Claude's `--mcp-config`.
///
/// Returns `None` for an empty slice, which is what keeps the no-servers
/// invocation byte-identical to the pre-seam one: the caller emits no flag at
/// all rather than an empty config. `claude --help` documents `--mcp-config
/// <configs...>` as "Load MCP servers from JSON files or strings", so the
/// string form is passed inline and no temp file is needed.
///
/// Note this does *not* pass `--strict-mcp-config`: OxiMux spawns with
/// `--setting-sources user,project,local` precisely so the chat sees the user's
/// own servers, and strict mode would suppress them. Host-declared servers are
/// additive.
pub fn to_claude_mcp_config(specs: &[McpServerSpec]) -> Option<String> {
    if specs.is_empty() {
        return None;
    }
    let servers: Map<String, Value> = specs
        .iter()
        .map(|s| (s.name.clone(), s.to_claude_entry()))
        .collect();
    Some(json!({ "mcpServers": Value::Object(servers) }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_specs_render_no_config() {
        // The load-bearing case: with no host servers the caller must emit no
        // `--mcp-config` flag, so today's invocation is unchanged.
        assert_eq!(to_claude_mcp_config(&[]), None);
    }

    #[test]
    fn one_server_renders_claude_shape() {
        let spec = McpServerSpec::new("oximux-computer-use", "cua-driver")
            .args(vec!["mcp".into(), "--socket".into(), "/tmp/s.sock".into()])
            .env(vec![("CUA_DRIVER_EMBEDDED".into(), "1".into())]);
        let raw = to_claude_mcp_config(std::slice::from_ref(&spec)).expect("some");
        let v: Value = serde_json::from_str(&raw).expect("valid json");

        let entry = &v["mcpServers"]["oximux-computer-use"];
        assert_eq!(entry["type"], "stdio");
        assert_eq!(entry["command"], "cua-driver");
        assert_eq!(entry["args"], json!(["mcp", "--socket", "/tmp/s.sock"]));
        assert_eq!(entry["env"]["CUA_DRIVER_EMBEDDED"], "1");
    }

    #[test]
    fn multiple_servers_all_present() {
        let specs = vec![
            McpServerSpec::new("alpha", "a-bin"),
            McpServerSpec::new("beta", "b-bin"),
        ];
        let v: Value =
            serde_json::from_str(&to_claude_mcp_config(&specs).expect("some")).expect("valid json");
        let servers = v["mcpServers"].as_object().expect("object");
        assert_eq!(servers.len(), 2);
        assert!(servers.contains_key("alpha"));
        assert!(servers.contains_key("beta"));
    }

    #[test]
    fn no_args_or_env_still_renders_empty_collections() {
        // Some MCP hosts reject a server entry missing `args`/`env`, so both are
        // always present even when empty (same reasoning as ACP's `mcpServers: []`).
        let v: Value = serde_json::from_str(
            &to_claude_mcp_config(&[McpServerSpec::new("bare", "bin")]).expect("some"),
        )
        .expect("valid json");
        assert_eq!(v["mcpServers"]["bare"]["args"], json!([]));
        assert_eq!(v["mcpServers"]["bare"]["env"], json!({}));
    }

    #[test]
    fn spec_round_trips_through_serde() {
        // The spec rides the persisted/threaded launch config, so it must
        // survive a serialize/deserialize hop unchanged.
        let spec = McpServerSpec::new("n", "c")
            .args(vec!["a".into()])
            .env(vec![("K".into(), "V".into())]);
        let raw = serde_json::to_string(&spec).expect("serialize");
        assert_eq!(
            serde_json::from_str::<McpServerSpec>(&raw).expect("deserialize"),
            spec
        );
    }
}
