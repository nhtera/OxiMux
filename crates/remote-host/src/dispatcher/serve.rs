//! The per-connection serve loop: multiplex incoming client requests against the
//! live events of any active `Subscribe`, so a live event is pushed the moment it
//! is produced without waiting on the next request.

use std::collections::HashMap;

use futures::future::{Either, select};
use futures::stream::{BoxStream, SelectAll, StreamExt};
use oximux_agents::session_registry::{Seq, SessionId};
use oximux_remote_proto::Transport;
use oximux_remote_proto::proto::{Request, Response, RpcError};

use super::stream::LiveFrame;
use super::{ConnState, Dispatcher, authorized_pubkey};

/// What this turn of the serve loop picked to process. `Live` is boxed — a
/// [`LiveFrame`] is far larger than a request frame. A `Request(None)` means the
/// transport closed/errored (stop); a `Live(None)` means all subscriptions ended
/// (loop back to the plain-recv path).
enum Picked {
    Request(Option<Vec<u8>>),
    Live(Option<Box<LiveFrame>>),
}

impl Dispatcher {
    /// Serve one connection until the peer closes or a send fails. A request
    /// yields exactly one response frame; a live subscription pushes many
    /// [`Response::Event`] frames unsolicited between requests.
    pub async fn serve(&self, transport: &dyn Transport) {
        let mut state = ConnState::default();
        // Active live subscriptions, merged so any one that produces an event wakes
        // the loop. Empty until the first accepted `Subscribe`.
        let mut streams: SelectAll<BoxStream<'static, LiveFrame>> = SelectAll::new();
        let mut cursors: HashMap<SessionId, Seq> = HashMap::new();
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
                    if !self.on_request(&mut state, &mut streams, &mut cursors, transport, frame).await
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
                            if !self.forward_live(&pubkey, &mut cursors, transport, *frame).await {
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
    async fn on_request(
        &self,
        state: &mut ConnState,
        streams: &mut SelectAll<BoxStream<'static, LiveFrame>>,
        cursors: &mut HashMap<SessionId, Seq>,
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
        // `Subscribe` is the one request that also opens a live stream, so it is
        // handled in the serve loop rather than the sync `dispatch`.
        if let Request::Subscribe { session_id, after_seq } = req {
            let Some(pubkey) = authorized_pubkey(&state.authn, &self.auth) else {
                return self.send(transport, Response::Error(RpcError::Unauthorized)).await;
            };
            let (response, stream) =
                self.begin_subscribe(&pubkey, &session_id, after_seq.unwrap_or(0), cursors);
            if let Some(stream) = stream {
                streams.push(stream);
            }
            return self.send(transport, response).await;
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
