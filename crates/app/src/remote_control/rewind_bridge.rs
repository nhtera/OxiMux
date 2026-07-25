//! The inbound rewind path: a rewind request crossing from the dispatcher's
//! tokio task onto the GPUI thread, and its answer coming back.
//!
//! Second user of the bridge shape [`launch_bridge`](super::launch_bridge)
//! introduced, and for the same reason: `AsyncApp` is not `Send` while the
//! dispatcher runs on a multi-threaded tokio runtime, so the RPC task cannot
//! touch GPUI at all. It parks on a oneshot and the GPUI loop answers it.
//!
//! It differs from the launch bridge in one way that matters. Launching picks a
//! window deterministically because a new session has no home yet; a rewind
//! names an *existing* session, so this has to find the exact view holding it —
//! across every window, every project (including ones the user has switched
//! away from), and every tab.

use oximux_remote_host::{RewindError, RewindService};
use tokio::sync::{mpsc, oneshot};

/// One request to rewind a session, plus where to send the answer.
pub struct RewindRequest {
    pub session_id: String,
    pub ordinal: usize,
    pub include_files: bool,
    pub reply: oneshot::Sender<Result<(), RewindError>>,
}

/// How many rewind requests may queue before the UI thread has drained them.
///
/// Small for the same reason the launch queue is: these arrive at human speed,
/// so a deep queue would only hold requests nobody is waiting on. A full queue
/// reports [`RewindError::Unavailable`] — honest, since the desktop is not
/// keeping up.
const QUEUE: usize = 4;

/// The dispatcher's end of the bridge.
pub struct BridgeRewinder {
    tx: mpsc::Sender<RewindRequest>,
}

/// Build both ends. Dropping the receiver makes every later rewind fail with
/// [`RewindError::Unavailable`], the right answer once the UI is gone.
pub fn rewind_bridge() -> (BridgeRewinder, mpsc::Receiver<RewindRequest>) {
    let (tx, rx) = mpsc::channel(QUEUE);
    (BridgeRewinder { tx }, rx)
}

#[async_trait::async_trait]
impl RewindService for BridgeRewinder {
    async fn rewind(
        &self,
        session_id: &str,
        ordinal: usize,
        include_files: bool,
    ) -> Result<(), RewindError> {
        let (reply, answer) = oneshot::channel();
        let request = RewindRequest {
            session_id: session_id.to_string(),
            ordinal,
            include_files,
            reply,
        };
        // `try_send`, not `send`: a full queue or dropped receiver should fail
        // now rather than park this RPC forever holding the connection's
        // request slot.
        self.tx.try_send(request).map_err(|_| RewindError::Unavailable)?;
        // A dropped sender means the UI gave up without replying — a failure,
        // not something to hang on, since nothing else will ever answer.
        answer.await.map_err(|_| RewindError::Unavailable)?
    }
}

/// Drain rewind requests on the GPUI thread.
///
/// Hosted at the top level of `app.run` alongside `serve_launches`: a rewind
/// targets a session in any window, so it needs the global window set, which
/// only `&App` can enumerate.
pub fn serve_rewinds(rx: mpsc::Receiver<RewindRequest>, cx: &mut gpui::App) {
    let mut rx = rx;
    cx.spawn(async move |cx: &mut gpui::AsyncApp| {
        while let Some(request) = rx.recv().await {
            // Split before sending: `send` consumes the reply channel, so the
            // request cannot still be borrowed at that point.
            let RewindRequest { session_id, ordinal, include_files, reply } = request;
            let outcome =
                cx.update(|cx| start_rewind(&session_id, ordinal, include_files, cx));
            // A send failure means the caller gave up (RPC timed out, connection
            // dropped). Nothing to do — and unlike a launch, the rewind it asked
            // for has already begun, which subscribers learn from the `Rewound`
            // event rather than from this reply.
            let _ = reply.send(outcome);
        }
    })
    .detach();
}

/// Find the view holding `session_id` and start its rewind.
fn start_rewind(
    session_id: &str,
    ordinal: usize,
    include_files: bool,
    cx: &mut gpui::App,
) -> Result<(), RewindError> {
    let view = find_chat_view(session_id, cx).ok_or(RewindError::Unavailable)?;
    // The view re-validates the ordinal against its own thread — the phone's
    // fold can legitimately be behind this one.
    view.update(cx, |view, cx| view.remote_rewind(ordinal, include_files, cx))
}

/// The chat view bound to `session_id`, across every window and project.
///
/// Searches all projects, not just each window's active one: a session keeps
/// running in a project the user has switched away from, and reporting it
/// missing because of what is currently on screen would be wrong.
fn find_chat_view(
    session_id: &str,
    cx: &mut gpui::App,
) -> Option<gpui::Entity<crate::shell::agent_chat::AgentChatView>> {
    for (_, workspace) in crate::platform::window_registry::all_windows(cx) {
        for panes in workspace.read(cx).all_project_panes() {
            if let Some(view) = panes.read(cx).agent_chat_view_by_remote_id(session_id, cx) {
                return Some(view);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_rewind_request_carries_its_answer_back() {
        let (rewinder, mut rx) = rewind_bridge();
        let ui = tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            assert_eq!(req.session_id, "agent-3");
            assert_eq!(req.ordinal, 2);
            assert!(!req.include_files);
            let _ = req.reply.send(Ok(()));
        });
        rewinder.rewind("agent-3", 2, false).await.expect("accepted");
        ui.await.unwrap();
    }

    /// The refusal must reach the client intact rather than collapsing into a
    /// generic failure — a phone that asked for files needs to know it can fall
    /// back to a conversation-only rewind.
    #[tokio::test]
    async fn a_refusal_keeps_its_category() {
        let (rewinder, mut rx) = rewind_bridge();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            let _ = req.reply.send(Err(RewindError::FilesUnsupported));
        });
        assert!(matches!(
            rewinder.rewind("agent-3", 2, true).await,
            Err(RewindError::FilesUnsupported)
        ));
    }

    /// No UI side at all — the window is gone, or remote was enabled before one
    /// existed. Must fail rather than park the RPC forever.
    #[tokio::test]
    async fn a_dropped_receiver_fails_instead_of_hanging() {
        let (rewinder, rx) = rewind_bridge();
        drop(rx);
        assert!(matches!(
            rewinder.rewind("agent-1", 0, false).await,
            Err(RewindError::Unavailable)
        ));
    }

    /// The UI took the request then went away without answering.
    #[tokio::test]
    async fn a_dropped_reply_channel_fails_instead_of_hanging() {
        let (rewinder, mut rx) = rewind_bridge();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("a request");
            drop(req.reply);
        });
        assert!(matches!(
            rewinder.rewind("agent-1", 0, false).await,
            Err(RewindError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn a_full_queue_is_refused_rather_than_awaited() {
        let (rewinder, _rx) = rewind_bridge(); // receiver held, never drained
        // The futures must be *polled* to enqueue anything — building and
        // dropping them would leave the queue empty and the assertion below
        // passing for the wrong reason.
        let mut held = Vec::new();
        for _ in 0..QUEUE {
            held.push(Box::pin(rewinder.rewind("agent-1", 0, false)));
        }
        for f in &mut held {
            let _ = futures::poll!(f.as_mut());
        }
        assert!(matches!(
            rewinder.rewind("agent-1", 0, false).await,
            Err(RewindError::Unavailable)
        ));
    }
}
