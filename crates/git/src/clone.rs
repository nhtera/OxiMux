//! `git clone <url> <dest>` — create a local repository from a remote URL.
//!
//! Unlike the other operations in this crate, clone does not run inside an
//! existing repo: the working dir is `dest`'s parent (created if missing).
//! Cloning pulls over the network, so it gets a far longer timeout than the
//! local-query default.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Network clones of a large repo can take minutes; allow generous headroom
/// before the hard timeout trips.
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// Clone `url` into `dest`. `dest` must not already exist; its parent
/// directory is created if missing. Returns the cloned-into path on success.
pub async fn clone_repo(url: &str, dest: &Path) -> Result<PathBuf> {
    let url = url.trim();
    if url.is_empty() {
        return Err(GitError::parse("clone URL is empty"));
    }
    if dest.exists() {
        return Err(GitError::parse(format!(
            "destination already exists: {}",
            dest.display()
        )));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| GitError::parse("clone destination has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| GitError::parse(format!("create clone parent dir: {e}")))?;
    GitCmd::new(parent)
        .args(["clone", url])
        .arg(dest.as_os_str())
        .timeout(CLONE_TIMEOUT)
        .run()
        .await?;
    Ok(dest.to_path_buf())
}

/// Derive a local directory name from a clone URL: the final path segment
/// with any trailing `.git` removed. Handles both `https://host/owner/repo.git`
/// and `git@host:owner/repo.git` shapes. Returns `None` when nothing usable
/// can be extracted (the caller should reject the URL).
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    // Split on both `/` and `:` so the `owner/repo` of an scp-style SSH URL
    // (`git@host:owner/repo.git`) yields `repo`, not `host:owner/repo`.
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or("")
        .trim();
    let name = last.strip_suffix(".git").unwrap_or(last).trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_from_https_url() {
        assert_eq!(
            repo_name_from_url("https://github.com/foo/bar.git").as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn name_from_https_url_no_suffix() {
        assert_eq!(
            repo_name_from_url("https://github.com/foo/bar").as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn name_from_ssh_scp_url() {
        assert_eq!(
            repo_name_from_url("git@github.com:foo/bar.git").as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn name_strips_trailing_slash() {
        assert_eq!(
            repo_name_from_url("https://github.com/foo/bar/").as_deref(),
            Some("bar")
        );
    }

    #[test]
    fn name_from_empty_is_none() {
        assert_eq!(repo_name_from_url("   "), None);
    }
}
