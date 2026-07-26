//! The authenticated session-RPC handlers — each re-checks the device's scope
//! (`is_allowed_for`) before touching the [`SessionRegistry`], so revocation and
//! per-device scoping bite on every call.

use oximux_agents::session_registry::{ChoiceKind, SessionHandle};
use oximux_remote_proto::messages::{
    AnswerQuestionReq, ResolvePermissionReq, SendPromptReq, SessionInfoWire, SessionStatusWire,
    SessionSummary, SessionTranscriptWire,
};
use oximux_remote_proto::proto::{Choice, Response, RpcError, SessionChoices};
use oximux_remote_proto::HostEvent;

use super::Dispatcher;
use crate::auth::AppPubkey;

/// The project label a phone session row shows for attribution — the final path
/// component of the session's working directory (its workspace root). `None` when
/// no cwd has been published yet (a freshly-registered session), so the row simply
/// omits the context line rather than showing a placeholder.
fn project_label(cwd: Option<&std::path::Path>) -> Option<String> {
    cwd.and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// Compose the phone-facing session title. The wire summary carries a single
/// `title`, so the project is folded into it as `"<project> · <title>"`; the phone
/// splits on the separator to render the project as a muted context label. This
/// keeps a remote session row attributable to its project without adding a
/// `SessionSummary` field — which the append-only wire forbids. Falls back to the
/// bare title (or the id, so a row is never blank) when no cwd is known, which is
/// also what an older host sends, so the phone degrades to the un-prefixed row.
fn remote_title(title: Option<String>, cwd: Option<&std::path::Path>, session_id: &str) -> String {
    let base = title.unwrap_or_else(|| session_id.to_string());
    match project_label(cwd) {
        Some(project) => format!("{project} · {base}"),
        None => base,
    }
}

impl Dispatcher {
    /// The per-device-filtered session list — the shared body behind both the
    /// [`Request::ListSessions`](oximux_remote_proto::proto::Request::ListSessions)
    /// reply and every pushed
    /// [`SessionsChanged`](oximux_remote_proto::proto::Response::SessionsChanged)
    /// snapshot, so both honour the same scope filter and meta lookup.
    pub(super) fn snapshot_sessions(&self, pubkey: &AppPubkey) -> Vec<SessionSummary> {
        let live = self.live_session_rows(pubkey);
        let Some(catalog) = &self.catalog else {
            return live;
        };
        // One row per session, whatever the catalog hands over. Two guards in one
        // pass, because both failures look identical to a user — the same
        // conversation listed twice:
        //
        // - The registry is authoritative for anything it holds, so a dormant row
        //   for a live session would contradict the live one's status.
        // - A session can appear in more than one project's saved layout (a tab
        //   moved between projects leaves the old entry behind), so the catalog
        //   can legitimately return the same id twice.
        let mut seen: std::collections::HashSet<String> =
            live.iter().map(|r| r.session_id.clone()).collect();
        let mut rows = Vec::with_capacity(live.len());
        let dormant: Vec<SessionSummary> = catalog
            .dormant()
            .into_iter()
            .filter(|d| seen.insert(d.session_id.clone()))
            // Same scoping as a live row: a session-scoped device must not learn
            // that other sessions exist, whether or not they are running.
            .filter(|d| self.auth.is_allowed_for(pubkey, &d.session_id))
            .map(|d| SessionSummary {
                title: remote_title(d.title, d.cwd.as_deref(), &d.session_id),
                model: d.model,
                // A dormant session has no event stream, so there is no cursor to
                // resume from and nothing can be outstanding. A client opening it
                // starts at 0, which is what it would get anyway.
                last_seq: 0,
                awaiting_permission: false,
                session_id: d.session_id,
            })
            .collect();
        rows.extend(live);
        rows.extend(dormant);
        rows
    }

    /// The session rows backed by a live view, which own their status.
    fn live_session_rows(&self, pubkey: &AppPubkey) -> Vec<SessionSummary> {
        self.registry
            .statuses()
            .into_iter()
            // A session-scoped device must only learn about sessions it may act
            // on — never enumerate the full session set.
            .filter(|(session_id, _)| self.auth.is_allowed_for(pubkey, session_id))
            .map(|(session_id, status)| {
                // Title/model/cwd are published by the desktop view via the
                // registry's session meta. The title is project-qualified for the
                // remote row; a session not yet titled falls back to its id (never
                // blank), and one with no published cwd gets no project prefix.
                let meta = self
                    .registry
                    .get(&session_id)
                    .map(|handle| handle.meta_snapshot())
                    .unwrap_or_default();
                SessionSummary {
                    title: remote_title(meta.title, meta.cwd.as_deref(), &session_id),
                    model: meta.model,
                    last_seq: status.last_seq,
                    awaiting_permission: status.awaiting_permission,
                    session_id,
                }
            })
            .collect()
    }

    pub(super) fn list_sessions(&self, pubkey: &AppPubkey) -> Response {
        Response::Sessions(self.snapshot_sessions(pubkey))
    }

    /// Serve a session's authoritative folded-transcript snapshot so a client opens
    /// it with full history — including a transcript restored from disk after a host
    /// restart, which lives only in the view's fold and never entered the event ring.
    /// An empty base (`"[]"`, seq 0) is returned when the view has not published yet,
    /// so the client opens cleanly and the live stream fills it rather than erroring.
    pub(super) fn fetch_transcript(&self, pubkey: &AppPubkey, session_id: &str) -> Response {
        if !self.auth.is_allowed_for(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let snap = handle.transcript_snapshot().unwrap_or_default();
        let entries_json = if snap.entries_json.is_empty() {
            "[]".to_string()
        } else {
            snap.entries_json
        };
        // Attachments accumulate for the life of a session, so this reply is the
        // one whose size no other check bounds — the send-side guard passes each
        // prompt on its own, and their sum lands here. Over the frame cap the
        // client cannot assemble the reply at all, losing the whole history rather
        // than the images that overflowed.
        let (entries_json, stripped) =
            crate::transcript_budget::fit_images(&entries_json, crate::transcript_budget::IMAGE_BUDGET);
        if stripped > 0 {
            tracing::debug!(
                session_id,
                stripped,
                "dropped image data from older transcript entries to fit one frame"
            );
        }
        Response::SessionTranscript(SessionTranscriptWire {
            session_id: session_id.to_string(),
            seq: snap.seq,
            entries_json,
            model: snap.model,
        })
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
                title: remote_title(meta.title, meta.cwd.as_deref(), session_id),
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

    /// List the desktop's projects as new-session targets.
    ///
    /// Gated exactly like [`create_session`](Self::create_session) — a client that
    /// may not create a session has no use for the paths and must not be handed
    /// them. A session-scoped device is refused outright; a host with no project
    /// provider answers an empty list, since "this desktop exposes no projects" is
    /// not a capability worth hiding (the create path stays gated regardless).
    pub(super) async fn list_projects(&self, pubkey: &AppPubkey) -> Response {
        if !self.auth.may_create_sessions(pubkey) {
            return Response::Error(RpcError::Unauthorized);
        }
        match self.projects.as_ref() {
            Some(provider) => Response::Projects(provider.projects().await),
            None => Response::Projects(Vec::new()),
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
            // From the desktop view's published metadata, like the model: the
            // connection knows which modes exist but not which one this session
            // is sitting in — the view owns that, because it is the thing that
            // spawned the child with it.
            current_mode: meta.permission_mode,
        })
    }

    /// Switch the session's model or permission mode.
    ///
    /// Async because the handle's setter is: a backend that fixes the value at
    /// spawn (Claude, Codex) cannot switch in place, and the change is completed
    /// by the desktop respawning the child resumed on the new pick — a round trip
    /// to the UI thread, like launching a session.
    ///
    /// Separate from [`Self::scoped`] purely for the error message. `scoped`
    /// answers any failure with a generic "session command failed", which is
    /// right when the cause is genuinely internal — but a picker the user just
    /// tapped is owed something better than that.
    ///
    /// The real error is logged host-side and a **fixed** string is returned.
    /// Forwarding the underlying text would repeat the leak the git handlers
    /// already had to fix, where raw tool output carried host paths to the
    /// client.
    pub(super) async fn set_choice(
        &self,
        pubkey: &AppPubkey,
        session_id: &str,
        kind: ChoiceKind,
        value: &str,
    ) -> Response {
        if !self.auth.may_write(pubkey, session_id) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Response::Error(RpcError::UnknownSession);
        };
        let what = kind.noun();
        let outcome = match kind {
            ChoiceKind::Model => handle.set_model(value).await,
            ChoiceKind::PermissionMode => handle.set_permission_mode(value).await,
        };
        match outcome {
            Ok(()) => Response::Ack,
            Err(e) => {
                tracing::warn!(error = %e, session = %session_id, "remote {what} change refused");
                // Says what is true of every remaining failure here: the backend
                // would not switch in place and no desktop view completed it. The
                // old text blamed a running turn, which is usually not the cause
                // and is never something the user can act on.
                Response::Error(RpcError::BadRequest(format!(
                    "the desktop could not change this session's {what}"
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
