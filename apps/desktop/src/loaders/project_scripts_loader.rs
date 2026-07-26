//! Loader for per-project lifecycle scripts from `.oximux/scripts.toml`.
//!
//! Scripts are per-project only (no global tier, unlike `commands.toml`) —
//! setup/run/cleanup are inherently repo-specific. The file is intended to be
//! committed to git so a team shares it; do not store secrets there.
//!
//! A missing file is a no-op (the all-`None` default → no buttons surface). A
//! malformed file is logged and skipped; it never crashes the application.

use std::path::Path;

use oximux_settings::ProjectScripts;
use oximux_settings::project_scripts::FILE_NAME;

/// Load the lifecycle scripts for `project_root` (or a worktree of it — the
/// `.oximux/` dir is committed, so a worktree carries the same file). Reads
/// `<project_root>/.oximux/scripts.toml`. Never panics; returns the default
/// on a missing or malformed file.
pub fn load_for_project(project_root: &Path) -> ProjectScripts {
    let path = project_root.join(".oximux").join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => match ProjectScripts::from_toml_str(&text) {
            Ok(scripts) => scripts,
            Err(err) => {
                tracing::warn!(
                    ?path,
                    %err,
                    "scripts.toml parse failed; ignoring per-project lifecycle scripts"
                );
                ProjectScripts::default()
            }
        },
        Err(_) => ProjectScripts::default(), // absent file → silent default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_settings::ScriptKind;
    use std::io::Write;

    fn write_scripts(root: &Path, content: &str) {
        let dir = root.join(".oximux");
        std::fs::create_dir_all(&dir).unwrap();
        let mut f = std::fs::File::create(dir.join(FILE_NAME)).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = load_for_project(tmp.path());
        assert_eq!(scripts, ProjectScripts::default());
    }

    #[test]
    fn valid_file_loads() {
        let tmp = tempfile::tempdir().unwrap();
        write_scripts(
            tmp.path(),
            "auto_setup = true\nsetup = \"pnpm i\"\nrun = \"pnpm dev\"\n",
        );
        let scripts = load_for_project(tmp.path());
        assert!(scripts.auto_setup);
        assert_eq!(scripts.script(ScriptKind::Setup), Some("pnpm i"));
        assert_eq!(scripts.script(ScriptKind::Run), Some("pnpm dev"));
        assert_eq!(scripts.script(ScriptKind::Cleanup), None);
    }

    #[test]
    fn malformed_file_returns_default_without_panic() {
        let tmp = tempfile::tempdir().unwrap();
        write_scripts(tmp.path(), "setup = [[[broken");
        let scripts = load_for_project(tmp.path());
        assert_eq!(scripts, ProjectScripts::default());
    }
}
