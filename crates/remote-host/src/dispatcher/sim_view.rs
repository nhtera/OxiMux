//! Watching a device from the paired phone: the three view RPCs, and the
//! pump that writes one subscription's picture to its own one-way stream.
//!
//! The gates run in the same order as the simulator's, so a caller learns no
//! more than it may: may it watch at all ([`AuthStore::may_view_simulator`]),
//! does this host show devices, then whatever the desktop says (its setting,
//! its per-device ask). Only a paired phone subscribes: a local caller's
//! socket carries no second stream.
//!
//! The picture never rides the connection's message stream. Each
//! subscription gets a uni-stream the host opens, so a key frame in flight
//! never holds a terminal's keystrokes behind it, and the stream sits below
//! the message stream's priority on the wire.
//!
//! **Authorization is checked for every frame, and at least once a second
//! while the screen is still**, like terminal frames: revoking or forgetting
//! the phone, or the user stopping sharing on the desktop, ends a live view
//! within a second instead of when the connection next does something.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use oximux_remote_proto::Transport;
use oximux_remote_proto::proto::{Response, RpcError};
use oximux_remote_proto::simulator::{SimErrorWire, SimSubscribedWire, SimViewToken, encode_view_frame};
use oximux_remote_proto::transport::{TransportError, UniSend};
use rand::RngCore;
use rand::rngs::OsRng;

use super::Dispatcher;
use crate::auth::{AuthStore, Peer};
use crate::simulator::{SimViewSource, SimViewer};

/// How long the pump waits on a still screen (or a stalled write) before
/// asking again whether the phone may still watch.
const RECHECK: Duration = Duration::from_secs(1);

/// A paced stream's write size: about one datagram, so the link never holds
/// more than a packet or two of video ahead of an RPC.
const CHUNK: usize = 1200;

/// How far a paced stream may run ahead of its rate (a short burst).
const BURST: Duration = Duration::from_millis(20);

/// Spreads a stream's bytes over time at a fixed rate.
///
/// QUIC's own priority puts RPCs first only while bytes wait in the sender;
/// once a key frame is in the link's queue, the RPC behind it waits there.
/// Writing at the feed's rate, below what the link carries, keeps that
/// queue empty: the bytes wait here instead, where nothing else is behind
/// them.
struct WritePacer {
    /// Seconds per byte.
    per_byte: f64,
    /// When the next byte may go.
    next: tokio::time::Instant,
}

impl WritePacer {
    fn new(bits_per_sec: u64) -> Option<Self> {
        (bits_per_sec > 0).then(|| Self { per_byte: 8.0 / bits_per_sec as f64, next: tokio::time::Instant::now() })
    }

    /// When `len` bytes may be written, booking them.
    fn book(&mut self, len: usize) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        // An idle stream earns at most a burst's worth of credit.
        let start = self.next.max(now.checked_sub(BURST).unwrap_or(now));
        self.next = start + Duration::from_secs_f64(len as f64 * self.per_byte);
        start
    }
}

/// The most devices one connection watches at once (the phone shows one;
/// a few allow for switching while the last one winds down).
const MAX_VIEWS: usize = 4;

/// One connection's live views, by token. Dropping it (the connection is
/// over) stops every pump.
#[derive(Default)]
pub(super) struct SimViews(HashMap<SimViewToken, tokio::task::AbortHandle>);

impl SimViews {
    /// Forget pumps that ended on their own (the device stopped, the phone
    /// stopped reading), so they do not count against [`MAX_VIEWS`].
    fn prune(&mut self) {
        self.0.retain(|_, pump| !pump.is_finished());
    }
}

impl Drop for SimViews {
    fn drop(&mut self) {
        for pump in self.0.values() {
            pump.abort();
        }
    }
}

impl Dispatcher {
    /// The first gates every view RPC shares: may watch, host shows devices,
    /// a paired phone asking.
    fn view_gate(&self, peer: &Peer) -> Result<(Arc<dyn crate::simulator::SimulatorControl>, SimViewer), RpcError> {
        if !self.auth.may_view_simulator(peer) {
            return Err(RpcError::Unauthorized);
        }
        let Some(simulator) = self.simulator.as_ref().filter(|s| s.supports_view()) else {
            return Err(RpcError::Unsupported);
        };
        let Some(phone) = peer.remote_pubkey().copied() else {
            return Err(RpcError::Unsupported);
        };
        let phone_name = self.auth.device_name(&phone).unwrap_or_else(|| "a paired phone".into());
        Ok((Arc::clone(simulator), SimViewer { id: 0, phone, phone_name }))
    }

    pub(super) async fn sim_viewables(&self, peer: &Peer) -> Response {
        match self.view_gate(peer) {
            Ok((simulator, viewer)) => Response::SimViewables(simulator.view_list(&viewer).await),
            Err(e) => Response::Error(e),
        }
    }

    pub(super) async fn sim_subscribe(
        &self,
        views: &mut SimViews,
        peer: &Peer,
        device: &str,
        transport: &dyn Transport,
    ) -> Response {
        let (simulator, mut viewer) = match self.view_gate(peer) {
            Ok(gate) => gate,
            Err(e) => return Response::Error(e),
        };
        // The pump is a task of its own; a host driven by a bare executor (a
        // test) has nowhere to run one.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return Response::Error(RpcError::Unsupported);
        };
        views.prune();
        if views.0.len() >= MAX_VIEWS {
            return Response::SimSubscribed(Err(SimErrorWire::Refused(
                "this phone is already watching as many devices as it may; stop one first".into(),
            )));
        }
        viewer.id = self.view_ids.fetch_add(1, Ordering::Relaxed) + 1;
        let feed = match simulator.view_subscribe(&viewer, device).await {
            Ok(feed) => feed,
            Err(e) => return Response::SimSubscribed(Err(e)),
        };
        // Dropping `feed` on any failure below releases the viewer.
        let stream = match transport.open_uni().await {
            Ok(stream) => stream,
            Err(TransportError::Unsupported) => return Response::Error(RpcError::Unsupported),
            Err(e) => {
                tracing::debug!(error = %e, "could not open a view stream");
                return Response::Error(RpcError::Internal("the connection could not carry the picture".into()));
            }
        };
        let mut token: SimViewToken = [0; 16];
        OsRng.fill_bytes(&mut token);
        let pacer = WritePacer::new(feed.bits_per_sec);
        let pump = runtime.spawn(pump(stream, token, feed.source, pacer, Arc::clone(&self.auth), peer.clone()));
        views.0.insert(token, pump.abort_handle());
        Response::SimSubscribed(Ok(SimSubscribedWire {
            token,
            width: feed.width,
            height: feed.height,
        }))
    }

    pub(super) fn sim_unsubscribe(&self, views: &mut SimViews, peer: &Peer, token: &SimViewToken) -> Response {
        if !self.auth.may_view_simulator(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        if let Some(pump) = views.0.remove(token) {
            pump.abort();
        }
        Response::Ack
    }
}

/// Whether the phone may still watch: paired, not revoked, full scope, and
/// the desktop still sharing.
fn still_allowed(auth: &AuthStore, peer: &Peer, source: &dyn SimViewSource) -> bool {
    auth.may_view_simulator(peer) && source.allowed()
}

/// Write `bytes`, asking [`still_allowed`] every [`RECHECK`] while the phone
/// is not keeping up. `false` once it may not, or the stream is gone.
async fn write_while_allowed(
    stream: &mut dyn UniSend,
    bytes: &[u8],
    auth: &AuthStore,
    peer: &Peer,
    source: &dyn SimViewSource,
) -> bool {
    let mut write = std::pin::pin!(stream.write_all(bytes));
    loop {
        match tokio::time::timeout(RECHECK, &mut write).await {
            Ok(written) => return written.is_ok(),
            // Still writing: a stalled link must not keep a revoked phone's
            // stream open. An unfinished frame is fine when ending.
            Err(_) if still_allowed(auth, peer, source) => continue,
            Err(_) => return false,
        }
    }
}

/// One subscription's stream: the token, then frames until the device stops,
/// the phone may no longer watch, or the stream is gone. Dropping `source`
/// on the way out ends the subscription on the desktop.
async fn pump(
    mut stream: Box<dyn UniSend>,
    token: SimViewToken,
    mut source: Box<dyn SimViewSource>,
    mut pacer: Option<WritePacer>,
    auth: Arc<AuthStore>,
    peer: Peer,
) {
    if !write_while_allowed(stream.as_mut(), &token, &auth, &peer, source.as_ref()).await {
        return;
    }
    loop {
        if !still_allowed(&auth, &peer, source.as_ref()) {
            tracing::debug!("a phone's view ended: it may no longer watch");
            return;
        }
        let frame = match tokio::time::timeout(RECHECK, source.next()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            // A still screen: loop to check again.
            Err(_) => continue,
        };
        if !still_allowed(&auth, &peer, source.as_ref()) {
            return;
        }
        let bytes = encode_view_frame(frame.kind, frame.pts_us, &frame.payload);
        let Some(pacer) = pacer.as_mut() else {
            if !write_while_allowed(stream.as_mut(), &bytes, &auth, &peer, source.as_ref()).await {
                return;
            }
            continue;
        };
        for chunk in bytes.chunks(CHUNK) {
            tokio::time::sleep_until(pacer.book(chunk.len())).await;
            // A large frame takes seconds to pace out: a revoke or Stop
            // sharing meanwhile ends it at the next chunk, not after it.
            if !still_allowed(&auth, &peer, source.as_ref()) {
                return;
            }
            if !write_while_allowed(stream.as_mut(), chunk, &auth, &peer, source.as_ref()).await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_pacer_spreads_bytes_at_its_rate_with_a_short_burst() {
        // 800 kbit/s: 100 kB/s, so 10 kB takes 100 ms.
        let mut pacer = WritePacer::new(800_000).expect("paced");
        let start = tokio::time::Instant::now();
        let mut last = start;
        for _ in 0..10 {
            last = pacer.book(1_000);
        }
        let spread = last.duration_since(start);
        assert!(spread >= Duration::from_millis(85) && spread <= Duration::from_millis(95), "{spread:?}");
        assert!(WritePacer::new(0).is_none(), "0 is unpaced");
    }

    #[tokio::test]
    async fn an_idle_pacer_allows_only_a_short_burst() {
        let mut pacer = WritePacer::new(800_000).expect("paced");
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Idle for 300 ms: the credit is capped at the burst, not 30 kB.
        let start = tokio::time::Instant::now();
        let mut last = start;
        for _ in 0..10 {
            last = pacer.book(1_000);
        }
        assert!(last.duration_since(start) >= Duration::from_millis(60), "{:?}", last.duration_since(start));
    }
}
