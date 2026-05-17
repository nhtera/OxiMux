//! `Repository` — handle for one git working tree.
//!
//! Validates on open via `git rev-parse --show-toplevel` and stores the canonical
//! working-tree root. All subsequent git commands run from that root, so callers
//! can pass any subdirectory of the tree when opening.

use crate::diff::parse_unified_diff;
use crate::error::{GitError, Result};
use crate::process::GitCmd;
use crate::status;
use oximux_core::{FileDiff, GitState};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Diff command timeout. Diffs on monorepos can be several MB and slower
/// than the 10 s default; keep status calls fast while giving diff room.
const DIFF_TIMEOUT: Duration = Duration::from_secs(30);

/// Common args shared across all `git diff` invocations:
/// - `-p`: produce a patch (we only parse this format)
/// - `--no-color`: ANSI codes would corrupt our line parser
/// - `--no-ext-diff`: ignore user `diff.external` config so output is the
///   format we parse
///
/// `-z` (which the plan listed) is intentionally omitted: it only affects
/// `--raw`/`--name-only`/`--name-status`/`--numstat` output. For `-p` it's
/// a no-op, so we drop the noise.
const DIFF_BASE_ARGS: &[&str] = &["diff", "-p", "--no-color", "--no-ext-diff"];

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
            Err(GitError::Spawn {
                kind: std::io::ErrorKind::NotFound,
                ..
            }) => {
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

    /// Patch-format diff of the working tree against the index (changes not
    /// yet staged for commit).
    pub async fn diff_unstaged(&self) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&[], None).await
    }

    /// Patch-format diff of the index against HEAD (changes already
    /// `git add`ed but not committed).
    pub async fn diff_staged(&self) -> Result<Vec<FileDiff>> {
        self.diff_with_args(&["--cached"], None).await
    }

    /// Diff a single path. `path` is interpreted relative to the working
    /// tree root; absolute paths inside the working tree also work because
    /// git resolves them itself.
    ///
    /// Returns an empty `Vec` when the path has no diff in the requested
    /// stage (e.g. asking for staged changes on a file that's only modified
    /// in the worktree). Caller distinguishes "no changes" from "file
    /// missing" via `git status` on the same path — `git diff` does not
    /// error on an unmodified path.
    pub async fn diff_for_path(&self, path: &Path, staged: bool) -> Result<Vec<FileDiff>> {
        let extra: &[&str] = if staged { &["--cached"] } else { &[] };
        self.diff_with_args(extra, Some(path)).await
    }

    async fn diff_with_args(&self, extra: &[&str], path: Option<&Path>) -> Result<Vec<FileDiff>> {
        let mut cmd = GitCmd::new(&self.workdir)
            .timeout(DIFF_TIMEOUT)
            .args(DIFF_BASE_ARGS.iter().copied())
            .args(extra.iter().copied());
        if let Some(p) = path {
            cmd = cmd.arg("--").arg(p.as_os_str());
        }
        let out = cmd.run().await?;
        // git diff output is conceptually UTF-8 on macOS (filenames + patch
        // text); non-UTF-8 patches in binary files are filtered out via the
        // "Binary files X and Y differ" line, so the residue is always text.
        let raw = std::str::from_utf8(&out.stdout)
            .map_err(|e| GitError::parse(format!("diff stdout not utf-8: {e}")))?;
        let diffs = parse_unified_diff(raw)?;
        Ok(diffs)
    }
}
