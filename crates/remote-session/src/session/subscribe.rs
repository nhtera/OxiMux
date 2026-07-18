//! The live-subscription primitives on [`RemoteSession`]: open a stream and read
//! the pushed frames. The fold itself lives in [`crate::subscription`].

use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::HostEvent;

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// Subscribe to a session's live stream. Returns the backlog after `after_seq`
    /// (the immediate `Events` reply); each subsequent live frame is read with
    /// [`Self::next_event`].
    ///
    /// **Once subscribed, this connection interleaves pushed `Response::Event`
    /// frames with any RPC reply**, so do NOT issue other RPCs (via `call`) on it —
    /// drive the stream with `next_event`. Concurrent RPC-while-subscribed needs a
    /// demux pump, a later slice (see the note on `RemoteSession::call`).
    pub async fn subscribe(&self, session_id: &str, after_seq: u64) -> Result<Vec<HostEvent>> {
        let req =
            Request::Subscribe { session_id: session_id.to_string(), after_seq: Some(after_seq) };
        match self.call(req).await? {
            Response::Events(backlog) => Ok(backlog),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Events" }),
        }
    }

    /// Read the next pushed live event on a subscribed connection. `Ok(None)` when
    /// the host closes the stream.
    pub async fn next_event(&self) -> Result<Option<HostEvent>> {
        let frame = match self.transport.recv().await {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(None),
            Err(e) => return Err(SessionError::Transport(e.to_string())),
        };
        match Response::from_bytes(&frame).map_err(|e| SessionError::Wire(e.to_string()))? {
            Response::Event(event) => Ok(Some(event)),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Event" }),
        }
    }
}
