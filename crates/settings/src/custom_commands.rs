//! User-defined custom commands loaded from TOML config files.
//!
//! Commands are defined in `commands.toml` in the global app data dir
//! and/or a per-project `.oximux/commands.toml` (git-committable so
//! teams can share commands). The per-project file overrides global
//! entries by name. Do not put secrets in project-level files —
//! `.oximux/commands.toml` is intended to be committed to git.
//!
//! Parsing is tolerant: unknown TOML keys are ignored for forward-compat;
//! malformed files return an error (logged by the caller, never panics).

use serde::{Deserialize, Serialize};

/// File name for both global and per-project command files.
pub const FILE_NAME: &str = "commands.toml";

/// A single user-defined command: a display name and a prompt string
/// sent verbatim to the active agent session when invoked. The optional
/// `scope` field is reserved for future per-workspace routing; it is
/// stored but not yet acted upon in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCommand {
    pub name: String,
    pub prompt: String,
    /// Optional future scope hint (e.g. "agent", "shell"). Ignored by
    /// the dispatcher in v1; parsed and preserved for forward-compat.
    pub scope: Option<String>,
}

impl Default for CustomCommand {
    fn default() -> Self {
        Self {
            name: String::new(),
            prompt: String::new(),
            scope: None,
        }
    }
}

/// Root wrapper for the TOML file: `[[commands]]` array of tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomCommandsFile {
    pub commands: Vec<CustomCommand>,
}

impl CustomCommandsFile {
    /// Parse a TOML document. Unknown keys are tolerated. Malformed input
    /// returns an error; the caller is responsible for logging and falling
    /// back to an empty list.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a TOML document (used to seed a default file).
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

/// Merge global, project, and workspace command lists with name-keyed
/// precedence: workspace overrides project overrides global. Commands
/// that share a name are deduplicated — the higher-precedence entry wins.
/// The output order is: global entries (possibly replaced) in their
/// original order, then project-only entries, then workspace-only entries.
///
/// This function is pure and has no I/O or GPUI dependency — it can be
/// called from any crate.
pub fn load_and_merge(
    global: &[CustomCommand],
    project: &[CustomCommand],
    workspace: &[CustomCommand],
) -> Vec<CustomCommand> {
    // Build the merged set in precedence order (workspace > project >
    // global). Use a Vec to preserve a stable, predictable output order
    // while deduplicating by name.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut result: Vec<CustomCommand> = Vec::new();

    // Walk all three tiers from highest to lowest precedence, keeping the
    // first occurrence of each name (the winner). Then reverse for output.
    let all_in_precedence = workspace.iter().chain(project.iter()).chain(global.iter());
    for cmd in all_in_precedence {
        let key = cmd.name.to_lowercase();
        if seen.insert(key) {
            result.push(cmd.clone());
        }
    }

    // Reverse: we collected highest-precedence-first; final list should
    // list global-flavored commands first (lowest id), workspace last.
    // But the spec says "workspace overrides project overrides global by
    // name" — the order within each tier should be preserved, and the tiers
    // themselves should appear in the intuitive global → project → workspace
    // order for display purposes.
    //
    // Rebuild in display order: iterate global, then project-only, then
    // workspace-only, each time picking the winning version from `result`.
    let mut display: Vec<CustomCommand> = Vec::new();
    for tier in [global, project, workspace] {
        for cmd in tier {
            let key = cmd.name.to_lowercase();
            // Find the winning entry for this name (may be from a higher tier).
            if let Some(pos) = result.iter().position(|c| c.name.to_lowercase() == key) {
                let winner = result.remove(pos);
                // Only add if we haven't already placed this name in `display`.
                if !display.iter().any(|c| c.name.to_lowercase() == winner.name.to_lowercase()) {
                    display.push(winner);
                }
            }
        }
    }
    // Any leftover entries in `result` (shouldn't happen, but be safe).
    display.extend(result);
    display
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, prompt: &str) -> CustomCommand {
        CustomCommand {
            name: name.to_string(),
            prompt: prompt.to_string(),
            scope: None,
        }
    }

    #[test]
    fn global_only_returns_global_order() {
        let g = vec![cmd("run", "cargo run"), cmd("test", "cargo test")];
        let result = load_and_merge(&g, &[], &[]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "run");
        assert_eq!(result[1].name, "test");
    }

    #[test]
    fn project_overrides_global_by_name() {
        let g = vec![cmd("run", "cargo run"), cmd("test", "cargo test")];
        let p = vec![cmd("run", "cargo run --release")];
        let result = load_and_merge(&g, &p, &[]);
        assert_eq!(result.len(), 2);
        let run = result.iter().find(|c| c.name == "run").unwrap();
        assert_eq!(run.prompt, "cargo run --release");
        let test = result.iter().find(|c| c.name == "test").unwrap();
        assert_eq!(test.prompt, "cargo test");
    }

    #[test]
    fn workspace_overrides_project_and_global() {
        let g = vec![cmd("run", "cargo run")];
        let p = vec![cmd("run", "cargo run --release")];
        let w = vec![cmd("run", "cargo run --profile bench")];
        let result = load_and_merge(&g, &p, &w);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].prompt, "cargo run --profile bench");
    }

    #[test]
    fn project_only_entry_added_after_global() {
        let g = vec![cmd("test", "cargo test")];
        let p = vec![cmd("lint", "cargo clippy")];
        let result = load_and_merge(&g, &p, &[]);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "test");
        assert_eq!(result[1].name, "lint");
    }

    #[test]
    fn empty_all_returns_empty() {
        let result = load_and_merge(&[], &[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn case_insensitive_name_dedup() {
        let g = vec![cmd("Run", "cargo run")];
        let p = vec![cmd("run", "cargo run --release")];
        let result = load_and_merge(&g, &p, &[]);
        // "Run" (global) vs "run" (project) — project wins; one entry
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].prompt, "cargo run --release");
    }

    #[test]
    fn from_toml_str_parses_valid_file() {
        let toml = r#"
[[commands]]
name = "test"
prompt = "cargo test"

[[commands]]
name = "build"
prompt = "cargo build"
"#;
        let f = CustomCommandsFile::from_toml_str(toml).expect("parse");
        assert_eq!(f.commands.len(), 2);
        assert_eq!(f.commands[0].name, "test");
        assert_eq!(f.commands[1].prompt, "cargo build");
    }

    #[test]
    fn from_toml_str_tolerates_unknown_keys() {
        let toml = r#"
[[commands]]
name = "test"
prompt = "cargo test"
future_field = "ignored"
"#;
        let f = CustomCommandsFile::from_toml_str(toml).expect("unknown keys ignored");
        assert_eq!(f.commands.len(), 1);
    }

    #[test]
    fn from_toml_str_malformed_returns_error() {
        let toml = "[[commands\nbroken = [[[";
        let result = CustomCommandsFile::from_toml_str(toml);
        assert!(result.is_err(), "malformed TOML should return an error");
    }

    #[test]
    fn empty_file_returns_empty_commands() {
        let f = CustomCommandsFile::from_toml_str("").expect("empty is valid TOML");
        assert!(f.commands.is_empty());
    }

    #[test]
    fn round_trip_serialization() {
        let original = CustomCommandsFile {
            commands: vec![
                cmd("run", "cargo run"),
                cmd("test", "cargo test"),
            ],
        };
        let toml = original.to_toml_string();
        let parsed = CustomCommandsFile::from_toml_str(&toml).expect("round-trip");
        assert_eq!(original, parsed);
    }
}
