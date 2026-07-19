//! The authenticated session-RPC handlers — each re-checks the device's scope
//! (`is_allowed_for`) before touching the [`SessionRegistry`], so revocation and
//! per-device scoping bite on every call.

use oximux_agents::session_registry::SessionHandle;
use oximux_remote_proto::messages::{
    DiffHunkWire, DiffLineKindWire, DiffLineWire, DiffStatusWire, FileDiffWire, GitFileWire,
    GitStatusWire, IndexStatusWire, ResolvePermissionReq, SendPromptReq, SessionInfoWire,
    SessionStatusWire, SessionSummary, WorktreeStatusWire,
};
use oximux_remote_proto::proto::{Response, RpcError};
use oximux_remote_proto::HostEvent;

use super::Dispatcher;
use crate::auth::AppPubkey;

impl Dispatcher {
    pub(super) fn list_sessions(&self, pubkey: &AppPubkey) -> Response {
        let sessions = self
            .registry
            .statuses()
            .into_iter()
            // A session-scoped device must only learn about sessions it may act
            // on — never enumerate the full session set.
            .filter(|(session_id, _)| self.auth.is_allowed_for(pubkey, session_id))
            .map(|(session_id, status)| {
                // Title/model are published by the desktop view via the registry's
                // session meta. A session that hasn't been titled yet (no
                // `TitleUpdated` so far) falls back to its id so a row is never blank.
                let meta = self
                    .registry
                    .get(&session_id)
                    .map(|handle| handle.meta_snapshot())
                    .unwrap_or_default();
                SessionSummary {
                    title: meta.title.unwrap_or_else(|| session_id.clone()),
                    model: meta.model,
                    last_seq: status.last_seq,
                    awaiting_permission: status.awaiting_permission,
                    session_id,
                }
            })
            .collect();
        Response::Sessions(sessions)
    }

    pub(super) fn session_info(&self, pubkey: &AppPubkey, session_id: &str) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let status = handle.status_snapshot();
        let meta = handle.meta_snapshot();
        Response::SessionInfo(SessionInfoWire {
            summary: SessionSummary {
                session_id: session_id.to_string(),
                title: meta.title.unwrap_or_else(|| session_id.to_string()),
                model: meta.model,
                last_seq: status.last_seq,
                awaiting_permission: status.awaiting_permission,
            },
            // Session inventory (cwd/tools/mcp/agents) lives on the view's thread,
            // not the registry — populated when register-on-connect carries meta.
            meta: Default::default(),
        })
    }

    pub(super) fn send_prompt(&self, pubkey: &AppPubkey, req: SendPromptReq) -> Response {
        self.scoped(pubkey, &req.session_id, |h| h.send_prompt(&req.text, &req.images))
    }

    pub(super) fn resolve_permission(&self, pubkey: &AppPubkey, req: ResolvePermissionReq) -> Response {
        if !self.auth.is_allowed_for(pubkey, &req.session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let decision = match req.decision() {
            Ok(d) => d,
            Err(_) => return Response::Error(RpcError::BadRequest("bad decision payload".into())),
        };
        let Some(handle) = self.registry.get(&req.session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        match handle.resolve_permission(&req.request_id, decision) {
            Ok(true) => Response::Ack,
            // A benign race: someone already decided it. Idempotent — the client
            // treats this as success.
            Ok(false) => Response::Error(RpcError::AlreadyDecided),
            Err(e) => {
                // Log the detail host-side; never forward raw backend error text
                // to the client (it can carry paths / internal shapes).
                tracing::warn!(error = %e, session = %req.session_id, "resolve_permission failed");
                Response::Error(RpcError::Internal("permission resolve failed".into()))
            }
        }
    }

    pub(super) fn events_since(&self, pubkey: &AppPubkey, session_id: &str, after_seq: u64) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let status = handle.status_snapshot();
        let wire = SessionStatusWire {
            last_seq: status.last_seq,
            awaiting_permission: status.awaiting_permission,
        };
        let mut frames = Vec::new();
        for (seq, event) in handle.events_since(after_seq) {
            match HostEvent::new(session_id, seq, &event, wire.clone()) {
                Ok(frame) => frames.push(frame),
                Err(_) => return Response::Error(RpcError::Internal("event encode failed".into())),
            }
        }
        Response::Events(frames)
    }

    /// Run a session command behind the per-RPC ACL/authz recheck. `pub(super)`
    /// so the dispatcher's router can use it for the trivial Steer/Cancel arms.
    pub(super) fn scoped<F>(&self, pubkey: &AppPubkey, session_id: &str, f: F) -> Response
    where
        F: FnOnce(&SessionHandle) -> anyhow::Result<()>,
    {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        match f(&handle) {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "session command failed");
                Response::Error(RpcError::Internal("session command failed".into()))
            }
        }
    }

    /// Working-tree status of the repository the session lives in.
    ///
    /// Scoped by session, so remote git access inherits the device's existing
    /// session ACL rather than introducing a second, wider authorization surface:
    /// a session-scoped device can only see the repo of a session it may reach.
    /// Async (unlike the other handlers) because opening the repo and running
    /// status both shell out to git — the serve loop awaits this directly.
    pub(super) async fn git_status(&self, pubkey: &AppPubkey, session_id: &str) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let Some(cwd) = handle.meta_snapshot().cwd else {
            return Response::Error(RpcError::BadRequest(
                "session has no working directory".into(),
            ));
        };
        // Git error text routinely embeds absolute paths ("fatal: not a git
        // repository: /Users/…"), so it is logged host-side and never forwarded —
        // the same rule the other handlers follow for backend errors.
        let repo = match oximux_git::repository::Repository::open(&cwd).await {
            Ok(repo) => repo,
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "open repository failed");
                return Response::Error(RpcError::Internal("git unavailable".into()));
            }
        };
        match repo.status().await {
            Ok(state) => Response::GitStatus(to_status_wire(state)),
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "git status failed");
                Response::Error(RpcError::Internal("git status failed".into()))
            }
        }
    }

    /// Diff one path in the session's repository.
    ///
    /// The path arrives from the client, so it is **contained against the repo
    /// workdir here, at the RPC boundary** — not left to git. Only the tracked
    /// paths shell out; `diff_for_untracked` reads the file directly, so nothing
    /// downstream would catch a traversal on that branch.
    pub(super) async fn git_diff(
        &self,
        pubkey: &AppPubkey,
        session_id: &str,
        path: &str,
        staged: bool,
        untracked: bool,
    ) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let Some(cwd) = handle.meta_snapshot().cwd else {
            return Response::Error(RpcError::BadRequest(
                "session has no working directory".into(),
            ));
        };
        let repo = match oximux_git::repository::Repository::open(&cwd).await {
            Ok(repo) => repo,
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "open repository failed");
                return Response::Error(RpcError::Internal("git unavailable".into()));
            }
        };

        // Containment gate. The rejection message deliberately says nothing about
        // what does or does not exist on disk — it must not become a probe.
        let contained =
            match oximux_git::path_guard::contained_path(repo.workdir(), std::path::Path::new(path))
            {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(session = %session_id, "rejected out-of-repository diff path");
                    return Response::Error(RpcError::BadRequest("path is outside the repository".into()));
                }
            };

        let diffs = if untracked {
            repo.diff_for_untracked(&contained).await
        } else {
            repo.diff_for_path(&contained, staged).await
        };
        // Paths go back out repository-relative. The untracked codepath echoes the
        // absolute path it was handed, and shipping that would disclose the host's
        // directory layout (home dir, usernames) to the client — and break the
        // contract that a listed path can be echoed straight back on a diff request.
        let root = repo
            .workdir()
            .canonicalize()
            .unwrap_or_else(|_| repo.workdir().to_path_buf());
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
