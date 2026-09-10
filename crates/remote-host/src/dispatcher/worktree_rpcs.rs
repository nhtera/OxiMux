//! The worktree RPC handlers (v16): create, list, remove — each behind the
//! dedicated full-scope worktree gates, delegating the real work to the app's
//! [`WorktreeService`](crate::worktrees::WorktreeService).
//!
//! Authorization precedes capability on every arm: an under-scoped caller gets
//! `Unauthorized` whether or not the host has a service installed, so the
//! capability cannot be probed without the scope to use it. Only an authorized
//! caller on a service-less host sees `Unsupported`.

use oximux_core::WorkPhase;
use oximux_remote_proto::messages::CreateBaseWire;
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
        | WorktreeError::NoSuchLocalBranch
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
        base: &CreateBaseWire,
    ) -> Response {
        if !self.auth.may_manage_worktrees(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        // A second, narrower gate on top of the capability — and deliberately
        // ordered after it, so the base rule cannot be probed by a peer that
        // lacks the capability in the first place.
        //
        // Everything else this surface exposes is host-derived from a project
        // the host already knows and a slug it validates; the client never
        // names a location. A base ref is the exception — a string the host
        // resolves and checks out — and `oximux-worktree-ops` additionally
        // refuses to run an unreviewed ref's setup script. That guard covers
        // the local CLI. For a peer that is not sitting at the machine, the
        // stronger property is the one worth keeping: it cannot name a ref at
        // all. If a later phase wants remote base refs, it owes this boundary a
        // fresh decision rather than a quiet relaxation here.
        if !base.is_default() && !peer.is_local_operator() {
            tracing::warn!("remote peer asked for a worktree base ref; refusing");
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(service) = self.worktrees.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match service.create(project_path, slug, base).await {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ed25519_dalek::SigningKey;
    use oximux_remote_proto::messages::{RegisterReq, WorktreeProgressWire, WorktreeWire};

    use super::*;
    use crate::auth::{AuthStore, PairingSlot, registration_proof};
    use crate::dispatcher::Dispatcher;
    use oximux_agents::session_registry::SessionRegistry;
    use crate::worktrees::WorktreeService;

    /// Records what actually reached the service, so a refusal that never got
    /// there is distinguishable from one the service itself made.
    #[derive(Default)]
    struct SpyWorktrees {
        creates: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl WorktreeService for SpyWorktrees {
        async fn create(
            &self,
            project_path: &str,
            slug: &str,
            base: &CreateBaseWire,
        ) -> Result<WorktreeWire, WorktreeError> {
            self.creates.fetch_add(1, Ordering::SeqCst);
            let branch = match base {
                CreateBaseWire::Existing(name) => name.clone(),
                CreateBaseWire::Default | CreateBaseWire::From(_) => format!("oximux/{slug}"),
            };
            Ok(WorktreeWire {
                id: "wt-1".into(),
                project_path: project_path.into(),
                name: slug.into(),
                slug: slug.into(),
                branch,
                path: "/data/wt".into(),
            })
        }
        async fn list(&self, _: Option<&str>) -> Result<Vec<WorktreeWire>, WorktreeError> {
            Ok(vec![])
        }
        async fn remove(&self, _: &str) -> Result<(), WorktreeError> {
            Ok(())
        }
        async fn set_progress(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<(), WorktreeError> {
            Ok(())
        }
        async fn list_progress(
            &self,
            _: Option<&str>,
        ) -> Result<Vec<WorktreeProgressWire>, WorktreeError> {
            Ok(vec![])
        }
    }

    /// A dispatcher with a spy service, plus a **fully paired, write-capable**
    /// remote device.
    ///
    /// The pairing is not incidental. `may_manage_worktrees` runs first, so an
    /// unpaired key would be refused by the capability gate and the test would
    /// pass without the base gate existing at all. The seed is a real curve
    /// point for the same reason — an arbitrary 32-byte fill is not a valid
    /// verifying key, and the resulting `Unauthorized` looks exactly like a
    /// policy refusal.
    fn dispatcher_with_a_paired_device() -> (Dispatcher, Arc<SpyWorktrees>, Peer) {
        let store = AuthStore::new();
        let secret = [0x22; 16];
        store.set_pairing(PairingSlot::new(secret, None, false));
        let pubkey = SigningKey::from_bytes(&[0x33; 32]).verifying_key().to_bytes();
        let ts = 1_700_000_000;
        store
            .register(
                &RegisterReq {
                    app_pubkey: pubkey,
                    device_name: "phone".into(),
                    proof: registration_proof(&secret, &pubkey, ts),
                    timestamp_secs: ts,
                    session_id: None,
                },
                ts,
            )
            .expect("register");
        let spy = Arc::new(SpyWorktrees::default());
        let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::new(store))
            .with_worktrees(spy.clone());
        (dispatcher, spy, Peer::remote(pubkey))
    }

    /// The premise the whole gate rests on: this device CAN create worktrees.
    /// Without this the refusals below would prove nothing.
    #[test]
    fn a_paired_device_may_still_create_an_ordinary_worktree() {
        let (dispatcher, spy, peer) = dispatcher_with_a_paired_device();
        let response = futures::executor::block_on(dispatcher.create_worktree(
            &peer,
            "/work",
            "feat",
            &CreateBaseWire::Default,
        ));
        assert!(matches!(response, Response::WorktreeCreated(_)), "{response:?}");
        assert_eq!(spy.creates.load(Ordering::SeqCst), 1);
    }

    /// A base ref is a string the host resolves and checks out. A peer that is
    /// not sitting at the machine may not name one — and the refusal must land
    /// BEFORE the service, or the narrowing is decoration.
    #[test]
    fn a_remote_peer_may_not_name_a_base_ref() {
        for base in [
            CreateBaseWire::From("origin/pr-4711".into()),
            CreateBaseWire::Existing("side".into()),
        ] {
            let (dispatcher, spy, peer) = dispatcher_with_a_paired_device();
            let response = futures::executor::block_on(
                dispatcher.create_worktree(&peer, "/work", "feat", &base),
            );
            assert_eq!(
                response,
                Response::Error(RpcError::Unauthorized),
                "a remote peer named {base:?} and was not refused"
            );
            assert_eq!(
                spy.creates.load(Ordering::SeqCst),
                0,
                "the refusal must be upstream of the service"
            );
        }
    }

    /// A client-fixable mistake must reach the client as one.
    ///
    /// `--branch origin/main` used to collapse into `CreateFailed`, i.e.
    /// `Internal("the worktree could not be created")` — a dead end for a user
    /// whose only error was naming a remote-tracking branch. The refusal has to
    /// carry the rule and the alternative, or the CLI cannot say anything
    /// useful about it.
    #[test]
    fn adopting_a_name_that_is_not_a_local_branch_is_a_bad_request() {
        let response = worktree_failure(WorktreeError::NoSuchLocalBranch);
        match response {
            Response::Error(RpcError::BadRequest(msg)) => {
                assert!(msg.contains("--from"), "the refusal must name the way out: {msg}");
                assert!(msg.contains("local branch"), "and the rule: {msg}");
            }
            other => panic!("must be client-fixable, not Internal: {other:?}"),
        }
    }

    /// The same request from the machine's own socket is served — the gate is
    /// about *who is asking*, not about the feature being off.
    #[test]
    fn a_local_operator_may_name_a_base_ref() {
        let (dispatcher, spy, _) = dispatcher_with_a_paired_device();
        let peer = Peer::local(crate::auth::LocalScope::Full);
        let response = futures::executor::block_on(dispatcher.create_worktree(
            &peer,
            "/work",
            "feat",
            &CreateBaseWire::Existing("side".into()),
        ));
        match response {
            Response::WorktreeCreated(row) => assert_eq!(row.branch, "side"),
            other => panic!("expected WorktreeCreated, got {other:?}"),
        }
        assert_eq!(spy.creates.load(Ordering::SeqCst), 1);
    }
}
