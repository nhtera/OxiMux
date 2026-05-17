//! Error type for the git layer.

use std::path::PathBuf;

#[derive(Debug, Clone, thiserror::Error)]
pub enum GitError {
    /// Path is not inside a git working tree.
    #[error("not a git repository: {path}")]
    NotARepo { path: PathBuf },

    /// The `git` binary is not available on PATH.
    #[error("git binary not found on PATH")]
    NotInstalled,

    /// `git` binary spawn / pipe I/O failed (permission denied, broken pipe, …).
    ///
    /// Stored as a `(ErrorKind, String)` pair rather than wrapping
    /// `std::io::Error` directly so the enum can derive `Clone` — required
    /// for `PollState` which travels through a `tokio::sync::watch` channel.
    #[error("failed to spawn git: {kind:?}: {msg}")]
    Spawn {
        kind: std::io::ErrorKind,
        msg: String,
    },

    /// Process exceeded its timeout budget.
    #[error("git command timed out after {secs}s")]
    Timeout { secs: u64 },

    /// Process exited non-zero.
    #[error("git exited with code {code}: {stderr}")]
    NonZero { code: i32, stderr: String },

    /// Output couldn't be parsed (porcelain v2 malformed, unexpected EOF, etc.).
    #[error("parse error: {reason}")]
    Parse { reason: String },

    /// Caller-side error — invalid hunk index, attempted hunk-staging on a
    /// binary diff, etc. Surfaced to callers as a programmer/UI bug, not a
    /// git process failure.
    #[error("invalid input: {reason}")]
    InvalidInput { reason: String },
}

impl GitError {
    pub(crate) fn parse(reason: impl Into<String>) -> Self {
        Self::Parse {
            reason: reason.into(),
        }
    }

    pub(crate) fn invalid_input(reason: impl Into<String>) -> Self {
        Self::InvalidInput {
            reason: reason.into(),
        }
    }

    pub(crate) fn spawn(e: std::io::Error) -> Self {
        Self::Spawn {
            kind: e.kind(),
            msg: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, GitError>;
