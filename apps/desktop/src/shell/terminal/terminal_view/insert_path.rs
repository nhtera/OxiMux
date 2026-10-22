//! Insert path: type a picked file or directory at the cursor, quoted for the
//! shell and never as control bytes.

use std::path::Path;

use super::*;
use crate::shell::terminal::shell_quote::{has_control, posix_quote, powershell_quote};

impl TerminalView {
    /// Type `path` at the cursor: relative to the terminal's directory when it
    /// is inside it, else under `~/` or absolute; quoted only as needed; no
    /// trailing space or newline. `false` (nothing typed) for a path holding a
    /// control byte — a name like that would act, not appear.
    pub fn insert_path(&mut self, path: &Path, cx: &mut Context<Self>) -> bool {
        let shown = path_for_insertion(path, self.cwd_hint().as_deref(), dirs::home_dir().as_deref());
        if has_control(&shown) {
            return false;
        }
        let quoted = if cfg!(windows) { powershell_quote(&shown) } else { posix_quote(&shown) };
        self.send_as_paste(&quoted, false, cx);
        true
    }
}

/// How `path` should read at a prompt in `cwd`.
fn path_for_insertion(path: &Path, cwd: Option<&Path>, home: Option<&Path>) -> String {
    let is_dir = path.to_string_lossy().ends_with('/');
    let suffix = if is_dir { "/" } else { "" };
    if let Some(rel) = cwd.and_then(|c| path.strip_prefix(c).ok()) {
        let rel = rel.to_string_lossy();
        let rel = rel.trim_end_matches('/');
        return match rel {
            "" => ".".into(),
            // A file or folder literally named `~…` must not read as a home.
            r if r.starts_with('~') => format!("./{r}{suffix}"),
            r => format!("{r}{suffix}"),
        };
    }
    if let Some(rel) = home.and_then(|h| path.strip_prefix(h).ok()) {
        let rel = rel.to_string_lossy();
        let rel = rel.trim_end_matches('/');
        return if rel.is_empty() { "~/".into() } else { format!("~/{rel}{suffix}") };
    }
    path.to_string_lossy().into_owned()
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
}
