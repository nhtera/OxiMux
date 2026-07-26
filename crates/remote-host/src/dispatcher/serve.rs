//! The per-connection serve loop: multiplex incoming client requests against the
//! live events of any active `Subscribe`, so a live event is pushed the moment it
//! is produced without waiting on the next request.

use std::collections::HashMap;

use futures::future::{Either, select};
use futures::stream::{BoxStream, SelectAll, StreamExt};
use oximux_agents::session_registry::{ChoiceKind, Seq, SessionId};
use oximux_remote_proto::Transport;
use oximux_remote_proto::proto::{Request, Response, RpcError};

use super::stream::{Live, forward_terminal};
use super::{ConnState, Dispatcher, authorized_pubkey};

/// What this turn of the serve loop picked to process. `Live` is boxed — a
/// [`LiveFrame`] is far larger than a request frame. A `Request(None)` means the
/// transport closed/errored (stop); a `Live(None)` means all subscriptions ended
/// (loop back to the plain-recv path).
enum Picked {
    Request(Option<Vec<u8>>),
    Live(Option<Box<Live>>),
}

/// The session a request acts on, when it names one.
///
/// Used to decide whether a dormant session has to be built before the request
/// can be served. The catch-all is deliberate and safe in one direction only: a
/// session-scoped request missed here still works for a live session and answers
/// `UnknownSession` for a dormant one — the behaviour before catalogs existed —
/// whereas listing a request that does *not* act on a session would build views
/// for nothing.
fn target_session(req: &Request) -> Option<&str> {
    match req {
        Request::GetSessionInfo { session_id }
        | Request::FetchTranscript { session_id }
        | Request::Subscribe { session_id, .. }
        | Request::EventsSince { session_id, .. }
        | Request::Steer { session_id, .. }
        | Request::Cancel { session_id }
        | Request::ListChoices { session_id }
        | Request::SetModel { session_id, .. }
        | Request::SetPermissionMode { session_id, .. }
        | Request::RewindSession { session_id, .. }
        | Request::GitStatus { session_id }
        | Request::GitDiff { session_id, .. }
        | Request::GitStage { session_id, .. }
        | Request::GitUnstage { session_id, .. }
        | Request::GitCommit { session_id, .. }
        | Request::ListForgeItems { session_id, .. }
        | Request::GetForgeItemDetail { session_id, .. }
        | Request::ListForgeChecks { session_id } => Some(session_id),
        Request::SendPrompt(r) => Some(&r.session_id),
        Request::ResolvePermission(r) => Some(&r.session_id),
        Request::AnswerQuestion(r) => Some(&r.session_id),
        _ => None,
    }
}

impl Dispatcher {
    /// Serve one connection until the peer closes or a send fails. A request
    /// yields exactly one response frame; a live subscription pushes many
    /// [`Response::Event`] frames unsolicited between requests.
    pub async fn serve(&self, transport: &dyn Transport) {
        let mut state = ConnState::default();
        // Active live subscriptions, merged so any one that produces an event wakes
        // the loop. Empty until the first accepted `Subscribe`.
        let mut streams: SelectAll<BoxStream<'static, Live>> = SelectAll::new();
        let mut cursors: HashMap<SessionId, Seq> = HashMap::new();
        // Terminals this connection already streams, each with the handle that
        // ends its stream. A repeat attach serves the replay again without
        // opening a second stream — re-attaching IS the documented gap
        // recovery, so it has to stay cheap and repeatable — and a detach must
        // actually stop the old one, or the next attach stacks a second stream
        // beside it and every byte arrives twice.
        let mut attached: HashMap<String, tokio::sync::oneshot::Sender<()>> = HashMap::new();
        // Whether this connection holds a session-list subscription. A repeat
        // `SubscribeSessions` re-snapshots without opening a second stream.
        let mut sessions_subscribed = false;
        // `futures::future::select` is left-biased (always polls its first arg
        // first). Alternating which side leads each turn keeps neither the request
        // stream nor live delivery able to starve the other: a flood of pipelined
        // requests can't leave the bounded broadcast ring unread until it laps
        // (which would silently drop live events), and vice-versa.
        let mut live_leads = false;

        loop {
            let picked = if streams.is_empty() {
                // No live subscription: block purely on the next request — never
                // busy-spin on an ended/empty merged stream.
                Picked::Request(transport.recv().await.ok().flatten())
            } else {
                // Race the next request against the next live event, alternating the
                // lead. The unfinished future is returned by `select` and dropped
                // here, releasing its borrow on `streams` before the handler below
                // mutates it.
                live_leads = !live_leads;
                if live_leads {
                    match select(streams.next(), transport.recv()).await {
                        Either::Left((live, _recv)) => Picked::Live(live.map(Box::new)),
                        Either::Right((res, _live)) => Picked::Request(res.ok().flatten()),
                    }
                } else {
                    match select(transport.recv(), streams.next()).await {
                        Either::Left((res, _live)) => Picked::Request(res.ok().flatten()),
                        Either::Right((live, _recv)) => Picked::Live(live.map(Box::new)),
                    }
                }
            };

            match picked {
                Picked::Request(Some(frame)) => {
                    if !self
                        .on_request(
                            &mut state,
                            &mut streams,
                            &mut cursors,
                            &mut attached,
                            &mut sessions_subscribed,
                            transport,
                            frame,
                        )
                        .await
                    {
                        break;
                    }
                }
                // Transport closed or errored.
                Picked::Request(None) => break,
                Picked::Live(Some(frame)) => {
                    // Re-derive the still-authorized pubkey per frame; a revoked
                    // connection forwards nothing.
                    match authorized_pubkey(&state.authn, &self.auth) {
                        Some(pubkey) => {
                            let alive = match *frame {
                                Live::Session(f) => {
                                    self.forward_live(&pubkey, &mut cursors, transport, f).await
                                }
                                Live::Terminal { pty_id, frame } => {
                                    forward_terminal(&self.auth, &pubkey, transport, pty_id, frame)
                                        .await
                                }
                                Live::SessionList => {
                                    self.forward_sessions(&pubkey, transport).await
                                }
                            };
                            if !alive {
                                break;
                            }
                        }
                        None => continue,
                    }
                }
                // All subscriptions ended; loop back to the empty-stream arm.
                Picked::Live(None) => continue,
            }
        }
    }

    /// Handle one request frame: decode, special-case `Subscribe` (it also opens a
    /// live stream), else route through the synchronous [`Dispatcher::dispatch`].
    /// Returns whether the transport is still writable.
    ///
    /// The four `&mut` params after `state` are the serve loop's own per-connection
    /// state. Bundling them into a struct would satisfy the argument-count lint, but
    /// `streams` is borrowed by the left-biased `select` in the caller — the loop
    /// depends on the unfinished future being dropped to release that borrow before
    /// this runs — so hiding it behind a shared handle trades a real borrow hazard in
    /// the transport hot path for a cosmetic count. One private caller; kept flat.
    #[allow(clippy::too_many_arguments)]
    async fn on_request(
        &self,
        state: &mut ConnState,
        streams: &mut SelectAll<BoxStream<'static, Live>>,
        cursors: &mut HashMap<SessionId, Seq>,
        attached: &mut HashMap<String, tokio::sync::oneshot::Sender<()>>,
        sessions_subscribed: &mut bool,
        transport: &dyn Transport,
        frame: Vec<u8>,
    ) -> bool {
        let req = match Request::from_bytes(&frame) {
            Ok(req) => req,
            Err(_) => {
                let err = Response::Error(RpcError::BadRequest("undecodable request frame".into()));
                return self.send(transport, err).await;
            }
        };
        // A session whose project the desktop has not shown this run has no views
        // behind it and so no registry entry, which would make the requests below
        // answer `UnknownSession`. Build it first, so a client can reach any
        // session that exists rather than only the ones the desktop happens to be
        // displaying.
        //
        // Placed after authentication and behind the same per-session ACL the
        // handlers apply, because this spawns an agent process: an
        // unauthenticated peer must not be able to make the desktop do work, and
        // a session-scoped device must not reach past its scope by naming an id.
        // A failure is logged and falls through — the handler then answers
        // `UnknownSession` on its own, which is the truthful answer.
        if let Some(session_id) = target_session(&req)
            && self.registry.get(session_id).is_none()
            && let Some(catalog) = &self.catalog
            && let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth)
            && self.auth.is_allowed_for(&pubkey, session_id)
            && let Err(err) = catalog.open(session_id).await
        {
            tracing::warn!(session_id, %err, "could not open a session a client asked for");
        }
        // `Subscribe` is the one request that also opens a live stream, so it is
        // handled in the serve loop rather than the sync `dispatch`.
        if let Request::Subscribe { session_id, after_seq } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let (response, stream) =
                self.begin_subscribe(&pubkey, &session_id, after_seq.unwrap_or(0), cursors);
            if let Some(stream) = stream {
                streams.push(stream.map(Live::Session).boxed());
            }
            return self.send(transport, response).await;
        }
        // `SubscribeSessions`, like `Subscribe`, also opens a live stream, so it is
        // handled here rather than in the sync `dispatch`.
        if let Request::SubscribeSessions = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let (response, stream) = self.begin_subscribe_sessions(&pubkey, sessions_subscribed);
            if let Some(stream) = stream {
                streams.push(stream);
            }
            return self.send(transport, response).await;
        }
        // The terminal RPCs are async (the PTY layer is), so they are awaited
        // here rather than in the sync `dispatch`, exactly as the git ones are.
        if let Request::ListTerminals = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.list_terminals(&pubkey).await;
            return self.send(transport, response).await;
        }
        if let Request::TermAttach { pty_id } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let (response, stream) = self.begin_term_attach(&pubkey, &pty_id, attached).await;
            if let Some(stream) = stream {
                streams.push(stream);
            }
            return self.send(transport, response).await;
        }
        if let Request::TermInput { pty_id, bytes } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.term_input(&pubkey, &pty_id, &bytes).await;
            return self.send(transport, response).await;
        }
        if let Request::TermResize { pty_id, cols, rows } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.term_resize(&pubkey, &pty_id, cols, rows).await;
            return self.send(transport, response).await;
        }
        if let Request::TermDetach { pty_id } = req {
            // Idempotent and unauthenticated-safe: ending a stream this
            // connection holds is never a privilege. Dropping the cancel sender
            // is what actually stops it — `SelectAll` cannot remove a stream, so
            // merely forgetting the entry would leave it forwarding forever.
            attached.remove(&pty_id);
            return self.send(transport, Response::Ack).await;
        }
        // `GitStatus` is the one authenticated request whose handler is async (it
        // shells out to git), so it is awaited here rather than in the sync
        // `dispatch`. Authorization is identical: an authenticated pubkey, then the
        // handler's own per-session ACL recheck.
        if let Request::GitStatus { session_id } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.git_status(&pubkey, &session_id).await;
            return self.send(transport, response).await;
        }
        if let Request::GitDiff { session_id, path, staged, untracked } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.git_diff(&pubkey, &session_id, &path, staged, untracked).await;
            return self.send(transport, response).await;
        }
        // The git writes are async for the same reason as the reads (they shell
        // out), so they are awaited here too. Their own handlers apply the
        // `may_write` gate on top of this authentication check.
        if let Request::GitStage { session_id, paths } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.git_stage(&pubkey, &session_id, &paths).await;
            return self.send(transport, response).await;
        }
        if let Request::GitUnstage { session_id, paths } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.git_unstage(&pubkey, &session_id, &paths).await;
            return self.send(transport, response).await;
        }
        if let Request::GitCommit { session_id, message } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.git_commit(&pubkey, &session_id, &message).await;
            return self.send(transport, response).await;
        }
        // Launching is async (it round-trips to the desktop's UI thread), so it
        // is awaited here rather than in the sync `dispatch`, like the git RPCs.
        if let Request::CreateSession { cwd, agent_id } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.create_session(&pubkey, &cwd, agent_id.as_deref()).await;
            return self.send(transport, response).await;
        }
        // Switching a model or permission mode is async for the same reason: a
        // backend that fixes the value at spawn can only change it by respawning,
        // which the desktop view performs on its own thread.
        if let Request::SetModel { session_id, model } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response =
                self.set_choice(&pubkey, &session_id, ChoiceKind::Model, &model).await;
            return self.send(transport, response).await;
        }
        if let Request::SetPermissionMode { session_id, mode } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response =
                self.set_choice(&pubkey, &session_id, ChoiceKind::PermissionMode, &mode).await;
            return self.send(transport, response).await;
        }
        // Listing projects reads the desktop's recent-projects snapshot off its UI
        // thread, so it is async and awaited here beside its create-session sibling.
        if let Request::ListProjects = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.list_projects(&pubkey).await;
            return self.send(transport, response).await;
        }
        // The forge RPCs shell out to `gh`/`glab`, so they are awaited here for
        // the same reason the git reads are: a network-bound CLI call must not
        // block the synchronous dispatch path.
        // `item_state` rather than `state`: the connection's own `state` is in
        // scope here, and shadowing it inside this arm would be a trap for the
        // next edit.
        if let Request::ListForgeItems { session_id, kind, state: item_state, mine } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response =
                self.list_forge_items(&pubkey, &session_id, kind, item_state, mine).await;
            return self.send(transport, response).await;
        }
        if let Request::GetForgeItemDetail { session_id, kind, number } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.forge_item_detail(&pubkey, &session_id, kind, number).await;
            return self.send(transport, response).await;
        }
        if let Request::ListForgeChecks { session_id } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response = self.list_forge_checks(&pubkey, &session_id).await;
            return self.send(transport, response).await;
        }
        // Rewinding round-trips to the desktop's UI thread like launching does.
        if let Request::RewindSession { session_id, ordinal, include_files } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let response =
                self.rewind_session(&pubkey, &session_id, ordinal as usize, include_files).await;
            return self.send(transport, response).await;
        }
        // Transcription runs a CPU-heavy ONNX decode, so its handler is async (it
        // `spawn_blocking`s the decode) and is awaited here rather than in the
        // sync `dispatch`. Gated on the authenticated connection alone — it names
        // no session and mutates nothing.
        if let Request::TranscribeAudio { audio_base64, sample_rate } = req {
            if authorized_pubkey(&state.authn, &self.auth).is_none() {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            }
            let response = self.transcribe_audio(&audio_base64, sample_rate).await;
            return self.send(transport, response).await;
        }
        let response = self.dispatch(state, req);
        self.send(transport, response).await
    }

    /// Encode + send one response frame; returns whether the transport accepted it.
    async fn send(&self, transport: &dyn Transport, response: Response) -> bool {
        // `Response` carries no `serde_json::Value`, so it always postcard-encodes.
        let bytes = response.to_bytes().expect("response is always encodable");
        transport.send(bytes).await.is_ok()
    }
}
