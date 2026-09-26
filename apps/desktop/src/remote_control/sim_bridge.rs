//! The inbound simulator path: an `oximux sim …` verb crossing from the
//! dispatcher's tokio task onto the GPUI thread, and its answer coming back.
//!
//! The same bridge shape as [`launch_bridge`](super::launch_bridge) and
//! [`rewind_bridge`](super::rewind_bridge) (`AsyncApp` is not `Send`), with
//! one difference that matters: the drain loop **spawns a task per request**
//! instead of awaiting each in turn. A simulator verb can wait on the device
//! for a while — an install copies a whole app, a woken device takes seconds
//! to stream — and serial draining would make every other agent's `status`
//! wait behind it.

use oximux_remote_host::SimulatorControl;
use oximux_remote_proto::simulator::{SimCmdWire, SimErrorWire, SimReplyWire};
use tokio::sync::{mpsc, oneshot};

/// One verb, plus where to send the answer.
pub struct SimRequest {
    pub worktree: String,
    pub cmd: SimCmdWire,
    pub reply: oneshot::Sender<Result<SimReplyWire, SimErrorWire>>,
}

/// How many verbs may wait for the UI thread to pick them up. Agents poll
/// (`sim wait-consent`) and several may work at once, so a little deeper
/// than the launch queue; a full queue answers `Unavailable` at once.
const QUEUE: usize = 16;

/// The dispatcher's end of the bridge.
pub struct BridgeSimulator {
    tx: mpsc::Sender<SimRequest>,
}

/// Build both ends. Dropping the receiver makes every later verb answer
/// `Unavailable`, the right answer once the UI is gone.
pub fn sim_bridge() -> (BridgeSimulator, mpsc::Receiver<SimRequest>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (BridgeSimulator { tx }, rx)
}

fn busy() -> SimErrorWire {
    SimErrorWire::Unavailable("the desktop app is busy or closing; try again".into())
}

#[async_trait::async_trait]
impl SimulatorControl for BridgeSimulator {
    async fn run(&self, worktree: &str, cmd: SimCmdWire) -> Result<SimReplyWire, SimErrorWire> {
        let (reply, answer) = oneshot::channel();
        // `try_send`: a full queue or a dropped receiver fails now rather
        // than parking this RPC on the connection's request slot.
        self.tx.try_send(SimRequest { worktree: worktree.to_string(), cmd, reply }).map_err(|_| busy())?;
        answer.await.map_err(|_| busy())?
    }
}

/// Drain simulator verbs on the GPUI thread, one task each.
pub fn serve_simulator(mut rx: mpsc::Receiver<SimRequest>, cx: &mut gpui::App) {
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Some(SimRequest { worktree, cmd, reply }) = rx.recv().await {
            cx.spawn(async move |cx: &mut gpui::AsyncApp| {
                let outcome = crate::shell::simulator::agent_ops::run(worktree, cmd, cx).await;
                // A send failure means the caller gave up; the verb has
                // already happened or failed on its own.
                let _ = reply.send(outcome);
            })
            .detach();
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_verb_carries_its_answer_back() {
        let (bridge, mut rx) = sim_bridge();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            assert_eq!(req.worktree, "/w");
            assert_eq!(req.cmd, SimCmdWire::Status);
            let _ = req.reply.send(Err(SimErrorWire::ConsentPending));
        });
        assert_eq!(bridge.run("/w", SimCmdWire::Status).await, Err(SimErrorWire::ConsentPending));
    }

    #[tokio::test]
    async fn no_ui_side_fails_instead_of_hanging() {
        let (bridge, rx) = sim_bridge();
        drop(rx);
        assert!(matches!(bridge.run("/w", SimCmdWire::Status).await, Err(SimErrorWire::Unavailable(_))));
        let (bridge, mut rx) = sim_bridge();
        tokio::spawn(async move {
            drop(rx.recv().await.expect("a request").reply);
        });
        assert!(matches!(bridge.run("/w", SimCmdWire::Status).await, Err(SimErrorWire::Unavailable(_))));
    }
}
