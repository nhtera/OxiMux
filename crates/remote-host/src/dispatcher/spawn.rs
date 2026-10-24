//! Agent delegation (v36): spawn, list, cancel, and "who am I".
//!
//! Every child an agent (or the operator) starts goes through
//! [`Dispatcher::spawn_agent`], which goes through
//! [`LineageStore::reserve`](oximux_agents::lineage::LineageStore::reserve): the
//! one place a lineage row can be created, with the caps checked atomically
//! before anything launches. The order is fixed — authorize, check the spawn is
//! allowed at all, reserve, then create resources and launch — so a refused
//! spawn has created nothing, and a failed launch releases what it created
//! (its worktree, its reservation) and counts against the parent's breaker.
//!
//! Lineage verbs answer from the spawn table. A parked or ended child is
//! listed or ended without opening it: routing a dormant child through the
//! session RPCs would auto-open (and so respawn) it.

use oximux_agents::lineage::{
    LineageRow, LineageState, NotifyParent, Reservation, SpawnOrigin, SpawnParent, SpawnRefusal,
    SpawnSpec,
};
use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_remote_proto::lineage::{
    LineageStateWire, LineageWire, NotifyParentWire, SpawnAgentReq, SpawnAgentResult,
    SpawnCwdWire, SpawnErrorKind, SpawnErrorWire, SpawnParentWire, SpawnPostureWire, WhoamiWire,
};
use oximux_remote_proto::messages::CreateBaseWire;
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;
use crate::launcher::{ChildCtx, LaunchError};

fn refuse(kind: SpawnErrorKind, message: impl Into<String>) -> Response {
    Response::Error(RpcError::Spawn(SpawnErrorWire { kind, message: message.into() }))
}

impl Dispatcher {
    pub(super) async fn spawn_agent(&self, peer: &Peer, req: SpawnAgentReq) -> Response {
        let (Some(core), Some(launcher)) = (self.host_core.clone(), self.launcher.clone()) else {
            return Response::Error(RpcError::Unsupported);
        };
        let parent = match (&req.parent, peer.own_session()) {
            (SpawnParentWire::Caller, Some(own)) => own.to_string(),
            (SpawnParentWire::Caller, None) => {
                return Response::Error(RpcError::BadRequest(
                    "name the parent session: only an agent's own credential spawns under itself".into(),
                ));
            }
            (SpawnParentWire::Session(id), _) => id.clone(),
        };
        if !self.auth.may_spawn_child(peer, &parent) {
            return Response::Error(RpcError::Unauthorized);
        }
        // Derived from the caller, never taken from the request.
        let origin = if peer.own_session().is_some() { SpawnOrigin::Agent } else { SpawnOrigin::User };
        let posture = posture_from_wire(req.posture);

        if origin == SpawnOrigin::Agent {
            let allowance = core.spawn_allowance(&parent);
            if !allowance.allowed {
                let kind = if allowance.depth >= allowance.max_depth {
                    SpawnErrorKind::DepthExceeded
                } else {
                    SpawnErrorKind::NotAllowed
                };
                return refuse(kind, allowance.reason.unwrap_or_default());
            }
            // The ceiling: an agent never hands a child more than it has.
            let own = core.lineage().row(&parent).ok().flatten().map_or(SpawnPosture::Default, |r| r.posture);
            if !own.permits_child(posture) {
                return refuse(
                    SpawnErrorKind::PostureUnenforceable,
                    "An agent cannot give a child more than it has itself. Spawn it with a narrower \
                     posture, or complete this task yourself.",
                );
            }
        } else if !core.is_owner() {
            return refuse(
                SpawnErrorKind::NotAllowed,
                "This app does not host the data directory (local access is off, or another host owns it).",
            );
        }

        let roster = launcher.roster();
        if !roster.is_empty() && !roster.iter().any(|a| a.agent_id == req.agent_id && a.installed) {
            let installed: Vec<&str> =
                roster.iter().filter(|a| a.installed).map(|a| a.agent_id.as_str()).collect();
            return refuse(
                SpawnErrorKind::UnknownAgent,
                format!(
                    "No installed agent `{}`. Installed agents: {}.",
                    req.agent_id,
                    installed.join(", ")
                ),
            );
        }

        let parent_cwd = self
            .registry
            .get(&parent)
            .and_then(|h| h.meta_snapshot().cwd)
            .map(|p| p.to_string_lossy().into_owned());
        let Some(parent_cwd) = parent_cwd else {
            return Response::Error(RpcError::UnknownSession);
        };
        // A plain path is checked now (cheap, creates nothing); a worktree is
        // created only once the slot is reserved.
        let cwd_path = match &req.cwd {
            SpawnCwdWire::Path(path) => {
                match oximux_git::path_guard::contained_path(parent_cwd.as_ref(), path.as_ref()) {
                    Ok(p) => Some(p.to_string_lossy().into_owned()),
                    Err(_) => {
                        return refuse(
                            SpawnErrorKind::OutsideRepo,
                            "That working directory is outside the parent's. Use a path inside it, or Inherit.",
                        );
                    }
                }
            }
            SpawnCwdWire::Inherit => Some(parent_cwd.clone()),
            SpawnCwdWire::NewWorktree { .. } => None,
        };

        let spec = SpawnSpec {
            request_id: req.request_id.clone(),
            origin,
            posture,
            notify_parent: notify_from_wire(req.notify_parent),
            allow_parent_permits: req.allow_parent_permits,
            preassigned_id: None,
            fingerprint_input: format!(
                "{}\u{0}{:?}\u{0}{}\u{0}{:?}\u{0}{:?}",
                req.agent_id, req.model, req.prompt, req.cwd, req.output_schema
            ),
        };
        let reserved = core.lineage().reserve(
            &SpawnParent::Session(parent.clone()),
            &spec,
            core.config().caps,
            &core.working(),
            self.now_local(),
        );
        let (row_id, depth, root_id) = match reserved {
            Err(err) => {
                tracing::warn!(%err, "lineage reservation failed");
                return Response::Error(RpcError::Internal("could not reserve the spawn".into()));
            }
            Ok(Err(refusal)) => return refuse(refusal_kind(&refusal), refusal.to_string()),
            Ok(Ok(Reservation::Replayed(row))) => {
                return Response::AgentSpawned(SpawnAgentResult {
                    session_id: row.session_id.unwrap_or_default(),
                    parent_id: row.parent_id,
                    root_id: row.root_id,
                    depth: row.depth,
                    replayed: true,
                });
            }
            Ok(Ok(Reservation::Fresh { row_id, depth, root_id })) => (row_id, depth, root_id),
        };

        // Resources the attempt creates, released if the launch fails.
        let mut worktree_id = None;
        let cwd = match cwd_path {
            Some(cwd) => cwd,
            None => {
                let name = match &req.cwd {
                    SpawnCwdWire::NewWorktree { name } => name.as_deref(),
                    _ => None,
                };
                match self.worktree_for_child(&parent_cwd, name, &req.request_id).await {
                    Ok((id, path)) => {
                        worktree_id = Some(id);
                        path
                    }
                    Err(detail) => {
                        self.rollback_spawn(row_id, &detail, None).await;
                        return Response::Error(RpcError::BadRequest(detail));
                    }
                }
            }
        };

        let ctx = ChildCtx { parent_session_id: Some(parent.clone()), activate: false, posture };
        match launcher.create_child(&cwd, Some(&req.agent_id), req.model.as_deref(), ctx).await {
            Ok(session_id) => {
                if let Err(err) = core.lineage().finalize(row_id, &session_id, self.now_local()) {
                    // The reservation was reclaimed under a launch slower than
                    // the timeout: a replay may own the slot now. Stop this
                    // child rather than leave one the tree does not know.
                    tracing::warn!(%err, session_id, "spawned child could not be finalized; stopping it");
                    if let Some(h) = self.registry.get(&session_id) {
                        let _ = h.cancel();
                    }
                    return Response::Error(RpcError::Internal("the spawn could not be recorded".into()));
                }
                let delivered = self
                    .registry
                    .get(&session_id)
                    .is_some_and(|h| h.send_prompt(&req.prompt, &[]).is_ok());
                if !delivered {
                    tracing::warn!(session_id, "spawned child opened but its prompt was not delivered");
                }
                Response::AgentSpawned(SpawnAgentResult {
                    session_id,
                    parent_id: parent,
                    root_id,
                    depth,
                    replayed: false,
                })
            }
            Err(err) => {
                tracing::warn!(error = %err, "child launch failed");
                self.rollback_spawn(row_id, launch_reason(&err), worktree_id.as_deref()).await;
                match err {
                    LaunchError::PostureUnenforceable(why) => refuse(
                        SpawnErrorKind::PostureUnenforceable,
                        format!("{why}. Spawn a Claude or Codex agent for this posture."),
                    ),
                    other => Response::Error(RpcError::BadRequest(other.to_string())),
                }
            }
        }
    }

    /// Undo a reservation whose launch did not happen, releasing what the
    /// attempt created, and count it against the parent's breaker.
    async fn rollback_spawn(&self, row_id: i64, reason: &str, worktree_id: Option<&str>) {
        if let Some(core) = &self.host_core
            && let Err(err) = core.lineage().rollback(row_id, reason, self.now_local())
        {
            tracing::warn!(%err, "lineage rollback failed");
        }
        if let (Some(id), Some(worktrees)) = (worktree_id, self.worktrees.as_ref())
            && worktrees.remove(id).await.is_err()
        {
            tracing::warn!(id, "a failed spawn's worktree could not be removed");
        }
    }

    /// A fresh worktree for a child, named by the caller or by its request id.
    /// Returns `(id, path)`.
    async fn worktree_for_child(
        &self,
        parent_cwd: &str,
        name: Option<&str>,
        request_id: &str,
    ) -> Result<(String, String), String> {
        let Some(worktrees) = self.worktrees.as_ref() else {
            return Err("this host cannot create worktrees".into());
        };
        let slug = super::team::slugify(name.unwrap_or(request_id));
        if slug.is_empty() {
            return Err("that worktree name has nothing a branch can be named after".into());
        }
        worktrees
            .create(parent_cwd, &slug, &CreateBaseWire::Default)
            .await
            .map(|row| (row.id, row.path))
            .map_err(|_| "the child's worktree could not be created".to_string())
    }

    pub(super) fn list_children(&self, peer: &Peer, of: &str, recursive: bool) -> Response {
        let Some(core) = self.host_core.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        if !self.may_reach(peer, of) {
            return refuse(SpawnErrorKind::NotInSubtree, "That session is not in your subtree.");
        }
        match core.lineage().children(of, recursive) {
            Ok(rows) => Response::Children(rows.into_iter().filter_map(lineage_to_wire).collect()),
            Err(err) => {
                tracing::warn!(%err, "listing children failed");
                Response::Error(RpcError::Internal("could not read the spawn tree".into()))
            }
        }
    }

    pub(super) fn cancel_agent(&self, peer: &Peer, id: &str, cascade: bool) -> Response {
        let Some(core) = self.host_core.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        let is_run = id.starts_with("team:") || id.starts_with("plan:");
        let allowed = if is_run {
            self.auth.may_manage_teams(peer)
        } else {
            self.may_reach(peer, id)
                && (peer.own_session().is_some() || self.auth.may_write(peer, id) || peer.is_local_operator())
        };
        if !allowed {
            return refuse(SpawnErrorKind::NotInSubtree, "That session is not in your subtree.");
        }
        // A run is always cancelled as a whole.
        match core.lineage().mark_ended(id, cascade || is_run, self.now_local()) {
            Ok(ended) => {
                // Stop any turn still running; a dormant child has nothing to
                // stop and is not opened.
                for session in &ended {
                    if let Some(h) = self.registry.get(session) {
                        let _ = h.cancel();
                    }
                }
                Response::AgentsCancelled { ended }
            }
            Err(err) => {
                tracing::warn!(%err, "cancelling children failed");
                Response::Error(RpcError::Internal("could not update the spawn tree".into()))
            }
        }
    }

    pub(super) fn whoami(&self, peer: &Peer) -> Response {
        let scope = if peer.is_local_operator() {
            "full"
        } else if peer.is_mcp() {
            "mcp"
        } else if peer.own_session().is_some() {
            "session"
        } else {
            "device"
        };
        let session = peer.own_session().map(str::to_string);
        let (depth, root_id, spawn_allowed, reason) = match (self.host_core.as_ref(), session.as_deref()) {
            (Some(core), Some(own)) => {
                let row = core.lineage().row(own).ok().flatten();
                let allowance = core.spawn_allowance(own);
                (
                    allowance.depth,
                    Some(row.map_or_else(|| own.to_string(), |r| r.root_id)),
                    allowance.allowed,
                    allowance.reason,
                )
            }
            (Some(core), None) => {
                let ok = core.is_owner() && core.config().delegation_enabled && self.auth.may_create_sessions(peer);
                (0, None, ok, (!ok).then(|| "Spawning needs the operator and the host role.".to_string()))
            }
            (None, _) => (0, None, false, Some("This host does not support agent delegation.".to_string())),
        };
        let agents = self.launcher.as_ref().map(|l| l.roster()).unwrap_or_default();
        Response::Whoami(WhoamiWire { scope: scope.into(), session, depth, root_id, spawn_allowed, reason, agents })
    }

    /// Whether `peer` reaches `target` through its own scope or its subtree.
    fn may_reach(&self, peer: &Peer, target: &str) -> bool {
        let Some(core) = self.host_core.as_ref() else { return false };
        self.auth.may_reach_descendant(peer, target, |ancestor, t| {
            core.lineage().is_descendant(ancestor, t).unwrap_or(false)
        })
    }
}

fn refusal_kind(refusal: &SpawnRefusal) -> SpawnErrorKind {
    match refusal {
        SpawnRefusal::Depth { .. } => SpawnErrorKind::DepthExceeded,
        SpawnRefusal::Fanout { .. } => SpawnErrorKind::FanoutExceeded,
        SpawnRefusal::Open { .. } => SpawnErrorKind::OpenExceeded,
        SpawnRefusal::Budget { .. } => SpawnErrorKind::BudgetExceeded,
        SpawnRefusal::StartFailing { .. } => SpawnErrorKind::StartFailing,
        SpawnRefusal::IdempotencyConflict => SpawnErrorKind::IdempotencyConflict,
        SpawnRefusal::InProgress => SpawnErrorKind::InProgress,
    }
}

/// Short, path-free reason recorded on the parent's failed-start counter.
fn launch_reason(err: &LaunchError) -> &'static str {
    match err {
        LaunchError::Unavailable => "the host could not start a session",
        LaunchError::BadWorkingDirectory => "unusable working directory",
        LaunchError::Failed => "the agent did not start",
        LaunchError::ModelUnsupported => "model not settable at spawn",
        LaunchError::PostureUnenforceable(_) => "posture unenforceable",
    }
}

fn posture_from_wire(p: SpawnPostureWire) -> SpawnPosture {
    match p {
        SpawnPostureWire::Default => SpawnPosture::Default,
        SpawnPostureWire::Review => SpawnPosture::Review,
        SpawnPostureWire::Verify => SpawnPosture::Verify,
    }
}

fn posture_to_wire(p: SpawnPosture) -> SpawnPostureWire {
    match p {
        SpawnPosture::Default => SpawnPostureWire::Default,
        SpawnPosture::Review => SpawnPostureWire::Review,
        SpawnPosture::Verify => SpawnPostureWire::Verify,
    }
}

fn notify_from_wire(n: NotifyParentWire) -> NotifyParent {
    match n {
        NotifyParentWire::Auto => NotifyParent::Auto,
        NotifyParentWire::Stuck => NotifyParent::Stuck,
        NotifyParentWire::Off => NotifyParent::Off,
    }
}

fn notify_to_wire(n: NotifyParent) -> NotifyParentWire {
    match n {
        NotifyParent::Auto => NotifyParentWire::Auto,
        NotifyParent::Stuck => NotifyParentWire::Stuck,
        NotifyParent::Off => NotifyParentWire::Off,
    }
}

/// A row as the wire carries it. Reservations (no session yet) are not listed.
fn lineage_to_wire(row: LineageRow) -> Option<LineageWire> {
    let state = match row.state {
        LineageState::Reserved => return None,
        LineageState::Active => LineageStateWire::Active,
        LineageState::Parked => LineageStateWire::Parked,
        LineageState::Ended => LineageStateWire::Ended,
    };
    Some(LineageWire {
        session_id: row.session_id?,
        parent_id: row.parent_id,
        root_id: row.root_id,
        depth: row.depth,
        posture: posture_to_wire(row.posture),
        state,
        notify_parent: notify_to_wire(row.notify_parent),
        identity_slot: row.identity_slot,
        created_at: row.created_at.to_rfc3339(),
    })
}
