//! Client-side `fs/*` request handlers for the ACP worker.
//!
//! ACP lets an agent delegate file reads/writes to the client (per the client's
//! advertised `FileSystemCapabilities`). OxiMux advertises `read_text_file` +
//! `write_text_file` and serves them here, **scoped to the session cwd tree**: a
//! path that resolves outside the tree is denied with a JSON-RPC error + a log
//! (no silent widening). Canonicalizing the deepest existing ancestor defeats
//! symlink / `..` escapes; the tail (a not-yet-existing write target) is appended
//! lexically and the final path is re-checked for containment.
//!
//! `terminal/*` is deliberately NOT served yet (no verified local consumer;
//! heavier lifecycle) — see the phase's deferred follow-up.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use agent_client_protocol::Error;
use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, WriteTextFileRequest, WriteTextFileResponse,
};

/// JSON-RPC "invalid params" — used when a path escapes the session cwd tree.
const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC "internal error" — used for a failed filesystem op.
const INTERNAL_ERROR: i32 = -32603;

/// Serve `fs/read_text_file`: read the (cwd-guarded) file, honoring the optional
/// 1-based `line` start and `limit` line count.
pub(crate) fn read_text_file(
    req: &ReadTextFileRequest,
    cwd: &Path,
) -> Result<ReadTextFileResponse, Error> {
    let path = resolve_in_cwd(&req.path, cwd)?;
    let content = std::fs::read_to_string(&path).map_err(|e| {
        Error::new(INTERNAL_ERROR, format!("fs/read_text_file {}: {e}", path.display()))
    })?;
    Ok(ReadTextFileResponse::new(slice_lines(&content, req.line, req.limit)))
}

/// Serve `fs/write_text_file`: write the content to the (cwd-guarded) path,
/// creating parent directories inside the tree as needed.
pub(crate) fn write_text_file(
    req: &WriteTextFileRequest,
    cwd: &Path,
) -> Result<WriteTextFileResponse, Error> {
    let path = resolve_in_cwd(&req.path, cwd)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::new(INTERNAL_ERROR, format!("fs/write_text_file mkdir {}: {e}", parent.display()))
        })?;
    }
    std::fs::write(&path, &req.content).map_err(|e| {
        Error::new(INTERNAL_ERROR, format!("fs/write_text_file {}: {e}", path.display()))
    })?;
    Ok(WriteTextFileResponse::new())
}

/// Resolve `path` (absolute, or relative to `cwd`) and confirm it stays within
/// the session cwd tree. Canonicalizes the deepest existing ancestor (resolving
/// real symlinks/`..`), re-appends the not-yet-existing tail lexically, then
/// re-checks containment. On escape (or an unresolvable cwd) → a JSON-RPC error
/// that is logged; never widens.
fn resolve_in_cwd(path: &Path, cwd: &Path) -> Result<PathBuf, Error> {
    let cwd_canon = cwd
        .canonicalize()
        .map_err(|e| Error::new(INTERNAL_ERROR, format!("session cwd {}: {e}", cwd.display())))?;
    let joined = if path.is_absolute() { path.to_path_buf() } else { cwd_canon.join(path) };

    let (existing, tail) = split_existing(&joined);
    let base = existing.canonicalize().map_err(|e| {
        Error::new(INTERNAL_ERROR, format!("resolve {}: {e}", existing.display()))
    })?;
    let mut resolved = base;
    for comp in tail.components() {
        match comp {
            Component::Normal(c) => resolved.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            // A prefix/root in the tail can't appear (the tail is always relative);
            // reject defensively rather than silently normalizing.
            _ => return Err(deny(path, cwd)),
        }
    }
    if resolved.starts_with(&cwd_canon) {
        Ok(resolved)
    } else {
        Err(deny(path, cwd))
    }
}

/// Split `path` into (deepest existing ancestor, remaining tail). For a path that
/// fully exists the tail is empty; for a new write target the tail is the
/// not-yet-created suffix.
fn split_existing(path: &Path) -> (PathBuf, PathBuf) {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            // No file name (root or `..`) and still doesn't exist — stop; the
            // caller's canonicalize will surface the error.
            None => break,
        }
    }
    (existing, tail.iter().rev().collect())
}

/// A logged "path escapes the session cwd tree" denial.
fn deny(path: &Path, cwd: &Path) -> Error {
    tracing::warn!(
        requested = %path.display(),
        cwd = %cwd.display(),
        "acp fs request denied: path escapes session cwd tree",
    );
    Error::new(INVALID_PARAMS, "path escapes the session working directory")
}

/// Slice a file's text by an optional 1-based `line` start and `limit` line count.
/// With neither, the whole content is returned unchanged.
fn slice_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    if line.is_none() && limit.is_none() {
        return content.to_string();
    }
    let start = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
    let lines: Vec<&str> = content.lines().collect();
    let end = match limit {
        Some(n) => start.saturating_add(n as usize).min(lines.len()),
        None => lines.len(),
    };
    lines.get(start..end).map(|s| s.join("\n")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_req(path: &str, line: Option<u32>, limit: Option<u32>) -> ReadTextFileRequest {
        ReadTextFileRequest::new("s", path).line(line).limit(limit)
    }

    fn tmp_dir() -> PathBuf {
        // Per-process unique dir under the OS temp root (no `Date`/`rand` in the
        // workflow harness, so key on the pid + a static counter).
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("oximux-acp-fs-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_happy_path_and_line_limit_slice() {
        let dir = tmp_dir();
        std::fs::write(dir.join("f.txt"), "l1\nl2\nl3\nl4\nl5").unwrap();
        // Whole file.
        let all = read_text_file(&read_req("f.txt", None, None), &dir).unwrap();
        assert_eq!(all.content, "l1\nl2\nl3\nl4\nl5");
        // 1-based line 2, limit 2 → l2, l3.
        let slice = read_text_file(&read_req("f.txt", Some(2), Some(2)), &dir).unwrap();
        assert_eq!(slice.content, "l2\nl3");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_happy_path_creates_file() {
        let dir = tmp_dir();
        let req = WriteTextFileRequest::new("s", "sub/new.txt", "hello");
        write_text_file(&req, &dir).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(), "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_dotdot_escape_is_denied() {
        let dir = tmp_dir();
        let err = read_text_file(&read_req("../../etc/passwd", None, None), &dir).unwrap_err();
        assert_eq!(i32::from(err.code), INVALID_PARAMS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absolute_out_of_tree_path_is_denied() {
        let dir = tmp_dir();
        let err = read_text_file(&read_req("/etc/hosts", None, None), &dir).unwrap_err();
        assert_eq!(i32::from(err.code), INVALID_PARAMS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_escape_is_denied() {
        let dir = tmp_dir();
        let req = WriteTextFileRequest::new("s", "../evil.txt", "x");
        let err = write_text_file(&req, &dir).unwrap_err();
        assert_eq!(i32::from(err.code), INVALID_PARAMS);
        assert!(!dir.parent().unwrap().join("evil.txt").exists(), "escape must not write");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let dir = tmp_dir();
        let err = read_text_file(&read_req("nope.txt", None, None), &dir).unwrap_err();
        // In-tree but missing → internal (IO) error, not a containment denial.
        assert_eq!(i32::from(err.code), INTERNAL_ERROR);
        std::fs::remove_dir_all(&dir).ok();
    }
}
