//! The one way to add to a [`ConnectSpec`]: every writer merges, none assigns.
//!
//! A chat's spec has several independent writers — computer use (an MCP server,
//! stripped tools, a hook in the inline settings), Claude's fast mode, the
//! orchestration MCP, a spawn posture, and the host credential in `env`. Exactly
//! one `--settings` may reach Claude, and a plain assignment from any writer
//! silently erases what an earlier one declared (computer use used to do exactly
//! that). So each field has one merge rule here:
//!
//! - `mcp_servers`: appended, de-duplicated by name. A second declaration under
//!   the same name must be identical; a different one is a conflict.
//! - `disallowed_tools`: unioned.
//! - `settings_json`: deep-merged into one object. Objects merge key by key,
//!   arrays concatenate (hook lists, permission rules) without duplicates, and
//!   two different scalars at the same path are a conflict.
//! - `env`: per key; the same value twice is fine, a different one a conflict.
//!
//! Conflicts are errors, not last-writer-wins: two writers disagreeing about a
//! security-relevant field is a bug to surface, never one to resolve quietly.

use serde_json::{Map, Value};

use super::ConnectSpec;
use crate::thread::McpServerSpec;

/// Two writers declared different values for the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecConflict(pub String);

impl std::fmt::Display for SpecConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "launch spec conflict: {}", self.0)
    }
}

impl std::error::Error for SpecConflict {}

impl ConnectSpec {
    /// Declare a host MCP server for this session.
    pub fn add_mcp_server(&mut self, server: McpServerSpec) -> Result<(), SpecConflict> {
        match self.mcp_servers.iter().find(|s| s.name == server.name) {
            Some(existing) if *existing == server => Ok(()),
            Some(_) => Err(SpecConflict(format!("two different MCP servers named `{}`", server.name))),
            None => {
                self.mcp_servers.push(server);
                Ok(())
            }
        }
    }

    /// Strip these tools from the agent's surface, on top of any already stripped.
    pub fn add_disallowed_tools(&mut self, tools: impl IntoIterator<Item = String>) {
        for tool in tools {
            if !self.disallowed_tools.contains(&tool) {
                self.disallowed_tools.push(tool);
            }
        }
    }

    /// Deep-merge `overlay` (a JSON object) into the inline settings.
    pub fn merge_settings(&mut self, overlay: &Value) -> Result<(), SpecConflict> {
        let Some(overlay) = overlay.as_object() else {
            return Err(SpecConflict("settings overlay is not a JSON object".into()));
        };
        if overlay.is_empty() {
            return Ok(());
        }
        let mut base = match self.settings_json.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => Value::Object(Map::new()),
            Some(text) => match serde_json::from_str::<Value>(text) {
                Ok(v @ Value::Object(_)) => v,
                _ => return Err(SpecConflict("existing inline settings are not a JSON object".into())),
            },
        };
        deep_merge(&mut base, &Value::Object(overlay.clone()), "")?;
        self.settings_json = Some(base.to_string());
        Ok(())
    }

    /// Set one environment variable for the spawned agent.
    pub fn set_env(&mut self, key: &str, value: &str) -> Result<(), SpecConflict> {
        match self.env.iter().find(|(k, _)| k == key) {
            Some((_, v)) if v == value => Ok(()),
            Some(_) => Err(SpecConflict(format!("two different values for env `{key}`"))),
            None => {
                self.env.push((key.to_string(), value.to_string()));
                Ok(())
            }
        }
    }
}

fn deep_merge(base: &mut Value, overlay: &Value, path: &str) -> Result<(), SpecConflict> {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                let child = format!("{path}/{k}");
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v, &child)?,
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
            Ok(())
        }
        (Value::Array(b), Value::Array(o)) => {
            for v in o {
                if !b.contains(v) {
                    b.push(v.clone());
                }
            }
            Ok(())
        }
        (b, o) if *b == *o => Ok(()),
        (_, _) => Err(SpecConflict(format!("two different values at settings `{path}`"))),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::thread::{ChatBackend, Transport};

    fn spec() -> ConnectSpec {
        ConnectSpec::for_backend(
            &ChatBackend::stream_json(),
            std::path::PathBuf::from("/tmp"),
            None,
            None,
            None,
            None,
        )
    }

    fn server(name: &str, cmd: &str) -> McpServerSpec {
        McpServerSpec { name: name.into(), command: cmd.into(), args: vec![], env: vec![] }
    }

    #[test]
    fn mcp_servers_append_and_dedupe_by_name() {
        let mut s = spec();
        s.add_mcp_server(server("computer-use", "/bin/cu")).unwrap();
        s.add_mcp_server(server("oximux", "/bin/ox")).unwrap();
        s.add_mcp_server(server("computer-use", "/bin/cu")).unwrap();
        assert_eq!(s.mcp_servers.len(), 2);
        assert!(s.add_mcp_server(server("oximux", "/bin/other")).is_err());
    }

    #[test]
    fn disallowed_tools_union() {
        let mut s = spec();
        s.add_disallowed_tools(["Bash".to_string(), "Write".to_string()]);
        s.add_disallowed_tools(["Write".to_string(), "Edit".to_string()]);
        assert_eq!(s.disallowed_tools, ["Bash", "Write", "Edit"]);
    }

    #[test]
    fn settings_deep_merge_concatenates_hook_and_permission_arrays() {
        let mut s = spec();
        s.merge_settings(&json!({"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [1]}]}})).unwrap();
        s.merge_settings(&json!({
            "hooks": {"PreToolUse": [{"matcher": "mcp__x", "hooks": [2]}], "Stop": [{"hooks": [3]}]},
            "permissions": {"allow": ["Bash(cargo test:*)"]},
            "fastMode": true
        }))
        .unwrap();
        let v: Value = serde_json::from_str(s.settings_json.as_deref().unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2, "neither hook lost");
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(v["permissions"]["allow"][0], "Bash(cargo test:*)");
        assert_eq!(v["fastMode"], true);
        // The same value again is not a conflict; a different one is.
        s.merge_settings(&json!({"fastMode": true})).unwrap();
        assert!(s.merge_settings(&json!({"fastMode": false})).is_err());
    }

    #[test]
    fn a_non_object_existing_settings_is_a_conflict_not_a_silent_overwrite() {
        let mut s = spec();
        s.settings_json = Some("not json".into());
        assert!(s.merge_settings(&json!({"a": 1})).is_err());
        assert_eq!(s.settings_json.as_deref(), Some("not json"), "left untouched");
    }

    #[test]
    fn env_is_per_key_and_refuses_a_second_value() {
        let mut s = spec();
        s.set_env("OXIMUX_SESSION_ID", "a").unwrap();
        s.set_env("OXIMUX_SESSION_ID", "a").unwrap();
        s.set_env("ANTHROPIC_API_KEY", "k").unwrap();
        assert_eq!(s.env.len(), 2);
        assert!(s.set_env("OXIMUX_SESSION_ID", "b").is_err());
        assert_eq!(s.transport, Transport::StreamJson);
    }
}
