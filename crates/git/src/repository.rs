//! `Repository` — handle for one git working tree.
//!
//! Validates on open via `git rev-parse --show-toplevel` and stores the canonical
//! working-tree root. All subsequent git commands run from that root, so callers
//! can pass any subdirectory of the tree when opening.

use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::status;
use oximux_core::GitState;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Repository {
    workdir: PathBuf,
}

impl Repository {
    /// Open the repository that contains `path`. Returns `NotARepo` if `path`
    /// is not inside a git working tree (or the directory doesn't exist).
    /// Returns `NotInstalled` if the `git` binary is missing from PATH.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let res = match GitCmd::new(path)
            .args(["rev-parse", "--show-toplevel"])
            .run_raw()
            .await
        {
            Ok(r) => r,
            Err(GitError::Spawn(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                // Spawn returned NotFound — could be a missing `git` binary OR
                // a missing current_dir. If the path exists at this point, the
                // cwd is fine, so git itself is missing. (Pure error-path
                // disambiguation; not a TOCTOU pre-check on the success path.)
                if path.exists() {
                    return Err(GitError::NotInstalled);
                }
                return Err(GitError::NotARepo {
                    path: path.to_path_buf(),
                });
            }
            Err(_) => {
                return Err(GitError::NotARepo {
                    path: path.to_path_buf(),
                });
            }
        };
        if !res.status.success() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        // Path bytes are macOS-native UTF-8 (v1 is macOS-only). On other
        // platforms a non-UTF-8 toplevel would Parse-error here; revisit if
        // we ever port to Linux.
        let toplevel = String::from_utf8(res.stdout)
            .map_err(|e| GitError::parse(format!("toplevel not utf-8: {e}")))?
            .trim()
            .to_string();
        if toplevel.is_empty() {
            return Err(GitError::NotARepo {
                path: path.to_path_buf(),
            });
        }
        Ok(Self {
            workdir: PathBuf::from(toplevel),
        })
    }

    /// Canonical working-tree root.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Run `git status --porcelain=v2 --branch -z` and parse into `GitState`.
    pub async fn status(&self) -> Result<GitState> {
        let out = GitCmd::new(&self.workdir)
            .args(["status", "--porcelain=v2", "--branch", "-z"])
            .run()
            .await?;
        status::parse_porcelain_v2(&out.stdout)
    }
}
