//! The authenticated session-RPC handlers — each re-checks the device's scope
//! (`is_allowed_for`) before touching the [`SessionRegistry`], so revocation and
//! per-device scoping bite on every call.

use oximux_agents::session_registry::SessionHandle;
use oximux_remote_proto::messages::{
    AnswerQuestionReq, ResolvePermissionReq, SendPromptReq, SessionInfoWire, SessionStatusWire,
    SessionSummary,
};
use oximux_remote_proto::proto::{Choice, Response, RpcError, SessionChoices};
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

    /// Start a new agent session on the desktop.
    ///
    /// **The only RPC that creates rather than drives**, so the authorization is
    /// worth stating plainly: it gates on `may_write` with **no session id**,
    /// because there is no session yet to scope against. That has a consequence
    /// the other write gates do not have — a device the desktop narrowed to a
    /// single session must not be able to create a second one and escape its own
    /// scope, so a session-scoped device is refused outright here.
    ///
    /// A host with no launcher configured answers `Unauthorized` rather than a
    /// distinct "not supported": whether this desktop can start sessions is not
    /// something an unauthorized client should be able to probe, matching how
    /// the terminal RPCs treat a missing `TerminalSource`.
    pub(super) async fn create_session(
        &self,
        pubkey: &AppPubkey,
        cwd: &str,
        agent_id: Option<&str>,
    ) -> Response {
        // Refuse a session-scoped device before anything else: `may_write` alone
        // would let one through, since it has no session to narrow against.
        if !self.auth.may_create_sessions(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(launcher) = self.launcher.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        match launcher.create(cwd, agent_id).await {
            Ok(session_id) => Response::SessionCreated { session_id },
            Err(e) => {
                // The launcher's own error text routinely embeds absolute host
                // paths (a missing directory, a binary off `PATH`), so it is
                // logged here and only the category crosses the wire.
                tracing::warn!(error = %e, "remote session launch failed");
                Response::Error(RpcError::BadRequest(e.to_string()))
            }
        }
    }

    /// Rewind a session to an earlier turn.
    ///
    /// Gated on `may_write` like every state-changing RPC — and unlike
    /// `create_session`, plain `may_write` is the right gate here: this names a
    /// session, so a session-scoped device is already narrowed to the
    /// conversations it may touch and needs no extra scope check.
    ///
    /// The client's `ordinal` is **not** trusted. It is validated against the
    /// host's own transcript inside the service, because the phone's fold can
    /// legitimately be behind — and truncating at the wrong point on a stale
    /// ordinal would silently destroy turns the user meant to keep.
    pub(super) async fn rewind_session(
        &self,
        pubkey: &AppPubkey,
        session_id: &str,
        ordinal: usize,
        include_files: bool,
    ) -> Response {
        if !self.auth.may_write(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(rewinder) = self.rewinder.as_ref() else {
            return Response::Error(RpcError::Unauthorized);
        };
        match rewinder.rewind(session_id, ordinal, include_files).await {
            // The truncation itself reaches the client as a `Rewound` event on
            // the session stream — the same path a desktop-initiated rewind
            // takes — so this only acknowledges that the rewind was accepted.
            Ok(()) => Response::Ack,
            Err(e) => {
                // `RewindError`'s messages are curated to carry no host paths,
                // unlike the underlying fork/checkpoint failures they stand for,
                // which name session files and git objects.
                tracing::warn!(error = %e, "remote rewind failed");
                Response::Error(RpcError::BadRequest(e.to_string()))
            }
        }
    }

    /// The model and permission-mode options this session's backend offers.
    ///
    /// A **read**, so it gates on `is_allowed_for` rather than `may_write`: a
    /// read-only device should see which model is running, it simply cannot
    /// change it.
    ///
    /// Empty lists are a legitimate answer, not an error — a dynamic-catalog
    /// backend advertises nothing until its handshake completes, and some agents
    /// offer no mode choices at all. The phone hides a picker with no options
    /// rather than showing an empty one.
    pub(super) fn list_choices(&self, pubkey: &AppPubkey, session_id: &str) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let meta = handle.meta_snapshot();
        Response::Choices(SessionChoices {
            models: handle
                .models()
                .into_iter()
                .map(|m| Choice { id: m.wire, label: m.label, description: m.description })
                .collect(),
            modes: handle
                .permission_modes()
                .into_iter()
                .map(|m| Choice { id: m.wire, label: m.label, description: None })
                .collect(),
            current_model: meta.model,
            current_mode: None,
        })
    }

    /// Switch the session's model or permission mode in place.
    ///
    /// Separate from [`Self::scoped`] purely for the error message. `scoped`
    /// answers any failure with a generic "session command failed", which is
    /// right when the cause is genuinely internal — but the overwhelmingly
    /// common failure here is a backend that fixes its model at spawn, and
    /// telling the user that is the difference between a control that looks
    /// broken and one that explains itself.
    ///
    /// The real error is logged host-side and a **fixed** string is returned.
    /// Forwarding the underlying text would repeat the leak the git handlers
    /// already had to fix, where raw tool output carried host paths to the
    /// client.
    pub(super) fn set_choice<F>(
        &self,
        pubkey: &AppPubkey,
        session_id: &str,
        what: &'static str,
        f: F,
    ) -> Response
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
                tracing::warn!(error = %e, session = %session_id, "remote {what} change refused");
                Response::Error(RpcError::BadRequest(format!(
                    "this agent cannot change {what} while it is running"
                )))
            }
        }
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
