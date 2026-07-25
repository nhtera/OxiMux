//! Opening a session's live stream. The pushed frames are read off the demux
//! event stream ([`RemoteSession::take_events`]); the fold lives in
//! [`crate::subscription`].

use oximux_remote_proto::HostEvent;
use oximux_remote_proto::proto::{Request, Response};

use super::{RemoteSession, Result};
use crate::error::SessionError;

impl RemoteSession {
    /// Subscribe to a session's live stream. Returns the backlog after `after_seq`
    /// (the immediate `Events` reply); each subsequent live frame arrives on the
    /// event stream taken with [`Self::take_events`].
    ///
    /// The host awaits the backlog send before it forwards any live frame, so the
    /// backlog is complete as of the subscribe. Because every RPC rides the demux
    /// pump, other requests stay safe to issue on this connection while the stream
    /// runs — the pump keeps pushed events and RPC replies from colliding.
    pub async fn subscribe(&self, session_id: &str, after_seq: u64) -> Result<Vec<HostEvent>> {
        let req =
            Request::Subscribe { session_id: session_id.to_string(), after_seq: Some(after_seq) };
        match self.call(req).await? {
            Response::Events(backlog) => Ok(backlog),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Events" }),
        }
    }
}
