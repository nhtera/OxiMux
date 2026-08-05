//! The terminal RPCs driven through the real dispatcher over the in-memory
//! loopback, against a scripted [`TerminalSource`].
//!
//! The load-bearing assertions are the two authorization tiers. Terminal attach
//! is the widest privilege this protocol grants — bytes into a live shell is
//! arbitrary code execution on the desktop — so "a read-only device can watch
//! but not type" and "a session-scoped device sees nothing at all" are the
//! properties that make the surface survivable, and they are asserted against
//! the real dispatcher rather than the ACL in isolation.

use std::sync::Arc;

use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{
    AuthStore, Dispatcher, PairingSlot, TerminalAttach, TerminalError, TerminalFrame,
    TerminalSource, registration_proof,
};
use oximux_remote_proto::Transport;
use oximux_remote_proto::messages::{RegisterReq, TerminalSummary};
use oximux_remote_proto::proto::{Request, Response, RpcError};
use oximux_remote_proto::testing::duplex_pair;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

const SECRET: [u8; 16] = [0x22; 16];
const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}

/// A terminal host whose live frames the test drives by hand, and which records
/// what was typed into it — so "the write gate held" is asserted by the shell
/// receiving nothing, not merely by the response code.
struct FakeTerminals {
    frames: Mutex<Option<mpsc::Receiver<TerminalFrame>>>,
    typed: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Frames handed to a *second* attach, so a detach/re-attach cycle can be
    /// driven without the fake running out of receivers.
    reattached: Mutex<Option<mpsc::Receiver<TerminalFrame>>>,
}

#[async_trait::async_trait]
impl TerminalSource for FakeTerminals {
    async fn list(&self) -> Result<Vec<TerminalSummary>, TerminalError> {
        Ok(vec![TerminalSummary {
            pty_id: "pty-1".into(),
            cwd: "/work".into(),
            cols: 80,
            rows: 24,
        }])
    }

    async fn attach(
        &self,
        pty_id: &str,
    ) -> Result<(TerminalAttach, mpsc::Receiver<TerminalFrame>), TerminalError> {
        if pty_id != "pty-1" {
            return Err(TerminalError::NotFound);
        }
        // Second attach draws from its own receiver, mirroring a real source
        // handing out a fresh subscription per attach.
        let rx = match self.frames.lock().await.take() {
            Some(rx) => rx,
            None => self.reattached.lock().await.take().ok_or(TerminalError::Unavailable)?,
        };
        Ok((
            TerminalAttach { replay: b"$ echo hi\r\nhi\r\n".to_vec(), cols: 80, rows: 24 },
            rx,
        ))
    }

    async fn input(&self, pty_id: &str, bytes: &[u8]) -> Result<(), TerminalError> {
        if pty_id != "pty-1" {
            return Err(TerminalError::NotFound);
        }
        self.typed.lock().await.push(bytes.to_vec());
        Ok(())
    }

    async fn resize(&self, _pty_id: &str, _cols: u16, _rows: u16) -> Result<(), TerminalError> {
        Ok(())
    }
}

async fn call(client: &dyn Transport, req: Request) -> Response {
    client.send(req.to_bytes().unwrap()).await.unwrap();
    let frame = client.recv().await.unwrap().expect("a response frame");
    Response::from_bytes(&frame).unwrap()
}

fn register_req(pubkey: [u8; 32], session: Option<&str>) -> RegisterReq {
    RegisterReq {
        app_pubkey: pubkey,
        device_name: "phone".into(),
        proof: registration_proof(&SECRET, &pubkey, NOW),
        timestamp_secs: NOW,
        session_id: session.map(Into::into),
    }
}

struct Harness {
    dispatcher: Dispatcher,
    auth: Arc<AuthStore>,
    typed: Arc<Mutex<Vec<Vec<u8>>>>,
    tx: mpsc::Sender<TerminalFrame>,
}

fn harness(pairing_session: Option<&str>) -> Harness {
    let (tx, rx) = mpsc::channel(8);
    let typed = Arc::new(Mutex::new(Vec::new()));
    let terminals = Arc::new(FakeTerminals {
        frames: Mutex::new(Some(rx)),
        typed: Arc::clone(&typed),
        reattached: Mutex::new(None),
    });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, pairing_session.map(Into::into), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_terminals(terminals);
    Harness { dispatcher, auth, typed, tx }
}

/// The happy path: list, attach with replay, receive pushed output, and type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_access_device_lists_attaches_and_types() {
    let h = harness(None);
    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);
    let typed = Arc::clone(&h.typed);
    let tx = h.tx.clone();

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, None))).await
        else {
            panic!("expected Registered");
        };

        let Response::Terminals(rows) = call(&client, Request::ListTerminals).await else {
            panic!("expected Terminals");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pty_id, "pty-1");

        let Response::TermAttached { replay, cols, rows: r } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected TermAttached");
        };
        assert!(
            String::from_utf8_lossy(&replay).contains("hi"),
            "the replay ring comes back with the attach",
        );
        assert_eq!(
            (cols, r),
            (80, 24),
            "the dims ride along — replay bytes only land correctly in the grid that drew them",
        );

        // A live frame pushed after the attach reaches the client unsolicited.
        tx.send(TerminalFrame::Output(b"live\r\n".to_vec())).await.unwrap();
        let frame = client.recv().await.unwrap().expect("a pushed frame");
        let Response::TermOutput { pty_id, bytes } = Response::from_bytes(&frame).unwrap() else {
            panic!("expected a pushed TermOutput");
        };
        assert_eq!(pty_id, "pty-1");
        assert_eq!(bytes, b"live\r\n");

        let ack = call(&client, Request::TermInput {
            pty_id: "pty-1".into(),
            bytes: b"ls\n".to_vec(),
        })
        .await;
        assert_eq!(ack, Response::Ack);
    };

    futures::future::join(serve, script).await;
    assert_eq!(
        typed.lock().await.as_slice(),
        &[b"ls\n".to_vec()],
        "the keystrokes actually reached the terminal",
    );
}

/// A read-only device watches a terminal it cannot drive.
///
/// The refusal is asserted at the shell, not only on the wire: a response that
/// said `Unauthorized` while still delivering the bytes would look identical to
/// the client and be a total compromise of the tier.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_device_watches_but_cannot_type() {
    let h = harness(None);
    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let auth = Arc::clone(&h.auth);
    let typed = Arc::clone(&h.typed);
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, None))).await
        else {
            panic!("expected Registered");
        };
        auth.set_read_only(&pubkey, true);

        // Reads still work.
        let Response::Terminals(rows) = call(&client, Request::ListTerminals).await else {
            panic!("read-only still lists terminals");
        };
        assert_eq!(rows.len(), 1);
        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("read-only still attaches");
        };

        // Writes do not.
        let typed_resp = call(&client, Request::TermInput {
            pty_id: "pty-1".into(),
            bytes: b"rm -rf /\n".to_vec(),
        })
        .await;
        assert_eq!(typed_resp, Response::Error(RpcError::Unauthorized), "typing refused");

        let resized = call(&client, Request::TermResize {
            pty_id: "pty-1".into(),
            cols: 40,
            rows: 12,
        })
        .await;
        assert_eq!(
            resized,
            Response::Error(RpcError::Unauthorized),
            "resize refused too — the size is shared with the desktop's own window",
        );
    };

    futures::future::join(serve, script).await;
    assert!(
        typed.lock().await.is_empty(),
        "nothing reached the shell — the gate stopped the bytes, not just the reply",
    );
}

/// A session-scoped device gets no terminals at all, not a filtered list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_scoped_device_is_refused_terminals() {
    let h = harness(Some("sess-1"));
    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, Some("sess-1")))).await
        else {
            panic!("expected Registered");
        };

        assert_eq!(
            call(&client, Request::ListTerminals).await,
            Response::Error(RpcError::Unauthorized),
            "a narrowed device cannot even enumerate terminals",
        );
        assert_eq!(
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await,
            Response::Error(RpcError::Unauthorized),
        );
    };

    futures::future::join(serve, script).await;
}

/// A host with NO terminal source answers `Unsupported`, not `Unauthorized`.
///
/// The distinction is the whole diagnosis. A headless `oximux serve` that could
/// not start a relay serves everything except terminals — documented as normal
/// in `docs/server-install.md` — and it used to report that as an access
/// refusal, which the CLI renders with next-steps about `$OXIMUX_SESSION_ID`.
/// An operator on a fresh server hits this on their first `term ls` and goes
/// hunting for a credential problem that does not exist.
///
/// Asserted for a FULL-access peer specifically: with anything narrower the
/// authorization tier above would refuse first and this would pass without
/// testing anything. Every other optional service on the dispatcher (team,
/// heartbeats, worktrees, schedules, state, pairing admin) already answers
/// absence this way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_without_terminals_reports_unsupported_not_unauthorized() {
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    // The one difference from `harness()`: no `.with_terminals(…)` at all.
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock);
    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, None))).await
        else {
            panic!("expected Registered");
        };

        for (what, req) in [
            ("list", Request::ListTerminals),
            ("attach", Request::TermAttach { pty_id: "pty-1".into() }),
            ("input", Request::TermInput { pty_id: "pty-1".into(), bytes: b"x".to_vec() }),
            ("resize", Request::TermResize { pty_id: "pty-1".into(), cols: 80, rows: 24 }),
        ] {
            assert_eq!(
                call(&client, req).await,
                Response::Error(RpcError::Unsupported),
                "terminal {what} on a host with no terminal source is a capability \
                 answer, not an access refusal",
            );
        }
    };

    futures::future::join(serve, script).await;
}

/// A gap is forwarded rather than swallowed, so the client knows to re-attach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_frame_reaches_the_client_as_a_gap() {
    let h = harness(None);
    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);
    let tx = h.tx.clone();

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, None))).await
        else {
            panic!("expected Registered");
        };
        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected TermAttached");
        };

        tx.send(TerminalFrame::Gapped).await.unwrap();
        let frame = client.recv().await.unwrap().expect("a pushed frame");
        assert_eq!(
            Response::from_bytes(&frame).unwrap(),
            Response::TermGapped { pty_id: "pty-1".into() },
            "the host's own data loss is reported, not hidden behind a continuous stream",
        );
    };

    futures::future::join(serve, script).await;
}

/// A detach must actually STOP the stream, not merely forget it.
///
/// `SelectAll` cannot remove one of its streams, so a detach that only dropped
/// the bookkeeping entry would leave the old stream forwarding while the next
/// attach stacked a second one beside it. Every byte would then arrive twice —
/// and three times after the next cycle — which on screen is doubled characters
/// rather than anything that reads as a leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_attaching_after_a_detach_does_not_double_the_output() {
    let (tx_a, rx_a) = mpsc::channel(8);
    let (tx_b, rx_b) = mpsc::channel(8);
    let typed = Arc::new(Mutex::new(Vec::new()));
    let terminals = Arc::new(FakeTerminals {
        frames: Mutex::new(Some(rx_a)),
        typed: Arc::clone(&typed),
        reattached: Mutex::new(Some(rx_b)),
    });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_terminals(terminals);

    let pubkey = [0x33; 32];
    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req(pubkey, None))).await
        else {
            panic!("expected Registered");
        };

        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected TermAttached");
        };

        // Detach, then attach again — the mount/unmount/remount a client does
        // when the user leaves a terminal screen and comes back.
        assert_eq!(
            call(&client, Request::TermDetach { pty_id: "pty-1".into() }).await,
            Response::Ack,
        );
        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected a second TermAttached");
        };

        // The detached stream must be GONE, not merely forgotten. Cancelling it
        // drops its receiver, so the source's next send fails — which is also
        // what tells a real source to release its upstream subscription. If the
        // stream were only forgotten this send would succeed and STALE would
        // arrive below, ahead of the live stream's frames.
        assert!(
            tx_a.send(TerminalFrame::Output(b"STALE".to_vec())).await.is_err(),
            "the detached stream's receiver is gone, so its source stops feeding it",
        );

        tx_b.send(TerminalFrame::Output(b"once".to_vec())).await.unwrap();
        let frame = client.recv().await.unwrap().expect("a pushed frame");
        assert_eq!(
            Response::from_bytes(&frame).unwrap(),
            Response::TermOutput { pty_id: "pty-1".into(), bytes: b"once".to_vec() },
        );

        // The next frame must be the live stream's, never the detached one's —
        // that ordering is the assertion. A detached stream that still forwards
        // would have delivered STALE ahead of this.
        tx_b.send(TerminalFrame::Output(b"twice".to_vec())).await.unwrap();
        let frame = client.recv().await.unwrap().expect("a second pushed frame");
        assert_eq!(
            Response::from_bytes(&frame).unwrap(),
            Response::TermOutput { pty_id: "pty-1".into(), bytes: b"twice".to_vec() },
            "the detached stream is silent — it was stopped, not merely forgotten",
        );
    };

    futures::future::join(serve, script).await;
}
