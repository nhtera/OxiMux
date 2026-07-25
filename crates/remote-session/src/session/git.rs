//! The client half of the git RPCs: working-tree status, per-path diffs, and the
//! staging/commit writes.
//!
//! Paths cross the wire **repository-relative**, exactly as a
//! [`Response::GitStatus`] listing reported them — a path from a status listing
//! can be handed straight back here. The host re-contains every one against the
//! repository regardless, so nothing a client sends can reach outside it.

use oximux_remote_proto::messages::{FileDiffWire, GitStatusWire};
use oximux_remote_proto::proto::{Request, Response};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// Working-tree status of the repository the session lives in.
    pub async fn git_status(&self, session_id: &str) -> Result<GitStatusWire> {
        let req = Request::GitStatus { session_id: session_id.to_string() };
        match self.call(req).await? {
            Response::GitStatus(status) => Ok(status),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitStatus" }),
        }
    }

    /// Diff one path. `staged` picks index-vs-HEAD; `untracked` selects the
    /// read-off-disk path git itself will not diff.
    pub async fn git_diff(
        &self,
        session_id: &str,
        path: &str,
        staged: bool,
        untracked: bool,
    ) -> Result<Vec<FileDiffWire>> {
        let req = Request::GitDiff {
            session_id: session_id.to_string(),
            path: path.to_string(),
            staged,
            untracked,
        };
        match self.call(req).await? {
            Response::GitDiff(files) => Ok(files),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitDiff" }),
        }
    }

    /// Stage paths into the index. Refused for a read-only device.
    pub async fn git_stage(&self, session_id: &str, paths: &[String]) -> Result<()> {
        let req =
            Request::GitStage { session_id: session_id.to_string(), paths: paths.to_vec() };
        self.expect_ack(req).await
    }

    /// Remove paths from the index, leaving the worktree untouched.
    pub async fn git_unstage(&self, session_id: &str, paths: &[String]) -> Result<()> {
        let req =
            Request::GitUnstage { session_id: session_id.to_string(), paths: paths.to_vec() };
        self.expect_ack(req).await
    }

    /// Commit what is already staged, returning the new HEAD sha.
    ///
    /// Takes no paths on purpose. The path-taking git variant pre-stages with
    /// `git add`, which would overwrite hunk-level partial staging set up on the
    /// desktop and commit more than was selected — and a remote client cannot see
    /// that partial staging to know it is doing so.
    pub async fn git_commit(&self, session_id: &str, message: &str) -> Result<String> {
        let req = Request::GitCommit {
            session_id: session_id.to_string(),
            message: message.to_string(),
        };
        match self.call(req).await? {
            Response::GitCommitted { sha } => Ok(sha),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "GitCommitted" }),
        }
    }
}
