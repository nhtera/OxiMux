//! The git surface: working-tree status, per-path diffs, and the staging writes.
//!
//! Status and diffs cross as **canonical JSON** rather than as projected uniffi
//! records, for the same reason [`RemoteEvent`](crate::ffi_types::RemoteEvent)
//! does: they are deep, evolving trees (nested hunks, rename origins, mode
//! changes), and mirroring every variant into an FFI type would mean re-cutting
//! the bindings — and every consumer — each time the desktop's diff vocabulary
//! grows. The write calls stay typed, because their shapes are flat and stable.
//!
//! Paths are repository-relative, exactly as a status listing reported them. The
//! host re-contains each one against the repository regardless, so a path from
//! here cannot reach outside it.

use crate::client::MobileClient;
use crate::ffi_types::MobileError;

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Working-tree status of the repository a session lives in, as JSON:
    /// `{ branch, upstream, ahead, behind, files: [...] }`.
    pub async fn git_status(&self, session_id: String) -> Result<String, MobileError> {
        let session = self.shared.session()?;
        let status = session
            .git_status(&session_id)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        serde_json::to_string(&status).map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Diff one path, as a JSON array of file diffs. `staged` picks index-vs-HEAD;
    /// `untracked` selects the read-off-disk path git itself will not diff.
    pub async fn git_diff(
        &self,
        session_id: String,
        path: String,
        staged: bool,
        untracked: bool,
    ) -> Result<String, MobileError> {
        let session = self.shared.session()?;
        let files = session
            .git_diff(&session_id, &path, staged, untracked)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))?;
        serde_json::to_string(&files).map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Stage paths into the index. Refused for a read-only device.
    pub async fn git_stage(
        &self,
        session_id: String,
        paths: Vec<String>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session
            .git_stage(&session_id, &paths)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Remove paths from the index, leaving the worktree untouched.
    pub async fn git_unstage(
        &self,
        session_id: String,
        paths: Vec<String>,
    ) -> Result<(), MobileError> {
        let session = self.shared.session()?;
        session
            .git_unstage(&session_id, &paths)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }

    /// Commit what is already staged, returning the new HEAD sha.
    ///
    /// Takes no paths deliberately: the path-taking git variant pre-stages with
    /// `git add`, which would flatten hunk-level partial staging set up on the
    /// desktop and commit more than was chosen there — and the phone cannot see
    /// that partial staging to know it is doing so.
    pub async fn git_commit(
        &self,
        session_id: String,
        message: String,
    ) -> Result<String, MobileError> {
        let session = self.shared.session()?;
        session
            .git_commit(&session_id, &message)
            .await
            .map_err(|e| MobileError::Rpc(e.to_string()))
    }
}
