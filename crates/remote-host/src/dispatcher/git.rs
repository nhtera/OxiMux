//! The git RPCs: working-tree status, per-path diffs, and the staging/commit
//! writes.
//!
//! Split from [`super::handlers`] because git is its own surface with its own
//! rules — every one of these resolves a repository from the session's `cwd`
//! (so remote git access inherits the session ACL rather than opening a second,
//! wider authorization surface), contains every client-supplied path against the
//! repo workdir at this boundary, and never forwards raw git error text (it
//! routinely embeds absolute host paths).
//!
//! Reads gate on `is_allowed_for`; writes additionally gate on `may_write`, so a
//! read-only device can inspect a repository it may not change.

// The helpers below return `Result<T, Response>` — the error *is* the reply to
// send, already built. `Response` is a large enum, but boxing it here would add an
// allocation on every rejection path and a deref at each of the call sites, to
// move a value that is about to be encoded and written to the wire regardless.
#![allow(clippy::result_large_err)]

use std::path::{Path, PathBuf};

use oximux_remote_proto::messages::{
    DiffHunkWire, DiffLineKindWire, DiffLineWire, DiffStatusWire, FileDiffWire, GitFileWire,
    GitStatusWire, IndexStatusWire, WorktreeStatusWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

/// A repository resolved from a session, ready for a git call.
struct SessionRepo {
    repo: oximux_git::repository::Repository,
}

impl Dispatcher {
    /// Resolve the session's repository behind the appropriate authorization
    /// gate, or return the `Response` to send instead.
    ///
    /// `write` picks the gate: `may_write` for anything that mutates the index or
    /// history, `is_allowed_for` for reads. Centralized so a new git RPC cannot
    /// accidentally ship with the weaker check — the failure mode would be a
    /// read-only device silently gaining commit rights.
    async fn session_repo(
        &self,
        peer: &Peer,
        session_id: &str,
        write: bool,
    ) -> Result<SessionRepo, Response> {
        let allowed = if write {
            self.auth.may_write(peer, session_id)
        } else {
            self.auth.is_allowed_for(peer, session_id)
        };
        if !allowed {
            return Err(Response::Error(RpcError::Unauthorized));
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Err(Response::Error(RpcError::UnknownSession));
        };
        let Some(cwd) = handle.meta_snapshot().cwd else {
            return Err(Response::Error(RpcError::BadRequest(
                "session has no working directory".into(),
            )));
        };
        // Git error text routinely embeds absolute paths ("fatal: not a git
        // repository: /Users/…"), so it is logged host-side and never forwarded.
        match oximux_git::repository::Repository::open(&cwd).await {
            Ok(repo) => Ok(SessionRepo { repo }),
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "open repository failed");
                Err(Response::Error(RpcError::Internal("git unavailable".into())))
            }
        }
    }
}

impl SessionRepo {
    fn workdir(&self) -> &Path {
        self.repo.workdir()
    }

    /// The workdir canonicalized — the form a path must be stripped against.
    /// Canonicalizing the ROOT matters as much as canonicalizing the path: on
    /// macOS `/var` resolves to `/private/var`, and stripping an uncanonicalized
    /// root off a canonicalized path silently no-ops, leaking absolute host
    /// paths to the client.
    fn canonical_root(&self) -> PathBuf {
        self.workdir().canonicalize().unwrap_or_else(|_| self.workdir().to_path_buf())
    }

    /// Contain one client-supplied path against the repository.
    ///
    /// The rejection deliberately says nothing about what does or does not exist
    /// on disk — it must not become a probe for the host's filesystem.
    fn contain(&self, session_id: &str, path: &str) -> Result<PathBuf, Response> {
        oximux_git::path_guard::contained_path(self.workdir(), Path::new(path)).map_err(|_| {
            tracing::warn!(session = %session_id, "rejected out-of-repository path");
            Response::Error(RpcError::BadRequest("path is outside the repository".into()))
        })
    }

    /// Contain every path in a batch, failing the whole request on the first
    /// escape. All-or-nothing on purpose: silently staging the acceptable subset
    /// of a request that also tried to reach outside the repo would report
    /// success for an operation the client did not ask for.
    fn contain_all(&self, session_id: &str, paths: &[String]) -> Result<Vec<PathBuf>, Response> {
        if paths.is_empty() {
            return Err(Response::Error(RpcError::BadRequest("no paths given".into())));
        }
        paths.iter().map(|p| self.contain(session_id, p)).collect()
    }
}

impl Dispatcher {
    /// Working-tree status of the repository the session lives in.
    pub(super) async fn git_status(&self, peer: &Peer, session_id: &str) -> Response {
        let repo = match self.session_repo(peer, session_id, false).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        match repo.repo.status().await {
            Ok(state) => Response::GitStatus(to_status_wire(state)),
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "git status failed");
                Response::Error(RpcError::Internal("git status failed".into()))
            }
        }
    }

    /// Diff one path in the session's repository.
    ///
    /// Only the tracked branch shells out to git; `diff_for_untracked` reads the
    /// file directly, so nothing downstream would catch a traversal there — which
    /// is why containment happens here, at the boundary.
    pub(super) async fn git_diff(
        &self,
        peer: &Peer,
        session_id: &str,
        path: &str,
        staged: bool,
        untracked: bool,
    ) -> Response {
        let repo = match self.session_repo(peer, session_id, false).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        let contained = match repo.contain(session_id, path) {
            Ok(p) => p,
            Err(resp) => return resp,
        };
        let diffs = if untracked {
            repo.repo.diff_for_untracked(&contained).await
        } else {
            repo.repo.diff_for_path(&contained, staged).await
        };
        // Paths go back out repository-relative. The untracked codepath echoes the
        // absolute path it was handed, and shipping that would disclose the host's
        // directory layout (home dir, usernames) to the client — and break the
        // contract that a listed path can be echoed straight back on a diff request.
        let root = repo.canonical_root();
        match diffs {
            Ok(files) => Response::GitDiff(
                files.into_iter().map(|d| to_file_diff_wire(d, &root)).collect(),
            ),
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "git diff failed");
                Response::Error(RpcError::Internal("git diff failed".into()))
            }
        }
    }

    /// Stage paths into the index.
    pub(super) async fn git_stage(
        &self,
        peer: &Peer,
        session_id: &str,
        paths: &[String],
    ) -> Response {
        self.stage_or_unstage(peer, session_id, paths, true).await
    }

    /// Remove paths from the index, leaving the worktree untouched.
    pub(super) async fn git_unstage(
        &self,
        peer: &Peer,
        session_id: &str,
        paths: &[String],
    ) -> Response {
        self.stage_or_unstage(peer, session_id, paths, false).await
    }

    async fn stage_or_unstage(
        &self,
        peer: &Peer,
        session_id: &str,
        paths: &[String],
        stage: bool,
    ) -> Response {
        let repo = match self.session_repo(peer, session_id, true).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        let contained = match repo.contain_all(session_id, paths) {
            Ok(p) => p,
            Err(resp) => return resp,
        };
        let refs: Vec<&Path> = contained.iter().map(PathBuf::as_path).collect();
        let result = if stage {
            repo.repo.stage_paths(&refs).await
        } else {
            repo.repo.unstage_paths(&refs).await
        };
        let what = if stage { "git stage" } else { "git unstage" };
        match result {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "{what} failed");
                Response::Error(RpcError::Internal(format!("{what} failed")))
            }
        }
    }

    /// Commit what is already staged, returning the new HEAD sha.
    ///
    /// Deliberately **staged-only** (`commit`, not `commit_paths`): the
    /// path-taking variant pre-stages with `git add`, which would overwrite any
    /// hunk-level partial staging the user set up on the desktop and commit more
    /// than they selected. A remote client cannot see that partial staging, so it
    /// must not be able to silently discard it.
    pub(super) async fn git_commit(
        &self,
        peer: &Peer,
        session_id: &str,
        message: &str,
    ) -> Response {
        let repo = match self.session_repo(peer, session_id, true).await {
            Ok(r) => r,
            Err(resp) => return resp,
        };
        // Rejected here so the empty-message case is a clear BadRequest rather
        // than surfacing as a generic internal failure from git's stderr.
        if message.trim().is_empty() {
            return Response::Error(RpcError::BadRequest("commit message is empty".into()));
        }
        match repo.repo.commit(message).await {
            Ok(sha) => Response::GitCommitted { sha },
            Err(e) => {
                // Covers "nothing staged" and a failing pre-commit hook as well as
                // real faults; the text is not forwarded, so the client sees one
                // generic failure either way.
                tracing::warn!(error = %e, session = %session_id, "git commit failed");
                Response::Error(RpcError::Internal("git commit failed".into()))
            }
        }
    }
}

/// Map one file's diff onto the wire shape. Paths cross as repository-relative
/// strings (never host-absolute); rename/copy origins come along so a client can
/// render "was X".
fn to_file_diff_wire(d: oximux_core::FileDiff, root: &std::path::Path) -> FileDiffWire {
    use oximux_core::DiffStatus as S;
    let status = match d.status {
        S::Added => DiffStatusWire::Added,
        S::Modified => DiffStatusWire::Modified,
        S::Deleted => DiffStatusWire::Deleted,
        S::Renamed { from, similarity } => {
            DiffStatusWire::Renamed { from: from.to_string_lossy().into_owned(), similarity }
        }
        S::Copied { from, similarity } => {
            DiffStatusWire::Copied { from: from.to_string_lossy().into_owned(), similarity }
        }
        S::ModeChanged { old_mode, new_mode } => {
            DiffStatusWire::ModeChanged { old_mode, new_mode }
        }
        S::Binary => DiffStatusWire::Binary,
    };
    FileDiffWire {
        path: d.path.strip_prefix(root).unwrap_or(&d.path).to_string_lossy().into_owned(),
        status,
        large: d.large,
        hunks: d
            .hunks
            .into_iter()
            .map(|h| DiffHunkWire {
                old_start: h.old_start,
                old_lines: h.old_lines,
                new_start: h.new_start,
                new_lines: h.new_lines,
                header_suffix: h.header_suffix,
                lines: h
                    .lines
                    .into_iter()
                    .map(|l| DiffLineWire {
                        kind: match l.kind {
                            oximux_core::DiffLineKind::Context => DiffLineKindWire::Context,
                            oximux_core::DiffLineKind::Added => DiffLineKindWire::Added,
                            oximux_core::DiffLineKind::Removed => DiffLineKindWire::Removed,
                            oximux_core::DiffLineKind::NoNewlineHint => {
                                DiffLineKindWire::NoNewlineHint
                            }
                        },
                        content: l.content,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Map the desktop's `GitState` onto the dependency-minimal wire shape. Paths are
/// emitted as the repository-relative strings git reported; a client echoes one
/// back on a diff request and the host contains it again there — this direction
/// never widens what the client may ask for.
fn to_status_wire(state: oximux_core::GitState) -> GitStatusWire {
    GitStatusWire {
        branch: state.branch,
        upstream: state.upstream,
        ahead: state.ahead,
        behind: state.behind,
        files: state
            .files
            .into_iter()
            .map(|f| GitFileWire {
                path: f.path.to_string_lossy().into_owned(),
                index: to_index_wire(f.index),
                worktree: to_worktree_wire(f.worktree),
                unstaged_lines: f.line_counts,
                staged_lines: f.staged_line_counts,
            })
            .collect(),
    }
}

fn to_index_wire(status: oximux_core::IndexStatus) -> IndexStatusWire {
    use oximux_core::IndexStatus as S;
    match status {
        S::Unmodified => IndexStatusWire::Unmodified,
        S::Modified => IndexStatusWire::Modified,
        S::Added => IndexStatusWire::Added,
        S::Deleted => IndexStatusWire::Deleted,
        S::Renamed => IndexStatusWire::Renamed,
        S::Copied => IndexStatusWire::Copied,
        S::Untracked => IndexStatusWire::Untracked,
        S::Ignored => IndexStatusWire::Ignored,
        S::Unmerged => IndexStatusWire::Unmerged,
    }
}

fn to_worktree_wire(status: oximux_core::WorktreeStatus) -> WorktreeStatusWire {
    use oximux_core::WorktreeStatus as S;
    match status {
        S::Unmodified => WorktreeStatusWire::Unmodified,
        S::Modified => WorktreeStatusWire::Modified,
        S::Deleted => WorktreeStatusWire::Deleted,
        S::Renamed => WorktreeStatusWire::Renamed,
        S::Untracked => WorktreeStatusWire::Untracked,
        S::Ignored => WorktreeStatusWire::Ignored,
        S::Unmerged => WorktreeStatusWire::Unmerged,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use oximux_core::{FileStatus, GitState, IndexStatus, WorktreeStatus};

    use super::{IndexStatusWire, WorktreeStatusWire, to_status_wire};

    /// The desktop's status maps onto the wire shape without losing branch
    /// context, per-file codes, or either line-count pair.
    #[test]
    fn git_state_maps_onto_the_wire_shape() {
        let state = GitState {
            branch: Some("main".into()),
            upstream: Some("origin/main".into()),
            ahead: 2,
            behind: 1,
            files: vec![FileStatus {
                path: PathBuf::from("src/lib.rs"),
                index: IndexStatus::Modified,
                worktree: WorktreeStatus::Unmodified,
                rename: None,
                line_counts: Some((3, 1)),
                staged_line_counts: Some((10, 2)),
                conflict_kind: None,
            }],
            ..Default::default()
        };

        let wire = to_status_wire(state);

        assert_eq!(wire.branch.as_deref(), Some("main"));
        assert_eq!(wire.upstream.as_deref(), Some("origin/main"));
        assert_eq!((wire.ahead, wire.behind), (2, 1));
        assert_eq!(wire.files.len(), 1);
        let f = &wire.files[0];
        assert_eq!(f.path, "src/lib.rs", "paths cross as repo-relative strings");
        assert_eq!(f.index, IndexStatusWire::Modified);
        assert_eq!(f.worktree, WorktreeStatusWire::Unmodified);
        assert_eq!(f.unstaged_lines, Some((3, 1)));
        assert_eq!(f.staged_lines, Some((10, 2)), "staged counts are not confused with unstaged");
    }

    /// An untracked file keeps its code across the mapping (the pair that decides
    /// which diff codepath a client asks for).
    #[test]
    fn untracked_status_survives_the_mapping() {
        let state = GitState {
            files: vec![FileStatus {
                path: PathBuf::from("new.txt"),
                index: IndexStatus::Untracked,
                worktree: WorktreeStatus::Untracked,
                rename: None,
                line_counts: None,
                staged_line_counts: None,
                conflict_kind: None,
            }],
            ..Default::default()
        };

        let wire = to_status_wire(state);

        assert_eq!(wire.files[0].index, IndexStatusWire::Untracked);
        assert_eq!(wire.files[0].worktree, WorktreeStatusWire::Untracked);
        assert_eq!(wire.branch, None, "a detached state has no branch");
    }
}
