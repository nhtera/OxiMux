//! Map an opened file to the language server that should drive it.
//!
//! A small static table keyed by file extension. Each entry names the server
//! program, its launch args, and the LSP `languageId` to advertise in
//! `didOpen`. The program is resolved against `$PATH` so an absent server
//! degrades gracefully (one log line, no editor disruption) rather than
//! spawning a doomed child. Servers are described by language, not brand, in
//! the public surface; the concrete program names live only in the table.

use std::path::Path;

/// A resolved language server ready to spawn for a buffer.
pub struct LspServerSpec {
    /// Absolute path to the server binary (resolved from `$PATH`).
    pub program: String,
    /// Launch arguments (e.g. the `--stdio` transport flag some servers need).
    pub args: Vec<String>,
    /// LSP `languageId` for the buffer (`didOpen`'s `textDocument.languageId`).
    pub language_id: &'static str,
}

/// One row of the extension → server table, before `$PATH` resolution.
struct ServerEntry {
    program: &'static str,
    args: &'static [&'static str],
    language_id: &'static str,
}

/// Resolve the language server for `path`, or `None` when the extension has no
/// mapping or the server binary isn't installed on `$PATH`.
pub fn resolve_lsp_server(path: &Path) -> Option<LspServerSpec> {
    let entry = entry_for_path(path)?;
    let program = resolve_on_path(entry.program)?;
    Some(LspServerSpec {
        program,
        args: entry.args.iter().map(|s| s.to_string()).collect(),
        language_id: entry.language_id,
    })
}

/// The static mapping. Returns `None` for unsupported extensions.
fn entry_for_path(path: &Path) -> Option<ServerEntry> {
    let ext = path.extension().and_then(|s| s.to_str())?;
    let entry = match ext {
        "rs" => ServerEntry {
            program: "rust-analyzer",
            args: &[],
            language_id: "rust",
        },
        "ts" | "mts" | "cts" => ServerEntry {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: "typescript",
        },
        "tsx" => ServerEntry {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: "typescriptreact",
        },
        "js" | "mjs" | "cjs" => ServerEntry {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: "javascript",
        },
        "jsx" => ServerEntry {
            program: "typescript-language-server",
            args: &["--stdio"],
            language_id: "javascriptreact",
        },
        "py" | "pyi" => ServerEntry {
            program: "pyright-langserver",
            args: &["--stdio"],
            language_id: "python",
        },
        "go" => ServerEntry {
            program: "gopls",
            args: &[],
            language_id: "go",
        },
        _ => return None,
    };
    Some(entry)
}

/// Resolve `program` against `$PATH`. If it already contains a path separator
/// it's used verbatim (when it exists). Returns the absolute path string, or
/// `None` when no executable is found — the caller then skips LSP for the file.
fn resolve_on_path(program: &str) -> Option<String> {
    if program.contains('/') {
        let p = Path::new(program);
        return is_executable_file(p).then(|| program.to_string());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// `true` when `path` is a regular file with an executable bit set.
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && (meta.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn maps_known_extensions_to_language_ids() {
        // Resolution depends on the server being installed, so assert the
        // table arm (language_id/args) via the inner entry function directly.
        let rs = entry_for_path(&PathBuf::from("a.rs")).unwrap();
        assert_eq!(rs.language_id, "rust");
        assert!(rs.args.is_empty());

        let ts = entry_for_path(&PathBuf::from("a.ts")).unwrap();
        assert_eq!(ts.language_id, "typescript");
        assert_eq!(ts.args, &["--stdio"]);

        let tsx = entry_for_path(&PathBuf::from("a.tsx")).unwrap();
        assert_eq!(tsx.language_id, "typescriptreact");

        let py = entry_for_path(&PathBuf::from("a.py")).unwrap();
        assert_eq!(py.language_id, "python");
        assert_eq!(py.program, "pyright-langserver");

        let go = entry_for_path(&PathBuf::from("a.go")).unwrap();
        assert_eq!(go.language_id, "go");
        assert!(go.args.is_empty());
    }

    #[test]
    fn unknown_extension_has_no_server() {
        assert!(entry_for_path(&PathBuf::from("a.txt")).is_none());
        assert!(entry_for_path(&PathBuf::from("README")).is_none());
    }

    #[test]
    fn missing_binary_resolves_to_none() {
        assert!(resolve_on_path("definitely-not-a-real-lsp-binary-xyz").is_none());
    }
}
