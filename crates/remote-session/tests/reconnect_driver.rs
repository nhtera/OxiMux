//! The `maintain_connection` driver end-to-end: it dials via a [`Connector`],
//! runs the real handshake + demux pump, and reconnects on loss per the backoff
//! policy. Two scripted worlds drive it deterministically with no network and no
//! real time — a scripted host that completes a handshake then drops the link, and
//! a connector that only ever fails.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::future::{Either, join, select};
use oximux_remote_proto::proto::Response;
use oximux_remote_proto::{PairingTicket, Transport};
use oximux_remote_proto::testing::{DuplexTransport, duplex_pair};
use oximux_remote_session::{
    Bootstrap, ClientSigner, ConnState, Connector, ConnectError, Sleeper, maintain_connection,
};

const SEED: [u8; 32] = [7u8; 32];

/// Records the backoff delays it is asked to wait, and returns instantly — so a
/// test asserts the schedule without spending real time.
struct RecordingSleeper {
    delays: Arc<std::sync::Mutex<Vec<Duration>>>,
}
#[async_trait]
impl Sleeper for RecordingSleeper {
    async fn sleep(&self, dur: Duration) {
        self.delays.lock().unwrap().push(dur);
    }
}

/// Hands out pre-made transports in order; once the queue is empty every dial
/// fails, modelling an unreachable host.
struct QueueConnector {
    queue: std::sync::Mutex<VecDeque<Arc<dyn Transport>>>,
    dials: AtomicU32,
}
#[async_trait]
impl Connector for QueueConnector {
    async fn connect(&self) -> Result<Arc<dyn Transport>, ConnectError> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        match self.queue.lock().unwrap().pop_front() {
            Some(t) => Ok(t),
            None => Err(ConnectError::Unreachable("queue drained".into())),
        }
    }
}

/// Play the host side of one reconnect handshake: read the client's `Connect` and
/// answer `Connected` with a fresh token (the fast-path branch the driver takes).
async fn answer_connect(server: &DuplexTransport, token: &str) {
    server.recv().await.expect("recv ok").expect("a Connect request");
    let reply = Response::Connected { session_token: token.into() }.to_bytes().unwrap();
    server.send(reply).await.expect("send Connected");
}

/// Play the host side of a first-time pairing: read the client's `Register` and
/// answer `Registered` with a fresh token.
async fn answer_register(server: &DuplexTransport, token: &str) {
    server.recv().await.expect("recv ok").expect("a Register request");
    let reply = Response::Registered { session_token: token.into() }.to_bytes().unwrap();
    server.send(reply).await.expect("send Registered");
}

#[test]
fn driver_backs_off_then_gives_up_when_the_host_is_unreachable() {
    let delays = Arc::new(std::sync::Mutex::new(Vec::new()));
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::new()), // every dial fails
        dials: AtomicU32::new(0),
    });
    let sleeper = Arc::new(RecordingSleeper { delays: delays.clone() });

    let states = Rc::new(RefCell::new(Vec::new()));
    let states_seen = states.clone();
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();

    block_on(maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Resume,
        shutdown_rx,
        move |s| states_seen.borrow_mut().push(s),
        |_session| unreachable!("never connects"),
    ));

    // 3 backoff waits (1s, 2s, 4s), then give up on the 4th failed dial.
    assert_eq!(*delays.lock().unwrap(), vec![secs(1), secs(2), secs(4)]);
    assert_eq!(connector.dials.load(Ordering::SeqCst), 4);
    assert!(
        matches!(states.borrow().last(), Some(ConnState::Unreachable { .. })),
        "settles Unreachable, saw {:?}",
        states.borrow(),
    );
}

#[test]
fn driver_reconnects_after_the_link_drops() {
    let (c1, s1) = duplex_pair();
    let (c2, s2) = duplex_pair();
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::from([
            Arc::new(c1) as Arc<dyn Transport>,
            Arc::new(c2) as Arc<dyn Transport>,
        ])),
        dials: AtomicU32::new(0),
    });
    let sleeper = Arc::new(RecordingSleeper { delays: Arc::new(std::sync::Mutex::new(Vec::new())) });

    let connected = Rc::new(RefCell::new(0u32));
    let connects_seen = connected.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (drop1_tx, drop1_rx) = oneshot::channel();
    // On the 1st connect signal the host to drop the link; on the 2nd, stop.
    let signals = Rc::new(RefCell::new((Some(drop1_tx), Some(shutdown_tx))));

    // The host: hand-shake conn 1 and keep it live until the driver is connected,
    // then drop it (a network drop); hand-shake conn 2 and hold until shutdown.
    let host = async move {
        answer_connect(&s1, "tok-1").await;
        drop1_rx.await.ok(); // wait until the driver reports conn 1 connected
        drop(s1); // link drops → the client's pump ends → the driver reconnects
        answer_connect(&s2, "tok-2").await;
        let _ = s2.recv().await; // ends when the driver returns and drops session 2
    };

    let driver = maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Resume,
        shutdown_rx,
        |_state| {},
        move |_session| {
            *connects_seen.borrow_mut() += 1;
            let mut signals = signals.borrow_mut();
            if *connects_seen.borrow() == 1 {
                // Connected — tell the host to sever the link so we reconnect.
                if let Some(tx) = signals.0.take() {
                    let _ = tx.send(());
                }
            } else if *connects_seen.borrow() == 2 {
                // Reconnected — stop.
                if let Some(tx) = signals.1.take() {
                    let _ = tx.send(());
                }
            }
        },
    );

    block_on(join(host, driver));

    assert_eq!(*connected.borrow(), 2, "connected, lost the link, then reconnected");
    assert_eq!(connector.dials.load(Ordering::SeqCst), 2, "exactly two dials, no wasted retries");
}

/// The mobile bootstrap: the first dial pairs (`Register`); after the link drops
/// the driver must reconnect via `Connect` (never re-consuming the QR secret). If
/// the driver wrongly re-ran `Register`, `session.connect()`'s reply-shape check
/// would reject the host's `Connected`, the reconnect would fail, and only one
/// connect would land — so the two-connect assertion is the real proof.
#[test]
fn driver_pairs_on_the_first_dial_then_reconnects_via_connect() {
    let (c1, s1) = duplex_pair();
    let (c2, s2) = duplex_pair();
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::from([
            Arc::new(c1) as Arc<dyn Transport>,
            Arc::new(c2) as Arc<dyn Transport>,
        ])),
        dials: AtomicU32::new(0),
    });
    let sleeper = Arc::new(RecordingSleeper { delays: Arc::new(std::sync::Mutex::new(Vec::new())) });

    let connected = Rc::new(RefCell::new(0u32));
    let connects_seen = connected.clone();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (drop1_tx, drop1_rx) = oneshot::channel();
    let signals = Rc::new(RefCell::new((Some(drop1_tx), Some(shutdown_tx))));

    // Host: pair (Register) conn 1, hold until connected, drop it; then answer the
    // reconnect as a Connect on conn 2 — proving the driver switched off Register.
    let host = async move {
        answer_register(&s1, "tok-1").await;
        drop1_rx.await.ok();
        drop(s1);
        answer_connect(&s2, "tok-2").await;
        let _ = s2.recv().await;
    };

    let ticket =
        PairingTicket { endpoint_id: [0u8; 32], handshake_secret: [1u8; 16], session_id: None };
    let driver = maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Pair { ticket, device_name: "phone".into(), at_secs: 1000 },
        shutdown_rx,
        |_state| {},
        move |_session| {
            *connects_seen.borrow_mut() += 1;
            let mut signals = signals.borrow_mut();
            // 1st connect (paired): sever the link so we reconnect. 2nd (reconnected): stop.
            let tx = match *connects_seen.borrow() {
                1 => signals.0.take(),
                2 => signals.1.take(),
                _ => None,
            };
            if let Some(tx) = tx {
                let _ = tx.send(());
            }
        },
    );
    block_on(join(host, driver));

    assert_eq!(*connected.borrow(), 2, "paired, lost the link, then reconnected via Connect");
    assert_eq!(connector.dials.load(Ordering::SeqCst), 2, "exactly two dials");
}

/// Fires a shutdown the first time it is asked to sleep, then never completes —
/// so the driver must observe the shutdown *during* the backoff wait, not by the
/// sleep finishing.
struct ShutdownDuringBackoff {
    shutdown: Arc<std::sync::Mutex<Option<oneshot::Sender<()>>>>,
}
#[async_trait]
impl Sleeper for ShutdownDuringBackoff {
    async fn sleep(&self, _dur: Duration) {
        if let Some(tx) = self.shutdown.lock().unwrap().take() {
            let _ = tx.send(());
        }
        futures::future::pending::<()>().await; // the sleep itself never returns
    }
}

#[test]
fn driver_shutdown_cuts_a_backoff_short() {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let sleeper = Arc::new(ShutdownDuringBackoff {
        shutdown: Arc::new(std::sync::Mutex::new(Some(shutdown_tx))),
    });
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::new()), // every dial fails
        dials: AtomicU32::new(0),
    });

    let states = Rc::new(RefCell::new(Vec::new()));
    let states_seen = states.clone();
    block_on(maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Resume,
        shutdown_rx,
        move |s| states_seen.borrow_mut().push(s),
        |_session| unreachable!("never connects"),
    ));

    // One failed dial → backoff begins → shutdown fires mid-wait → the driver
    // returns instead of dialing again or grinding to Unreachable.
    assert_eq!(connector.dials.load(Ordering::SeqCst), 1, "no dial after shutdown cut the wait");
    assert!(
        matches!(states.borrow().last(), Some(ConnState::WaitingToRetry { .. })),
        "stopped mid-backoff, not Unreachable, saw {:?}",
        states.borrow(),
    );
}

/// A host that completes the handshake then severs the link *before* the driver
/// hands the session off — the "connected but never usable" path. Every attempt
/// must count as a failure so backoff accrues and the give-up budget is honored;
/// a regression would spin an unbounded zero-backoff redial that never settles.
#[test]
fn driver_gives_up_when_the_host_answers_then_drops_every_time() {
    // 4 fresh transports (3 retries + the give-up dial); the host answers each
    // handshake then drops at once, so the client's pump ends before hand-off.
    let mut clients: Vec<Arc<dyn Transport>> = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..4 {
        let (c, s) = duplex_pair();
        clients.push(Arc::new(c));
        servers.push(s);
    }
    let delays = Arc::new(std::sync::Mutex::new(Vec::new()));
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::from(clients)),
        dials: AtomicU32::new(0),
    });
    let sleeper = Arc::new(RecordingSleeper { delays: delays.clone() });

    let states = Rc::new(RefCell::new(Vec::new()));
    let states_seen = states.clone();
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();

    let host = async move {
        for s in servers {
            answer_connect(&s, "tok").await;
            drop(s); // sever right after the reply → pump ends before hand-off
        }
    };
    let driver = maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Resume,
        shutdown_rx,
        move |s| states_seen.borrow_mut().push(s),
        |_session| unreachable!("the session never becomes usable"),
    );
    block_on(join(host, driver));

    // Backoff accrued (not the pre-fix empty schedule), then settled Unreachable.
    assert_eq!(*delays.lock().unwrap(), vec![secs(1), secs(2), secs(4)], "backoff accrues");
    assert_eq!(connector.dials.load(Ordering::SeqCst), 4, "4 dials then give up");
    assert!(
        matches!(states.borrow().last(), Some(ConnState::Unreachable { .. })),
        "settles Unreachable, saw {:?}",
        states.borrow(),
    );
}

/// A host that accepts the dial then goes silent mid-handshake — never replying,
/// never closing. `shutdown` must still abort the driver promptly; nothing bounds
/// the handshake RPC, so a regression would hang forever with the stop unobserved.
#[test]
fn driver_shutdown_aborts_a_stuck_handshake() {
    let (c1, s1) = duplex_pair();
    let connector = Arc::new(QueueConnector {
        queue: std::sync::Mutex::new(VecDeque::from([Arc::new(c1) as Arc<dyn Transport>])),
        dials: AtomicU32::new(0),
    });
    let sleeper = Arc::new(RecordingSleeper { delays: Arc::new(std::sync::Mutex::new(Vec::new())) });
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Accept the dial, read the client's Connect (handshake now outstanding), then
    // fire shutdown and hold the link open forever without ever replying.
    let host = async move {
        s1.recv().await.expect("recv ok").expect("a Connect request");
        let _ = shutdown_tx.send(());
        futures::future::pending::<()>().await; // never reply, never close
    };
    let driver = maintain_connection(
        connector.clone(),
        sleeper,
        ClientSigner::from_seed(&SEED),
        None,
        Bootstrap::Resume,
        shutdown_rx,
        |_state| {},
        |_session| unreachable!("the handshake never completes"),
    );

    // The host is `pending` forever, so completion can only come from the driver
    // returning on shutdown — a regression that ignores shutdown mid-handshake
    // would leave both futures parked and hang the test.
    let done = block_on(select(Box::pin(driver), Box::pin(host)));
    assert!(matches!(done, Either::Left(_)), "driver returned on shutdown");
    assert_eq!(connector.dials.load(Ordering::SeqCst), 1, "one dial, then aborted");
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}
