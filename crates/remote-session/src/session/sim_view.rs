//! Watching a device on the desktop (v34): list, subscribe, unsubscribe, and
//! the one-way streams that carry the pictures.
//!
//! The desktop's own answers (viewing switched off, waiting on the user's
//! allow, the device gone) come back as the inner `Err`, not as a
//! [`SessionError`]: they are answers the app shows, while the outer error is
//! the call itself failing.

use futures::FutureExt;
use futures::future::{Either, Map, Shared, select};
use oximux_remote_proto::{Transport, UniRecv};
use oximux_remote_proto::proto::{Request, Response};
use oximux_remote_proto::simulator::{SimErrorWire, SimSubscribedWire, SimViewToken, SimViewableWire};

use crate::error::SessionError;
use crate::session::{RemoteSession, Result};

impl RemoteSession {
    /// The devices this phone may watch.
    pub async fn sim_viewables(&self) -> Result<std::result::Result<Vec<SimViewableWire>, SimErrorWire>> {
        match self.call(Request::SimViewables).await? {
            Response::SimViewables(rows) => Ok(rows),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "SimViewables" }),
        }
    }

    /// Start watching `device`. The picture arrives on a stream of its own
    /// ([`Self::take_view_streams`]), named by the returned token.
    pub async fn sim_subscribe(&self, device: &str) -> Result<std::result::Result<SimSubscribedWire, SimErrorWire>> {
        match self.call(Request::SimSubscribe { device: device.to_string() }).await? {
            Response::SimSubscribed(answer) => Ok(answer),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "SimSubscribed" }),
        }
    }

    /// Stop watching. Idempotent.
    pub async fn sim_unsubscribe(&self, token: SimViewToken) -> Result<()> {
        match self.call(Request::SimUnsubscribe { token }).await? {
            Response::Ack => Ok(()),
            Response::Error(e) => Err(SessionError::Rpc(e)),
            _ => Err(SessionError::Unexpected { expected: "Ack" }),
        }
    }

    /// Take the view streams of this connection — once, like
    /// [`Self::take_terminals`].
    pub fn take_view_streams(&self) -> Option<ViewStreams> {
        let closed = self.views_closed.lock().unwrap().take()?;
        Some(ViewStreams { transport: self.transport.clone(), closed: closed.map(drop as fn(_)).shared() })
    }

    /// Whether the connection runs through a relay (see
    /// [`Transport::relayed`](oximux_remote_proto::Transport::relayed)).
    pub fn relayed(&self) -> Option<bool> {
        self.transport.relayed()
    }
}

/// Resolves once the [`RemoteSession`] it came from is dropped.
pub type SessionClosed = Shared<Map<futures::channel::oneshot::Receiver<()>, fn(std::result::Result<(), futures::channel::oneshot::Canceled>)>>;

/// The one-way streams the host opens on one connection (a watched device's
/// picture each), ending when the session is dropped.
///
/// Holds the transport, not the session: a session dropped (a disconnect, a
/// reconnect replacing it) must still close its connection, and anything
/// waiting on its streams would otherwise keep it open, with the host
/// streaming into a connection nobody reads.
pub struct ViewStreams {
    transport: std::sync::Arc<dyn Transport>,
    closed: SessionClosed,
}

impl ViewStreams {
    /// The next stream: `None` once the connection or the session is gone.
    /// Its first bytes are the token of the subscription it belongs to.
    pub async fn next(&self) -> Option<Box<dyn UniRecv>> {
        let accept = Box::pin(self.transport.accept_uni());
        match select(accept, self.closed.clone()).await {
            Either::Left((stream, _)) => stream.ok().flatten(),
            Either::Right(_) => None,
        }
    }

    /// Resolves once the session is dropped: a reader of one stream stops
    /// there too, letting the connection close.
    pub fn closed(&self) -> SessionClosed {
        self.closed.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;
    use oximux_remote_proto::testing::duplex_pair;

    use crate::session::RemoteSession;
    use crate::signer::ClientSigner;

    /// **A dropped session lets its connection go.** Waiting on its view
    /// streams must not keep it alive: a disconnect would otherwise leave the
    /// host streaming into a connection nobody reads.
    #[test]
    fn dropping_the_session_ends_its_view_streams() {
        let (client, host) = duplex_pair();
        let session = RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&[1; 32]));
        let views = session.take_view_streams().expect("once");
        assert!(session.take_view_streams().is_none(), "taken once");
        let closed = views.closed();
        drop(session);
        block_on(async {
            assert!(views.next().await.is_none(), "no more streams once the session is gone");
            closed.await;
        });
        drop(host);
    }
}
