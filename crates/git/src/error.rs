//! Error type for the git layer.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Path is not inside a git working tree.
    #[error("not a git repository: {path}")]
    NotARepo { path: PathBuf },

    /// The `git` binary is not available on PATH.
    #[error("git binary not found on PATH")]
    NotInstalled,

    /// `git` binary spawn failed for some other reason (permission denied, etc.).
    #[error("failed to spawn git: {0}")]
    Spawn(#[from] std::io::Error),

    /// Process exceeded its timeout budget.
    #[error("git command timed out after {secs}s")]
    Timeout { secs: u64 },

    /// Process exited non-zero.
    #[error("git exited with code {code}: {stderr}")]
    NonZero { code: i32, stderr: String },

    /// Output couldn't be parsed (porcelain v2 malformed, unexpected EOF, etc.).
    #[error("parse error: {reason}")]
    Parse { reason: String },
}

impl GitError {
    pub(crate) fn parse(reason: impl Into<String>) -> Self {
        Self::Parse {
            reason: reason.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, GitError>;
