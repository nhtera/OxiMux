//! `Request::Simulator`: the gate in front of the desktop's simulator.
//!
//! Three checks, in an order that leaks nothing: may this caller drive the
//! simulator at all (a paired phone may not, and learns no more than that);
//! does this host have one (a headless host says `Unsupported` only to a
//! caller who could have used it); and which worktree the call is about. A
//! session-confined agent does not get to say — its worktree is its own
//! session's working directory, so it cannot aim at another project's device.

use oximux_remote_proto::proto::{Response, RpcError};
use oximux_remote_proto::simulator::{SimErrorWire, SimRequestWire};

use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    pub(super) async fn simulator(&self, peer: &Peer, req: SimRequestWire) -> Response {
        if !self.auth.may_control_simulator(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(simulator) = &self.simulator else {
            return Response::Error(RpcError::Unsupported);
        };
        let worktree = match peer.own_session() {
            // The request's own field is ignored: the proven scope is the only
            // honest source. A session with no working directory has no
            // worktree, so no device.
            Some(session) => match self.registry.get(session).and_then(|h| h.meta_snapshot().cwd) {
                Some(cwd) => cwd.to_string_lossy().into_owned(),
                None => return Response::Simulator(Err(SimErrorWire::NoDevice)),
            },
            None => match req.worktree {
                Some(path) if !path.trim().is_empty() => path,
                _ => {
                    return Response::Error(RpcError::BadRequest(
                        "name the worktree whose simulator to use (`--worktree`, or run inside it)".into(),
                    ));
                }
            },
        };
        Response::Simulator(simulator.run(&worktree, req.cmd).await)
    }
}
