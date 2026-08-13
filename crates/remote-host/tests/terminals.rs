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
    AttachmentId, AuthStore, Dispatcher, PairingSlot, TerminalAttach, TerminalError, TerminalFrame,
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
    /// Mints a distinct attachment per attach, like a real host does.
    next_attachment: Mutex<u64>,
    /// Every resize with the attachment it named, so "each device resized its
    /// own" is asserted against what the PTY layer was actually told.
    resizes: Arc<Mutex<Vec<(AttachmentId, u16, u16)>>>,
    /// Attachments handed back, in order.
    released: Arc<Mutex<Vec<AttachmentId>>>,
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
        let attachment = {
            let mut next = self.next_attachment.lock().await;
            *next += 1;
            AttachmentId(*next)
        };
        Ok((
            TerminalAttach {
                replay: b"$ echo hi\r\nhi\r\n".to_vec(),
                cols: 80,
                rows: 24,
                attachment,
            },
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

    async fn resize(
        &self,
        _pty_id: &str,
        attachment: AttachmentId,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalError> {
        self.resizes.lock().await.push((attachment, cols, rows));
        Ok(())
    }

    async fn detach(&self, _pty_id: &str, attachment: AttachmentId) {
        self.released.lock().await.push(attachment);
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
    resizes: Arc<Mutex<Vec<(AttachmentId, u16, u16)>>>,
    released: Arc<Mutex<Vec<AttachmentId>>>,
    tx: mpsc::Sender<TerminalFrame>,
}

fn harness(pairing_session: Option<&str>) -> Harness {
    let (tx, rx) = mpsc::channel(8);
    let typed = Arc::new(Mutex::new(Vec::new()));
    let resizes = Arc::new(Mutex::new(Vec::new()));
    let released = Arc::new(Mutex::new(Vec::new()));
    let terminals = Arc::new(FakeTerminals {
        frames: Mutex::new(Some(rx)),
        typed: Arc::clone(&typed),
        // A second receiver so two devices can attach to the one terminal.
        reattached: Mutex::new(Some(mpsc::channel(8).1)),
        next_attachment: Mutex::new(0),
        resizes: Arc::clone(&resizes),
        released: Arc::clone(&released),
    });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, pairing_session.map(Into::into), false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), Arc::clone(&auth))
        .with_clock(clock)
        .with_terminals(terminals);
    Harness { dispatcher, auth, typed, resizes, released, tx }
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
        next_attachment: Mutex::new(0),
        resizes: Arc::new(Mutex::new(Vec::new())),
        released: Arc::new(Mutex::new(Vec::new())),
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

/// Two devices watching ONE terminal each resize the attachment they opened.
///
/// The host runs a shared PTY at the smallest grid any attachment asks for, so a
/// resize is a statement about one viewer's window. Nothing in the request names
/// which — the terminal id is the same for both — so the attachment has to come
/// from the connection's own record. Resolving it from the PTY instead (a map
/// keyed by terminal, in a source shared by every paired device) makes the last
/// device to attach the owner of every later resize: the second phone to open a
/// terminal silently drives the first one's window, and the first phone's own
/// attachment keeps whatever size it had forever, because nothing addresses it
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_devices_on_one_terminal_each_resize_their_own_attachment() {
    let h = harness(None);
    let resizes = Arc::clone(&h.resizes);
    let auth = Arc::clone(&h.auth);
    let (client_a, server_a) = duplex_pair();
    let (client_b, server_b) = duplex_pair();
    let serve_a = h.dispatcher.serve(&server_a);
    let serve_b = h.dispatcher.serve(&server_b);

    let script = async move {
        // Two paired devices, two connections, one terminal. A attaches first,
        // so the fake mints it attachment 1 and B attachment 2.
        // Both keys have to be valid curve points — registration rejects a
        // structurally invalid pubkey before it ever reaches the pairing proof.
        for (client, pubkey) in [(&client_a, [0x33; 32]), (&client_b, [0x55; 32])] {
            // A pairing slot is consumed by the device that uses it, so the
            // second phone pairs through its own window — as it does in reality.
            auth.set_pairing(PairingSlot::new(SECRET, None, false));
            let registered = call(client, Request::Register(register_req(pubkey, None))).await;
            let Response::Registered { .. } = registered else {
                panic!("expected Registered, got {registered:?}");
            };
            let Response::TermAttached { .. } =
                call(client, Request::TermAttach { pty_id: "pty-1".into() }).await
            else {
                panic!("expected TermAttached");
            };
        }

        for (client, cols, rows) in [(&client_a, 40, 12), (&client_b, 100, 30)] {
            assert_eq!(
                call(client, Request::TermResize { pty_id: "pty-1".into(), cols, rows }).await,
                Response::Ack,
            );
        }
    };

    futures::future::join3(serve_a, serve_b, script).await;
    assert_eq!(
        resizes.lock().await.as_slice(),
        [(AttachmentId(1), 40, 12), (AttachmentId(2), 100, 30)],
        "each resize named the attachment its own connection opened",
    );
}

/// Resizing a terminal this connection never attached to is refused.
///
/// There is nothing to name yet, and the alternative is worse than an error: a
/// resize resolved from the PTY would land on some *other* device's attachment
/// and shrink a window nobody asked to shrink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_without_an_attachment_is_refused() {
    let h = harness(None);
    let resizes = Arc::clone(&h.resizes);
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32], None))).await
        else {
            panic!("expected Registered");
        };
        assert_eq!(
            call(&client, Request::TermResize { pty_id: "pty-1".into(), cols: 40, rows: 12 }).await,
            Response::Error(RpcError::UnknownSession),
        );
    };

    futures::future::join(serve, script).await;
    assert!(resizes.lock().await.is_empty(), "nothing reached the PTY layer");
}

/// Detaching hands the attachment back, rather than only stopping the stream.
///
/// Those are two different resources. The stream is this connection's view; the
/// attachment is a claim on the terminal itself, including a standing vote on
/// its size. A host that runs a terminal at the smallest size any attachment
/// asks for goes on honouring a departed viewer's vote until it is withdrawn —
/// so a phone that shrank a terminal and then closed the screen would leave the
/// desktop's window narrow with nothing left that could widen it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detaching_hands_the_attachment_back() {
    let h = harness(None);
    let released = Arc::clone(&h.released);
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32], None))).await
        else {
            panic!("expected Registered");
        };
        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected TermAttached");
        };
        assert_eq!(
            call(&client, Request::TermDetach { pty_id: "pty-1".into() }).await,
            Response::Ack,
        );
    };

    futures::future::join(serve, script).await;
    assert_eq!(
        released.lock().await.as_slice(),
        [AttachmentId(1)],
        "the attachment this connection opened was given back",
    );
}

/// A repeat attach gives back the attachment it just minted.
///
/// Re-attaching is the documented gap recovery, so it has to stay cheap — it
/// serves the replay again without opening a second stream. But the host mints a
/// fresh attachment for every attach, and the discarded one carries a size vote
/// taken at whatever grid was in force. Left behind on a quiet terminal, that
/// vote pins the terminal there: the client recovers from one gap and then
/// cannot grow its window again, with nothing on screen to explain why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeat_attach_gives_back_the_attachment_it_minted() {
    let h = harness(None);
    let released = Arc::clone(&h.released);
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32], None))).await
        else {
            panic!("expected Registered");
        };
        for _ in 0..2 {
            let Response::TermAttached { .. } =
                call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
            else {
                panic!("a repeat attach still serves the replay");
            };
        }
        drop(client);
    };

    futures::future::join(serve, script).await;
    // The second attach (2) goes back immediately; the first (1) goes back when
    // the connection ends. The order is the assertion — 2 must not be waiting on
    // anything to notice it was discarded.
    assert_eq!(
        released.lock().await.as_slice(),
        [AttachmentId(2), AttachmentId(1)],
        "the duplicate was released at once, not left holding a size vote",
    );
}

/// A connection that simply goes away still hands its attachments back.
///
/// This is the COMMON exit, not the tidy one: a phone that loses signal or gets
/// swiped away never sends `TermDetach`. If only the explicit path released,
/// every lost connection would leave a terminal sized for a device that is no
/// longer there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_connection_hands_back_what_it_still_held() {
    let h = harness(None);
    let released = Arc::clone(&h.released);
    let (client, server) = duplex_pair();
    let serve = h.dispatcher.serve(&server);

    let script = async move {
        let Response::Registered { .. } =
            call(&client, Request::Register(register_req([0x33; 32], None))).await
        else {
            panic!("expected Registered");
        };
        let Response::TermAttached { .. } =
            call(&client, Request::TermAttach { pty_id: "pty-1".into() }).await
        else {
            panic!("expected TermAttached");
        };
        // No detach, no goodbye — the connection just ends.
        drop(client);
    };

    futures::future::join(serve, script).await;
    assert_eq!(
        released.lock().await.as_slice(),
        [AttachmentId(1)],
        "the attachment did not outlive the connection that opened it",
    );
}
