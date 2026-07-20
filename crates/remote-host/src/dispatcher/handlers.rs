//! The authenticated session-RPC handlers — each re-checks the device's scope
//! (`is_allowed_for`) before touching the [`SessionRegistry`], so revocation and
//! per-device scoping bite on every call.

use oximux_agents::session_registry::SessionHandle;
use oximux_remote_proto::messages::{
    AnswerQuestionReq, ResolvePermissionReq, SendPromptReq, SessionInfoWire, SessionStatusWire,
    SessionSummary,
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
        // Deciding a permission lets an agent act, so it is a write.
        if !self.auth.may_write(pubkey, &req.session_id) {
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

    pub(super) fn answer_question(&self, pubkey: &AppPubkey, req: AnswerQuestionReq) -> Response {
        // Answering releases a blocked turn, so it is a write.
        if !self.auth.may_write(pubkey, &req.session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(&req.session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        match handle.answer_question(&req.request_id, &req.questions, &req.answers) {
            Ok(true) => Response::Ack,
            Ok(false) => Response::Error(RpcError::AlreadyDecided),
            Err(e) => {
                // Backend error text can carry paths and internal shapes — log it
                // here, hand the client only the category.
                tracing::warn!(error = %e, session = %req.session_id, "answer_question failed");
                Response::Error(RpcError::Internal("question answer failed".into()))
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

    /// Run a session **command** behind the per-RPC ACL/authz recheck. `pub(super)`
    /// so the dispatcher's router can use it for the trivial Steer/Cancel arms.
    ///
    /// Every caller of this is state-changing (prompt/steer/cancel), so the gate is
    /// `may_write` — a read-only device is refused here even though it may read the
    /// same session.
    pub(super) fn scoped<F>(&self, pubkey: &AppPubkey, session_id: &str, f: F) -> Response
    where
        F: FnOnce(&SessionHandle) -> anyhow::Result<()>,
    {
        if !self.auth.may_write(pubkey, session_id) {
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

}
