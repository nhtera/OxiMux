//! Wiring the pure-Rust [`maintain_connection`] driver into the FFI client: it
//! ties the injected [`Connector`], the reconnect policy, and the demux pump into
//! one self-healing loop. The driver owns the connection lifecycle — pairing on
//! the first dial, reconnecting on every drop — and reports each transition here
//! so the UI reflects it and the very first outcome flows back to `connect_with`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::oneshot;
use oximux_remote_session::{
    Bootstrap, ClientSigner, ConnState as RsConnState, Connector, RemoteSession, Sleeper,
    maintain_connection,
};

use super::Shared;
use crate::callbacks::ConnStateListener;
use crate::ffi_types::{ConnState, MobileError};
use crate::runtime::rt;
use crate::subscription::{FirstResult, activate};

/// Backs the driver's reconnect backoff with real wall-clock sleeps.
struct TokioSleeper;
#[async_trait]
impl Sleeper for TokioSleeper {
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Spawn the self-healing connection driver and return a receiver for the first
/// connection's outcome. The driver keeps running after that — reconnecting on
/// each drop — until `shutdown_rx` fires.
pub(crate) fn spawn_maintainer(
    shared: Arc<Shared>,
    signer: ClientSigner,
    connector: Arc<dyn Connector>,
    bootstrap: Bootstrap,
    listener: Arc<dyn ConnStateListener>,
    shutdown_rx: oneshot::Receiver<()>,
) -> oneshot::Receiver<Result<(), MobileError>> {
    let (first_tx, first_rx) = oneshot::channel();
    let first: FirstResult = Arc::new(StdMutex::new(Some(first_tx)));

    // The epoch this driver currently owns. Starts at the value `start` bumped to
    // just before spawning us, and tracks each of our own reconnects. A newer
    // driver bumps past it, which is how we recognize that we've been superseded.
    let my_epoch = Arc::new(AtomicU64::new(shared.epoch.load(Ordering::Acquire)));

    let on_state = {
        let shared = shared.clone();
        let first = first.clone();
        let my_epoch = my_epoch.clone();
        move |state: RsConnState| {
            // Drop the live session whenever we're not Connected so an RPC during a
            // reconnect gap fails fast with `NotConnected` rather than stalling on a
            // dead pump (or racing the swap).
            //
            // Only while this driver is still the current one. `stop_current`
            // signals the old driver asynchronously, so a superseded driver can
            // report a shutdown transition after its replacement has already
            // connected and published — and an unguarded clear here would drop a
            // live session belonging to someone else, leaving every RPC failing
            // `NotConnected` against a connection the UI shows as up.
            //
            // Guarding the publish but not the clear was an asymmetry: `activate`
            // has always checked the epoch. This closes it. The window is narrow
            // and the loopback harness does not reproduce it — the superseded
            // driver has usually exited before the replacement dials — so this is
            // reasoned, not observed.
            if !matches!(state, RsConnState::Connected)
                && shared.epoch.load(Ordering::Acquire) == my_epoch.load(Ordering::Acquire)
            {
                *shared.session.lock().unwrap() = None;
            }
            // The host is unreachable before the first connection ever landed —
            // surface it as the `connect_with` failure.
            if let RsConnState::Unreachable { cause } = &state
                && let Some(tx) = first.lock().unwrap().take()
            {
                let _ = tx.send(Err(MobileError::Transport(cause.clone())));
            }
            listener.on_state(map_state(&state));
        }
    };

    let on_connected = move |session: Arc<RemoteSession>| {
        // Claim the new epoch for this driver too, so its own later transitions
        // (a drop, a retry) still recognize themselves as current.
        // Mark this connection the current epoch, then store + wire it up
        // off-thread so the driver keeps polling the pump — which services the
        // re-subscribe RPCs `activate` issues — instead of blocking the driver
        // loop here. `activate` only publishes if this epoch is still current, so
        // a superseding reconnect/disconnect can't be overwritten by a slow one.
        let epoch = shared.bump_epoch();
        my_epoch.store(epoch, Ordering::Release);
        rt().spawn(activate(shared.clone(), session, first.clone(), epoch));
    };

    rt().spawn(maintain_connection(
        connector,
        Arc::new(TokioSleeper),
        signer,
        None,
        bootstrap,
        shutdown_rx,
        on_state,
        on_connected,
    ));
    first_rx
}

/// Project the reconnect policy's state onto the FFI enum the UI renders.
fn map_state(state: &RsConnState) -> ConnState {
    match state {
        RsConnState::Disconnected => ConnState::Disconnected,
        RsConnState::Connecting => ConnState::Connecting,
        RsConnState::Connected => ConnState::Connected,
        RsConnState::WaitingToRetry { .. } => ConnState::Reconnecting,
        RsConnState::Unreachable { cause } => ConnState::Unreachable { cause: cause.clone() },
    }
}
