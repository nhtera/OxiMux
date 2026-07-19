//! [`MobileClient`] — the handle the app holds: connect to a paired host, then
//! drive sessions. This module owns construction + the connection lifecycle; the
//! async RPCs live in [`rpc`](crate::client::rpc) and subscription in
//! [`subscription`](crate::subscription).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::channel::oneshot;
use oximux_remote_iroh::{IrohConnector, bind_client};
use oximux_remote_proto::PairingTicket;
use oximux_remote_session::{ClientSigner, Connector, RemoteSession};
use tokio::sync::Mutex;

use crate::callbacks::ConnStateListener;
use crate::ffi_types::MobileError;
use crate::subscription::Sub;

mod connect;
mod rpc;

use connect::spawn_maintainer;

/// State shared between the client handle and its background tasks (the demux
/// pump feeds events; the dispatcher folds them into the registered [`Sub`]s).
pub(crate) struct Shared {
    /// The live session — swapped in by the reconnect driver on each (re)connect,
    /// cleared while reconnecting. A plain (non-async) mutex so the sync driver
    /// callbacks can update it; only ever held to clone the `Arc` out, never
    /// across an `.await`.
    pub session: StdMutex<Option<Arc<RemoteSession>>>,
    pub subs: Mutex<HashMap<String, Sub>>,
    /// Monotonic connection epoch. Bumped on every (re)connect, and on
    /// `connect_with`/`disconnect` (supersede). A backgrounded `activate` records
    /// the epoch it was spawned for and refuses to publish its session if a newer
    /// bump has since happened — so a slow/stale activation (reconnect flap, or a
    /// disconnect racing an in-flight activate) can't resurrect a dead session.
    pub epoch: AtomicU64,
}

impl Shared {
    /// Supersede the current connection: any in-flight `activate` for an older
    /// epoch will decline to publish. Returns the new current epoch.
    pub(crate) fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }
}

impl Shared {
    /// The live session, or [`MobileError::NotConnected`].
    pub(crate) fn session(&self) -> Result<Arc<RemoteSession>, MobileError> {
        self.session.lock().unwrap().clone().ok_or(MobileError::NotConnected)
    }
}

/// The phone's client object. Construct with [`MobileClient::new`], then
/// [`connect`](MobileClient::connect).
#[derive(uniffi::Object)]
pub struct MobileClient {
    signer: ClientSigner,
    pub(crate) shared: Arc<Shared>,
    /// Stops the current reconnect driver when the app disconnects or supersedes
    /// the connection. Taken + fired to signal the background loop to return.
    shutdown: StdMutex<Option<oneshot::Sender<()>>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Build a client with a stable app identity. `seed` is the 32-byte Ed25519
    /// seed persisted in the OS keystore; `None` mints a fresh random identity.
    #[uniffi::constructor]
    pub fn new(seed: Option<Vec<u8>>) -> Arc<Self> {
        let signer = match seed.as_deref() {
            Some(bytes) if bytes.len() == 32 => {
                let mut seed32 = [0u8; 32];
                seed32.copy_from_slice(bytes);
                ClientSigner::from_seed(&seed32)
            }
            _ => ClientSigner::generate(),
        };
        Arc::new(Self {
            signer,
            shared: Arc::new(Shared {
                session: StdMutex::new(None),
                subs: Mutex::new(HashMap::new()),
                epoch: AtomicU64::new(0),
            }),
            shutdown: StdMutex::new(None),
        })
    }

    /// Pair with and connect to the host named by a scanned
    /// `oximux://connect?ticket=…` deep link, over iroh. On success the session is
    /// live and `listener` has seen [`ConnState::Connected`](crate::ConnState); the
    /// connection then self-heals across drops until [`disconnect`](Self::disconnect).
    pub async fn connect(
        &self,
        ticket_url: String,
        device_name: String,
        listener: Arc<dyn ConnStateListener>,
    ) -> Result<(), MobileError> {
        let ticket =
            PairingTicket::from_url(&ticket_url).map_err(|e| MobileError::BadTicket(e.to_string()))?;
        let endpoint = bind_client().await.map_err(|e| MobileError::Transport(e.to_string()))?;
        let connector = IrohConnector::new(endpoint, ticket.endpoint_id)
            .map_err(|e| MobileError::Transport(e.to_string()))?;
        self.connect_with(Arc::new(connector), ticket, device_name, listener).await
    }

    /// Drop the connection: stop the reconnect driver and release the session +
    /// subscriptions so its pump + dispatcher wind down.
    pub async fn disconnect(&self) {
        self.stop_current();
        // Supersede first so any in-flight `activate` can't re-store a session
        // after we clear it below.
        self.shared.bump_epoch();
        self.shared.subs.lock().await.clear();
        *self.shared.session.lock().unwrap() = None;
    }
}

impl MobileClient {
    /// The transport-agnostic connect path (production injects the iroh
    /// [`Connector`]; a test or a future WebSocket transport injects its own). Not
    /// part of the FFI surface — it takes an `Arc<dyn Connector>`. Hands the pieces
    /// to the self-healing [`maintain_connection`](oximux_remote_session::maintain_connection)
    /// driver and returns once the first pairing lands (or fails); the driver then
    /// keeps the link alive across drops in the background.
    pub async fn connect_with(
        &self,
        connector: Arc<dyn Connector>,
        ticket: PairingTicket,
        device_name: String,
        listener: Arc<dyn ConnStateListener>,
    ) -> Result<(), MobileError> {
        // Supersede any prior connection: stop its driver and clear subs + session
        // so a reentrant connect (e.g. a double-tapped reconnect) never leaves two
        // drivers or stale sinks behind. The epoch bump makes any in-flight
        // `activate` from the old driver decline to publish.
        self.stop_current();
        self.shared.bump_epoch();
        self.shared.subs.lock().await.clear();
        *self.shared.session.lock().unwrap() = None;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        *self.shutdown.lock().unwrap() = Some(shutdown_tx);

        // Spawn the driver; it signals the first connection's outcome back here.
        let first = spawn_maintainer(
            self.shared.clone(),
            self.signer.clone(),
            connector,
            ticket,
            device_name,
            listener,
            shutdown_rx,
        );
        first.await.map_err(|_| MobileError::Transport("connection task ended".into()))?
    }

    /// Stop the current reconnect driver, if one is running.
    fn stop_current(&self) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}
