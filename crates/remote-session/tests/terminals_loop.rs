//! Terminals end to end: the real client against the real `remote-host`
//! dispatcher over the in-memory loopback.
//!
//! The load-bearing case is **RPC-while-streaming**. The wire carries no
//! correlation id — replies come back in send order and the pump routes the next
//! non-push frame to the single outstanding slot — so a pushed terminal frame
//! that is *not* recognized as a push gets handed to whatever RPC happens to be
//! in flight. One misrouted frame desynchronizes the connection permanently.
//! Terminal output arrives continuously and unbidden, which makes it by far the
//! most likely frame to trigger that, so it is asserted here rather than assumed
//! to follow from the session-event case.

use std::sync::Arc;

use futures::StreamExt;
use futures::executor::block_on;
use futures::future::join3;
use oximux_agents::session_registry::SessionRegistry;
use oximux_remote_host::{
    AuthStore, Dispatcher, PairingSlot, TerminalAttach, TerminalError, TerminalFrame,
    TerminalSource,
};
use oximux_remote_proto::PairingTicket;
use oximux_remote_proto::messages::TerminalSummary;
use oximux_remote_proto::testing::duplex_pair;
use oximux_remote_session::{ClientSigner, RemoteSession, TerminalPush};
use tokio::sync::mpsc;

const NOW: u64 = 1_700_000_000;
fn clock() -> u64 {
    NOW
}
const SECRET: [u8; 16] = [0x22; 16];
const CLIENT_SEED: [u8; 32] = [7u8; 32];

fn ticket() -> PairingTicket {
    PairingTicket { endpoint_id: [0u8; 32], handshake_secret: SECRET, session_id: None }
}

/// A terminal host with one PTY whose live frames the test drives by hand.
struct FakeTerminals {
    frames: std::sync::Mutex<Option<mpsc::Receiver<TerminalFrame>>>,
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
        let rx = self.frames.lock().unwrap().take().ok_or(TerminalError::Unavailable)?;
        Ok((TerminalAttach { replay: b"prompt$ ".to_vec(), cols: 80, rows: 24 }, rx))
    }

    async fn input(&self, _pty_id: &str, _bytes: &[u8]) -> Result<(), TerminalError> {
        Ok(())
    }

    async fn resize(&self, _pty_id: &str, _cols: u16, _rows: u16) -> Result<(), TerminalError> {
        Ok(())
    }
}

/// Attach, receive pushed output, and — the point — issue an RPC *while* the
/// terminal is streaming, proving the pushed frames never consume the reply slot.
#[test]
fn a_streaming_terminal_does_not_steal_rpc_replies() {
    let (tx, rx) = mpsc::channel(8);
    let terminals = Arc::new(FakeTerminals { frames: std::sync::Mutex::new(Some(rx)) });
    let auth = Arc::new(AuthStore::new());
    auth.set_pairing(PairingSlot::new(SECRET, None, false));
    let dispatcher = Dispatcher::new(Arc::new(SessionRegistry::new()), auth)
        .with_clock(clock)
        .with_terminals(terminals);

    let (client, server) = duplex_pair();
    let serve = dispatcher.serve(&server);
    let session = RemoteSession::new(Arc::new(client), ClientSigner::from_seed(&CLIENT_SEED));
    let pump = session.take_pump().expect("pump");
    let mut pushes = session.take_terminals().expect("terminal stream");

    let script = async move {
        session.pair(&ticket(), "phone", NOW).await.expect("pair");

        let listed = session.list_terminals().await.expect("list terminals");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].pty_id, "pty-1");

        let attached = session.term_attach("pty-1").await.expect("attach");
        assert_eq!(attached.replay, b"prompt$ ");
        assert_eq!(
            (attached.cols, attached.rows),
            (80, 24),
            "the dims ride with the replay — the bytes only render correctly in that grid",
        );

        // Push output, then immediately issue an RPC. If the pushed frame were
        // routed to the reply slot, this call would resolve with terminal bytes
        // (or hang), and every later call would be one reply out of step.
        tx.send(TerminalFrame::Output(b"ls\r\n".to_vec())).await.unwrap();
        let listed_again = session.list_terminals().await.expect("RPC still works while streaming");
        assert_eq!(listed_again.len(), 1, "the reply is the list, not the terminal output");

        // …and the output is still delivered, on its own stream.
        let push = pushes.next().await.expect("a pushed terminal frame");
        assert_eq!(
            push,
            TerminalPush::Output { pty_id: "pty-1".into(), bytes: b"ls\r\n".to_vec() },
        );

        // A gap and an exit route the same way.
        tx.send(TerminalFrame::Gapped).await.unwrap();
        assert_eq!(
            pushes.next().await.expect("gap"),
            TerminalPush::Gapped { pty_id: "pty-1".into() },
        );
        tx.send(TerminalFrame::Exited(Some(0))).await.unwrap();
        assert_eq!(
            pushes.next().await.expect("exit"),
            TerminalPush::Exited { pty_id: "pty-1".into(), code: Some(0) },
        );

        session.term_input("pty-1", b"echo hi\n").await.expect("input");
        session.term_resize("pty-1", 100, 30).await.expect("resize");
        session.term_detach("pty-1").await.expect("detach");
    };

    let (_, pump_res, ()) = block_on(join3(serve, pump.run(), script));
    pump_res.expect("pump ran to a clean shutdown");
}
