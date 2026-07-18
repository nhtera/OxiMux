//! [`MobileClient`] — the handle the app holds: connect to a paired host, then
//! drive sessions. This module owns construction + the connection lifecycle; the
//! async RPCs live in [`rpc`](crate::client::rpc) and subscription in
//! [`subscription`](crate::subscription).

use std::collections::HashMap;
use std::sync::Arc;

use oximux_remote_iroh::{IrohConnector, bind_client};
use oximux_remote_proto::PairingTicket;
use oximux_remote_proto::transport::Transport;
use oximux_remote_session::{ClientSigner, Connector, RemoteSession};
use tokio::sync::Mutex;

use crate::callbacks::ConnStateListener;
use crate::ffi_types::{ConnState, MobileError};
use crate::runtime::{now_secs, rt};
use crate::subscription::{Sub, run_dispatcher};

mod rpc;

/// State shared between the client handle and its background tasks (the demux
/// pump feeds events; the dispatcher folds them into the registered [`Sub`]s).
pub(crate) struct Shared {
    pub session: Mutex<Option<Arc<RemoteSession>>>,
    pub subs: Mutex<HashMap<String, Sub>>,
}

impl Shared {
    /// The live session, or [`MobileError::NotConnected`].
    pub(crate) async fn session(&self) -> Result<Arc<RemoteSession>, MobileError> {
        self.session.lock().await.clone().ok_or(MobileError::NotConnected)
    }
}

/// The phone's client object. Construct with [`MobileClient::new`], then
/// [`connect`](MobileClient::connect).
#[derive(uniffi::Object)]
pub struct MobileClient {
    signer: ClientSigner,
    pub(crate) shared: Arc<Shared>,
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
                session: Mutex::new(None),
                subs: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Pair with and connect to the host named by a scanned
    /// `oximux://connect?ticket=…` deep link, over iroh. On success the session is
    /// live and `listener` has seen [`ConnState::Connected`].
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

    /// Drop the connection; background tasks wind down as the session is released.
    pub async fn disconnect(&self) {
        self.shared.subs.lock().await.clear();
        *self.shared.session.lock().await = None;
    }
}

impl MobileClient {
    /// The transport-agnostic connect path (production injects the iroh
    /// [`Connector`]; a test or a future WebSocket transport injects its own).
    /// Not part of the FFI surface — it takes an `Arc<dyn Connector>`. Dials, spins
    /// up the demux pump + event dispatcher, then registers the device via `pair`.
    pub async fn connect_with(
        &self,
        connector: Arc<dyn Connector>,
        ticket: PairingTicket,
        device_name: String,
        listener: Arc<dyn ConnStateListener>,
    ) -> Result<(), MobileError> {
        listener.on_state(ConnState::Connecting);
        // Reset any prior connection first: dropping the old session ends its pump
        // (via the demux shutdown), and clearing subs prevents a reentrant connect
        // (e.g. a double-tapped reconnect) from leaving two pumps or stale sinks.
        self.shared.subs.lock().await.clear();
        *self.shared.session.lock().await = None;

        let transport: Arc<dyn Transport> =
            connector.connect().await.map_err(|e| MobileError::Transport(e.to_string()))?;
        let session = Arc::new(RemoteSession::new(transport, self.signer.clone()));

        // Drive the demux + the event dispatcher on the core-owned runtime so they
        // outlive this call. `take_*` are once-per-session; unwrap is sound here.
        let pump = session.take_pump().expect("pump taken once");
        let events = session.take_events().expect("events taken once");
        rt().spawn(async move {
            let _ = pump.run().await;
        });

        // Register (or re-authorize) this device with the QR secret.
        session
            .pair(&ticket, &device_name, now_secs())
            .await
            .map_err(|e| MobileError::Pairing(e.to_string()))?;

        *self.shared.session.lock().await = Some(session);
        rt().spawn(run_dispatcher(self.shared.clone(), events));
        listener.on_state(ConnState::Connected);
        Ok(())
    }
}
