//! The connection maintainer: dials via a [`Connector`], establishes a session,
//! hands it to the app, and — when the link drops — reconnects per the
//! [`Reconnect`] policy (backing off through an injected [`Sleeper`]) until it
//! succeeds or the budget is spent.
//!
//! Runtime-agnostic and spawn-free: the demux pump is inlined into the maintain
//! future via `select`, so a plain executor drives it in tests and any runtime
//! hosts it in prod. A **lost link is observed as the pump future completing** —
//! the same signal the demux uses for host-close — so no separate liveness ping is
//! needed. `on_connected` hands the live [`RemoteSession`] to the app; the app's
//! RPCs are serviced because the pump keeps being polled inside this future until
//! the link drops or `shutdown` fires.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::{Either, select};

use crate::connector::Connector;
use crate::reconnect::{ConnAction, ConnState, Reconnect};
use crate::session::RemoteSession;
use crate::signer::ClientSigner;

/// Sleeps for the backoff between reconnect attempts. Injected so tests advance
/// instantly (and assert the schedule) while prod waits real wall-clock.
#[async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, dur: Duration);
}

/// Maintain a connection to the host until the reconnect budget is spent or
/// `shutdown` fires.
///
/// (Re)dials via `connector`, runs the [`RemoteSession`] handshake seeded with the
/// last `token`, hands the live session to `on_connected`, and reconnects on loss
/// per the [`Reconnect`] policy — backing off through `sleeper`. `on_state`
/// observes every transition for the UI.
pub async fn maintain_connection(
    connector: Arc<dyn Connector>,
    sleeper: Arc<dyn Sleeper>,
    signer: ClientSigner,
    mut token: Option<String>,
    mut shutdown: oneshot::Receiver<()>,
    mut on_state: impl FnMut(ConnState),
    mut on_connected: impl FnMut(Arc<RemoteSession>),
) {
    let mut policy = Reconnect::new();
    let mut action = policy.begin();
    on_state(policy.state().clone());

    loop {
        match action {
            ConnAction::Dial => {
                // A dial can block (the real iroh dial has no internal deadline),
                // so let `shutdown` cancel it.
                let dial = connector.connect();
                futures::pin_mut!(dial);
                let dialed = match select(dial, &mut shutdown).await {
                    Either::Left((res, _)) => res,
                    Either::Right(_) => return,
                };
                action = match dialed {
                    Err(e) => policy.on_dial_result(Err(e.to_string())),
                    Ok(transport) => {
                        let session = Arc::new(RemoteSession::new(transport, signer.clone()));
                        session.set_session_token(token.clone());
                        let pump = session.take_pump().expect("pump is taken once per session");
                        let mut pump_fut = Box::pin(pump.run());

                        // Drive the handshake and the pump together — the pump must
                        // run to route the handshake's reply. If the pump ends
                        // first, the link closed, but the reply may have been routed
                        // just before EOF; the pump's teardown resolves the
                        // handshake either way, so await it rather than assume it
                        // failed. Nothing bounds a silent host (the demux `call` has
                        // no timeout), so race `shutdown` here too — otherwise a host
                        // that accepts the dial then never replies would hang forever
                        // with the stop signal unobserved. (On the pump-ended arm the
                        // trailing `await` is bounded: the pump already finished, so
                        // its teardown resolves the handshake at once.)
                        let handshake = Box::pin(session.connect());
                        let (established, pump_ended) =
                            match select(select(handshake, pump_fut.as_mut()), &mut shutdown).await {
                                Either::Left((Either::Left((res, _)), _)) => (res.is_ok(), false),
                                Either::Left((Either::Right((_ended, handshake)), _)) => {
                                    (handshake.await.is_ok(), true)
                                }
                                Either::Right(_) => return,
                            };
                        // Refresh the reconnect token on every path: a successful
                        // handshake minted a fresh one; a failed one leaves it as-is.
                        token = session.session_token();

                        if !established {
                            policy.on_dial_result(Err("handshake failed".into()))
                        } else if pump_ended {
                            // The handshake landed but the link closed before the
                            // session was ever handed off — it never became usable.
                            // Count it as a FAILED attempt, not a lost-but-good one:
                            // routing it through `on_dial_result(Ok)` + `on_lost`
                            // would reset the retry counter every pass, so a host
                            // that answers-then-drops (a restart racing reconnect,
                            // load-shedding) would spin an unbounded zero-backoff
                            // redial that never surfaces `Unreachable`. Treating it
                            // as `Err` accrues backoff and honors the give-up budget.
                            // Don't re-poll the finished pump.
                            policy.on_dial_result(Err("link closed before the session became usable".into()))
                        } else {
                            let _ = policy.on_dial_result(Ok(()));
                            on_state(policy.state().clone());
                            on_connected(session.clone());
                            // Hold the live connection until the caller stops us or
                            // the pump ends (loss). `shutdown` is polled first so a
                            // stop wins a tie against a simultaneous drop.
                            match select(&mut shutdown, pump_fut.as_mut()).await {
                                Either::Left(_) => return,
                                Either::Right(_) => policy.on_lost(),
                            }
                        }
                    }
                };
            }
            ConnAction::Wait(delay) => {
                // Back off, but let `shutdown` cut the wait short.
                let nap = sleeper.sleep(delay);
                futures::pin_mut!(nap);
                match select(nap, &mut shutdown).await {
                    Either::Left(_) => action = policy.on_retry_elapsed(),
                    Either::Right(_) => return,
                }
            }
            ConnAction::GiveUp => {
                on_state(policy.state().clone());
                return;
            }
            ConnAction::Idle => return,
        }
        on_state(policy.state().clone());
    }
}
