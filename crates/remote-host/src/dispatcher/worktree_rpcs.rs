//! The worktree RPC handlers (v16): create, list, remove — each behind the
//! dedicated full-scope worktree gates, delegating the real work to the app's
//! [`WorktreeService`](crate::worktrees::WorktreeService).
//!
//! Authorization precedes capability on every arm: an under-scoped caller gets
//! `Unauthorized` whether or not the host has a service installed, so the
//! capability cannot be probed without the scope to use it. Only an authorized
//! caller on a service-less host sees `Unsupported`.

use oximux_core::WorkPhase;
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;
use crate::worktrees::WorktreeError;

/// Map a service failure onto the wire. The service's messages are curated
/// (see [`WorktreeError`]) — no host path ever crosses here.
fn worktree_failure(err: WorktreeError) -> Response {
    match err {
        // The client can fix these by asking differently.
        WorktreeError::UnknownProject
        | WorktreeError::BadSlug
        | WorktreeError::AlreadyExists
        | WorktreeError::UnknownWorktree => Response::Error(RpcError::BadRequest(err.to_string())),
        // The host failed; detail was logged by the service.
        WorktreeError::CreateFailed
        | WorktreeError::RemoveFailed
        | WorktreeError::Unavailable => Response::Error(RpcError::Internal(err.to_string())),
    }
}

impl Dispatcher {
    /// Create a worktree under a known project. Write-gated on the dedicated
    /// full-scope check — never `may_write`, which is session-scoped and would
    /// hold only by accident for an RPC that names no session.
    pub(super) async fn create_worktree(
        &self,
        peer: &Peer,
        project_path: &str,
        slug: &str,
    ) -> Response {
        if !self.auth.may_manage_worktrees(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.create(project_path, slug).await {
            Ok(row) => Response::WorktreeCreated(row),
            Err(err) => worktree_failure(err),
        }
    }

    /// List worktrees. A read, so the read-only tier is admitted — but still
    /// full-scope: rows carry host paths across every project.
    pub(super) async fn list_worktrees(
        &self,
        peer: &Peer,
        project_path: Option<&str>,
    ) -> Response {
        if !self.auth.may_read_worktrees(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.list(project_path).await {
            Ok(rows) => Response::Worktrees(rows),
            Err(err) => worktree_failure(err),
        }
    }

    /// Remove a worktree by the id a listing carried. Destructive, so it
    /// shares the create gate.
    pub(super) async fn remove_worktree(&self, peer: &Peer, id: &str) -> Response {
        if !self.auth.may_manage_worktrees(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.remove(id).await {
            Ok(()) => Response::Ack,
            Err(err) => worktree_failure(err),
        }
    }

    /// Set a worktree's progress line and/or work phase.
    ///
    /// **Gated as coordination state, not worktree management.** The gates the
    /// three RPCs above use are full-scope because they write the filesystem
    /// and the repository; this writes neither. Its primary caller is a
    /// session-confined agent describing its own work — exactly the caller
    /// full scope excludes — so it shares the coordination blackboard's reach,
    /// which exists for the same reason: the payload is agent-authored text
    /// carrying no host path, no branch name, and no session content.
    ///
    /// The phase vocabulary is closed **here**, at the write edge, and nowhere
    /// on the read path. That asymmetry is deliberate: a typo must not become
    /// a stored phase no reader understands, while a phase a newer peer knows
    /// must still survive being read and rewritten by this build.
    pub(super) async fn set_worktree_progress(
        &self,
        peer: &Peer,
        id: &str,
        comment: Option<&str>,
        phase: Option<&str>,
    ) -> Response {
        if !self.auth.may_write_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // Validate before reaching for the service, so a bad phase reads the
        // same whether or not this host manages worktrees at all.
        if let Some(raw) = phase
            && !raw.is_empty()
            && WorkPhase::parse(raw).is_none()
        {
            let known: Vec<&str> = WorkPhase::ALL.iter().map(|p| p.as_str()).collect();
            return Response::Error(RpcError::BadRequest(format!(
                "unknown phase `{raw}` — expected one of: {}",
                known.join(", ")
            )));
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.set_progress(id, comment, phase).await {
            Ok(()) => Response::Ack,
            Err(err) => worktree_failure(err),
        }
    }

    /// The progress rows for a project's worktrees, or for every project.
    ///
    /// Read-gated as coordination state rather than as a worktree listing:
    /// these rows carry only an id and agent-authored text, never the host
    /// paths and branch names that make [`Self::list_worktrees`] full-scope.
    pub(super) async fn list_worktree_progress(
        &self,
        peer: &Peer,
        project_path: Option<&str>,
    ) -> Response {
        if !self.auth.may_read_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.list_progress(project_path).await {
            Ok(rows) => Response::WorktreeProgress(rows),
            Err(err) => worktree_failure(err),
        }
    }
}
