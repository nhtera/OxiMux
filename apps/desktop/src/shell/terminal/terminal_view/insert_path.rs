//! Insert path: type a picked file or directory at the cursor, quoted for the
//! shell and never as control bytes.

use std::path::Path;

use super::*;
use crate::shell::terminal::shell_quote::{ShellDialect, quote_for};

impl TerminalView {
    /// Type `path` at the cursor: relative to the terminal's directory when it
    /// is inside it, else under `~/` or absolute; quoted only as needed; no
    /// trailing space or newline, quoted the way the pane's shell reads it.
    /// `false` (nothing typed) for a path that cannot be typed safely there: a
    /// control byte (it would act, not appear), or what `cmd` cannot quote.
    pub fn insert_path(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let shown = path_for_insertion(path, self.live_cwd().as_deref(), dirs::home_dir().as_deref());
        let dialect = ShellDialect::of_program(&super::spawn_settings::spawn_shell_program());
        let Some(quoted) = quote_for(&shown, dialect) else {
            return false;
        };
        self.send_as_paste(&quoted, false, cx);
        true
    }

    /// The shell's directory now: its OSC 7 report, else the OS's answer for
    /// the shell process. A daemon-hosted terminal tracks no OSC 7 cwd, so the
    /// hint alone is `None` for most terminals.
    pub fn live_cwd(&self) -> Option<std::path::PathBuf> {
        self.cwd_hint().or_else(|| self.os_pid().and_then(crate::shell::cwd_resolver::cwd_of_pid))
    }
}

/// How `path` should read at a prompt in `cwd`.
fn path_for_insertion(path: &Path, cwd: Option<&Path>, home: Option<&Path>) -> String {
    let is_dir = path.to_string_lossy().ends_with('/');
    let suffix = if is_dir { "/" } else { "" };
    if let Some(rel) = cwd.and_then(|c| under(path, c)) {
        let rel = rel.to_string_lossy();
        let rel = rel.trim_end_matches('/');
        return match rel {
            "" => ".".into(),
            // A file or folder literally named `~…` must not read as a home.
            r if r.starts_with('~') => format!("./{r}{suffix}"),
            r => format!("{r}{suffix}"),
        };
    }
    if let Some(rel) = home.and_then(|h| under(path, h)) {
        let rel = rel.to_string_lossy();
        let rel = rel.trim_end_matches('/');
        return if rel.is_empty() { "~/".into() } else { format!("~/{rel}{suffix}") };
    }
    path.to_string_lossy().into_owned()
}

/// `path` relative to `base`, comparing resolved paths when the spellings
/// differ: a terminal reports its cwd resolved (`/private/tmp/x` on macOS,
/// where `/tmp` is a symlink) while a project may be rooted at `/tmp/x`.
fn under(path: &Path, base: &Path) -> Option<std::path::PathBuf> {
    if let Ok(rel) = path.strip_prefix(base) {
        return Some(rel.to_path_buf());
    }
    let (path, base) = (std::fs::canonicalize(path).ok()?, std::fs::canonicalize(base).ok()?);
    path.strip_prefix(&base).ok().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_read_relative_then_home_then_absolute() {
        let cwd = Path::new("/repo");
        let home = Path::new("/Users/me");
        assert_eq!(path_for_insertion(Path::new("/repo/src/main.rs"), Some(cwd), Some(home)), "src/main.rs");
        assert_eq!(path_for_insertion(Path::new("/repo/src/"), Some(cwd), Some(home)), "src/");
        assert_eq!(path_for_insertion(Path::new("/repo"), Some(cwd), Some(home)), ".");
        assert_eq!(path_for_insertion(Path::new("/Users/me/notes.md"), Some(cwd), Some(home)), "~/notes.md");
        assert_eq!(path_for_insertion(Path::new("/etc/hosts"), Some(cwd), Some(home)), "/etc/hosts");
        assert_eq!(path_for_insertion(Path::new("/etc/hosts"), None, None), "/etc/hosts");
        // A folder literally called `~` inside the cwd is not the home dir.
        assert_eq!(path_for_insertion(Path::new("/repo/~/"), Some(cwd), Some(home)), "./~/");
        assert_eq!(path_for_insertion(Path::new("/repo/~x"), Some(cwd), Some(home)), "./~x");
        assert_eq!(path_for_insertion(Path::new("/Users/me/"), Some(cwd), Some(home)), "~/");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_of_the_cwd_still_reads_relative() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::fs::write(real.join("src/a b.md"), "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // The project is rooted at the link; the shell reports the real path.
        let picked = link.join("src/a b.md");
        assert_eq!(path_for_insertion(&picked, Some(&real), None), "src/a b.md");
        assert_eq!(path_for_insertion(&link.join("src/"), Some(&real), None), "src/");
        // And the other way round.
        assert_eq!(path_for_insertion(&real.join("src/a b.md"), Some(&link), None), "src/a b.md");
    }
}
