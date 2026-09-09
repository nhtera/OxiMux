//! Reads of `git config` values OxiMux needs to name things.
//!
//! Repository-scoped on purpose: `git config user.name` run inside a worktree
//! answers with the repo-local override when there is one and falls through to
//! the global otherwise, which is exactly the value the user would expect to
//! see on a commit from that repository. Reading the global directly would get
//! the wrong answer for anyone who sets a work identity per-repo.

use crate::error::Result;
use crate::process::GitCmd;
use crate::repository::Repository;

impl Repository {
    /// `git config user.name`, trimmed; `None` when unset or empty.
    ///
    /// Never an error: an unset `user.name` is an ordinary state (a fresh
    /// container, CI, a machine where only `user.email` was configured), and
    /// `git config --get` signals it with exit code 1, which [`GitCmd::run`]
    /// would otherwise raise. Callers want "is there a name?", so the missing
    /// case is `Ok(None)` rather than something to handle.
    pub async fn user_name(&self) -> Result<Option<String>> {
        let raw = GitCmd::new(self.workdir())
            .args(["config", "--get", "user.name"])
            .run_raw()
            .await?;
        if !raw.status.success() {
            return Ok(None);
        }
        let name = String::from_utf8_lossy(&raw.stdout).trim().to_string();
        Ok((!name.is_empty()).then_some(name))
    }
}
