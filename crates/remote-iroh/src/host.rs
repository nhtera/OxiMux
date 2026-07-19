//! The host accept loop: bind is done by [`bind_host`](crate::bind_host); this
//! drives the bound endpoint, serving every inbound connection through the
//! transport-agnostic [`Dispatcher`] until asked to stop.
//!
//! Kept here (not in `remote-host`) because it is iroh-specific — it turns each
//! accepted QUIC bi-stream into a framed [`IrohTransport`](crate::IrohTransport)
//! and hands it to `dispatcher.serve`. The dispatcher itself never sees iroh.

use std::future::Future;
use std::sync::Arc;

use iroh::Endpoint;
use oximux_remote_host::Dispatcher;
use oximux_remote_proto::PairingTicket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::endpoint::{accept, bind_host};

/// Run the host accept loop until `shutdown` resolves or the endpoint stops
/// accepting, then close the endpoint.
///
/// Each accepted connection is served on its own task, so several phones can be
/// paired and streaming at once; when one hangs up its serve task ends on its
/// own. A single failed accept (e.g. a half-open dial or a bad handshake) is
/// skipped rather than tearing the whole host down. On exit the endpoint is
/// closed so n0 relays/pkarr drop the host promptly instead of advertising a
/// dead address.
///
/// `shutdown` is polled first each turn (`biased`) so a stop request is honored
/// even under a steady stream of inbound dials.
pub async fn serve_host(
    endpoint: Endpoint,
    dispatcher: Arc<Dispatcher>,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            res = accept(&endpoint) => match res {
                Ok(Some(transport)) => {
                    let dispatcher = dispatcher.clone();
                    tokio::spawn(async move { dispatcher.serve(&transport).await });
                }
                // The endpoint stopped accepting (closed elsewhere) — nothing
                // left to serve.
                Ok(None) => break,
                // One connection failed to establish; keep listening for others.
                Err(_) => continue,
            },
        }
    }
    endpoint.close().await;
}

/// A running host: the accept loop on its task plus the [`PairingTicket`] a phone
/// scans to reach it. Signal [`shutdown`](Self::shutdown) (or drop) to stop.
pub struct HostHandle {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    ticket: PairingTicket,
}

impl HostHandle {
    /// The ticket to hand the phone (encode as the pairing QR). Carries the host
    /// endpoint id + the one-time handshake secret — no network address, so the
    /// host stays reachable by id alone via pkarr/relays without disclosing an IP.
    pub fn ticket(&self) -> &PairingTicket {
        &self.ticket
    }

    /// Ask the accept loop to stop (idempotent). The endpoint closes as the task
    /// unwinds; fire-and-forget from a sync context. Use [`join`](Self::join) to
    /// await full teardown.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    /// Stop and await the accept loop's full teardown (endpoint closed).
    pub async fn join(mut self) {
        self.shutdown();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HostHandle {
    /// Dropping a handle stops its host — a dangling accept loop would keep the
    /// endpoint advertised after the owner is gone.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind a host endpoint, wait until it is dialable, and start serving `dispatcher`
/// on it — returning a [`HostHandle`] whose [`ticket`](HostHandle::ticket) reaches
/// this host. `secret` is the one-time handshake secret the ticket carries; it
/// MUST match the [`PairingSlot`](oximux_remote_host::PairingSlot) the dispatcher's
/// auth store was seeded with. Bind is deferred to call time (never at boot) so it
/// only pays its cost when remote access is actually turned on.
///
/// [`PairingSlot`]: oximux_remote_host::PairingSlot
pub async fn start_host(
    dispatcher: Arc<Dispatcher>,
    secret: [u8; 16],
) -> anyhow::Result<HostHandle> {
    let endpoint = bind_host().await?;
    // Wait until direct addresses / relay home are known so the ticket is
    // reachable the moment it is shown.
    endpoint.online().await;
    let ticket = PairingTicket {
        endpoint_id: *endpoint.id().as_bytes(),
        handshake_secret: secret,
        session_id: None,
    };
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(serve_host(endpoint, dispatcher, async move {
        let _ = rx.await;
    }));
    Ok(HostHandle { shutdown: Some(tx), task: Some(task), ticket })
}
