//! Loader for user-defined custom commands from TOML config files.
//!
//! Reads from:
//! - Global: `~/Library/Application Support/dev.nhtera.oximux/commands.toml`
//! - Per-project: `<project_root>/.oximux/commands.toml`
//!
//! The per-project file is intended to be committed to git so teams can
//! share commands. Do not store secrets there.
//!
//! Both files are parsed with `CustomCommandsFile::from_toml_str`. A
//! missing file is a no-op (empty list). A malformed file is logged and
//! skipped — it never crashes the application. The merged result uses
//! `load_and_merge` from `oximux-settings` with an empty workspace slice
//! (workspace-scoped commands are a future extension).

use std::path::{Path, PathBuf};

use oximux_settings::{CustomCommand, CustomCommandsFile, load_and_merge};

const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";

/// Returns the global commands.toml path, or `None` when `dirs::data_dir()`
/// is unavailable (rare on macOS, but defensive).
fn global_commands_path() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR).join(oximux_settings::custom_commands::FILE_NAME))
}

/// Load and parse one commands TOML file. Returns an empty `Vec` when the
/// file does not exist; logs a warning and returns empty on parse failure.
fn load_from_path(path: &Path) -> Vec<CustomCommand> {
    match std::fs::read_to_string(path) {
        Ok(text) => match CustomCommandsFile::from_toml_str(&text) {
            Ok(f) => f.commands,
            Err(err) => {
                tracing::warn!(
                    ?path,
                    %err,
                    "commands.toml parse failed; skipping custom commands from this file"
                );
                Vec::new()
            }
        },
        Err(_) => Vec::new(), // absent file → silent empty
    }
}

/// Load global + project commands and merge them. Called on startup and on
/// every project switch. Never panics; returns an empty `Vec` on any error.
///
/// `project_root` is the path to the active project directory.
pub fn load_for_project(project_root: &Path) -> Vec<CustomCommand> {
    let global = global_commands_path()
        .map(|p| load_from_path(&p))
        .unwrap_or_default();

    let project_path = project_root
        .join(".oximux")
        .join(oximux_settings::custom_commands::FILE_NAME);
    let project = load_from_path(&project_path);

    load_and_merge(&global, &project, &[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_toml(dir: &std::path::Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn missing_project_file_returns_empty_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        // Non-existent project root — no .oximux/commands.toml
        let result = load_from_path(&tmp.path().join("commands.toml"));
        assert!(result.is_empty());
    }

    #[test]
    fn malformed_file_returns_empty_and_does_not_panic() {
        let tmp = tempfile::tempdir().unwrap();
        write_toml(tmp.path(), "commands.toml", "[[commands\nbroken=");
        let result = load_from_path(&tmp.path().join("commands.toml"));
        assert!(result.is_empty());
    }

    #[test]
    fn valid_file_loads_correctly() {
        let tmp = tempfile::tempdir().unwrap();
        write_toml(
            tmp.path(),
            "commands.toml",
            r#"
[[commands]]
name = "run"
prompt = "cargo run"

[[commands]]
name = "test"
prompt = "cargo test"
"#,
        );
        let result = load_from_path(&tmp.path().join("commands.toml"));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "run");
        assert_eq!(result[1].name, "test");
    }
}
