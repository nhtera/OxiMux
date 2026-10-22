//! Attaching a local session to a daemon PTY, and the pump that keeps it fed.
//!
//! Exactness rests on the daemon: it snapshots its ring and adds our
//! subscriber under one lock, and holds that lock through every fan-out, so a
//! byte is either in the replay or delivered to us — never both, never
//! neither. This side keeps up its end:
//! - subscribe **before** sending `Attach`, so nothing the daemon fans in the
//!   window before `AttachOk` is dropped as "for an unknown pty";
//! - keep only output addressed to our own attachment (or unaddressed): until
//!   the subscription is bound it also receives copies meant for other
//!   attachments of the same PTY;
//! - after a gap, discard queued output up to the *fence* — an empty `Output`
//!   the daemon queues for us right behind the resync snapshot — since that
//!   output is already in the replay.
//!
//! [`AttachCtx`] holds only shared handles, so an attach can run without the
//! caller holding the backend (`TerminalBackend::begin_attach_existing`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use oximux_pty::program_status::PROGRAM_STATUS_REPLY;
use oximux_pty::{TerminalSessionId, TerminalState};
use oximux_relay_proto::{Notification, Request, Response, UNROUTED_ATTACHMENT};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::backend::{
    NEXT_SESSION_ID, OutputWakers, SCROLLBACK_ROWS, Session, SessionEventQueues, apply_relay_notification,
    lock_recover, push_event,
};
use crate::client::RelayClient;

/// How long a resync waits for its fence before taking output as it comes. A
/// daemon from before fences never sends one; a lost one must not stall the
/// pane for good.
const FENCE_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared handles an attach or a pump needs; cheap to clone.
#[derive(Clone)]
pub(crate) struct AttachCtx {
    pub(crate) client: Arc<RelayClient>,
    pub(crate) handle: Handle,
    pub(crate) sessions: Arc<Mutex<HashMap<TerminalSessionId, Session>>>,
    pub(crate) queues: SessionEventQueues,
    pub(crate) wakers: OutputWakers,
    /// The daemon predates this release: it neither answers program-status
    /// queries nor fences a resync (see `daemon_answers_program_status`).
    pub(crate) answers_program_status: bool,
}

impl AttachCtx {
    /// Attach to daemon PTY `relay_pty_id`, replay its buffered output into a
    /// fresh local grid BEFORE the pump starts — so the first frame the
    /// renderer sees is the full prior screen — and register the session.
    /// Blocks on the round-trip: call it off the UI thread, and without the
    /// backend lock held.
    pub(crate) fn attach(&self, relay_pty_id: &str) -> Result<TerminalSessionId> {
        // Subscribe first: the daemon starts fanning to the new attachment the
        // moment `Attach` lands, possibly ahead of the `AttachOk` that names it.
        let (sub_id, notif_rx) = self.client.subscribe_pty(relay_pty_id);
        let resp = self
            .handle
            .block_on(self.client.request(Request::Attach { pty_id: relay_pty_id.to_owned() }))
            .map_err(|e| anyhow!(e));
        let (replay, cols, rows, attachment_id) = match resp {
            Ok(Response::AttachOk { replay, cols, rows, attachment_id }) => (replay, cols, rows, attachment_id),
            other => {
                self.client.unsubscribe_pty(relay_pty_id, sub_id);
                match other {
                    Ok(Response::Err { code, message }) => bail!("attach: {code:?} — {message}"),
                    Ok(other) => bail!("unexpected attach response: {other:?}"),
                    Err(e) => return Err(e),
                }
            }
        };
        // Build the local emulator at the daemon PTY's CURRENT dims, NOT a
        // hardcoded default. The replay bytes were produced by a process
        // drawing into a grid of this exact size — absolute-position CSI
        // sequences only land correctly when the receiving grid matches.
        // Replaying into the wrong size (then reflowing on the first
        // pane-driven resize) is what scrambled restored full-screen TUIs.
        // When the pane later resizes, `TerminalBackend::resize` resizes
        // the REAL daemon PTY, the live process repaints via SIGWINCH, and
        // those bytes arrive on the live stream — no static reflow.
        let cols = cols.max(1);
        let rows = rows.max(1);
        // NOTE: attach uses the const scrollback (not the user's
        // `scrollback_lines` setting) by design — the replay buffer must hold
        // the daemon's full retained output and the receiver grid has to be
        // sized to fit it, regardless of what the user picked for fresh spawns.
        let state = Arc::new(Mutex::new(TerminalState::new(cols, rows, SCROLLBACK_ROWS)));
        let id = TerminalSessionId(NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed));
        {
            let mut s = lock_recover(&state, "terminal state");
            s.set_answers_program_status(self.answers_program_status);
            // Replay is historical: it drops the title/clipboard/bell/color
            // events it fired so they don't leak into the first live frame, but
            // keeps the program's last reported status, which consumers start
            // from.
            s.replay(&replay);
            if let Some(ev) = s.program_status_event(id) {
                push_event(&self.queues, id, ev);
            }
        }
        // Address this subscription. Without it the pane also receives the
        // copies the daemon fans to every OTHER attachment on the same PTY — a
        // paired phone watching this terminal is enough to make the pane render
        // every byte twice.
        self.client.bind_attachment(relay_pty_id, sub_id, attachment_id);
        let generation = Arc::new(AtomicU64::new(1));
        let pump = self.spawn_pump(
            id,
            relay_pty_id.to_owned(),
            Arc::clone(&state),
            Arc::clone(&generation),
            1,
            attachment_id,
            notif_rx,
        );
        lock_recover(&self.sessions, "sessions").insert(
            id,
            Session {
                relay_pty_id: relay_pty_id.to_owned(),
                attachment_id,
                sub_id,
                state,
                cols,
                rows,
                generation,
                _pump: pump,
            },
        );
        Ok(id)
    }

    /// The task that applies this session's notifications as they arrive.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_pump(
        &self,
        id: TerminalSessionId,
        relay_pty_id: String,
        state: Arc<Mutex<TerminalState>>,
        generation: Arc<AtomicU64>,
        my_generation: u64,
        attachment_id: u64,
        mut notif_rx: UnboundedReceiver<Notification>,
    ) -> JoinHandle<()> {
        let ctx = self.clone();
        self.handle.spawn(async move {
            let queues = &ctx.queues;
            // `Some` from a resync until its fence: output in between is
            // already in the replay.
            let mut fence_wait: Option<Instant> = None;
            while let Some(n) = notif_rx.recv().await {
                // Only our own copies (or unaddressed ones): before the bind the
                // client also hands us copies for other attachments of this PTY.
                if let Some(to) = n.attachment_id()
                    && to != attachment_id
                    && to != UNROUTED_ATTACHMENT
                {
                    continue;
                }
                if let Notification::Output { bytes, .. } = &n {
                    match fence_wait {
                        Some(_) if bytes.is_empty() => {
                            fence_wait = None;
                            continue;
                        }
                        Some(since) if since.elapsed() < FENCE_TIMEOUT => continue,
                        Some(_) => fence_wait = None,
                        None => {}
                    }
                    // A fence meant for another resync on this connection, or a
                    // stray: an empty chunk is not activity.
                    if bytes.is_empty() {
                        continue;
                    }
                }
                // A gap is handled here rather than in `apply_relay_notification`
                // because recovery needs an await on the daemon, and that
                // function is deliberately synchronous so the generation guard
                // stays unit-testable without a runtime.
                if matches!(n, Notification::Gapped { .. }) {
                    // Same guard the apply path opens with: a superseded pump
                    // must not touch the shared state, and a resync writes the
                    // whole grid.
                    if generation.load(Ordering::Acquire) != my_generation {
                        return;
                    }
                    if resync_after_gap(&ctx.client, &relay_pty_id, &state, queues, id).await
                        && !ctx.answers_program_status
                    {
                        fence_wait = Some(Instant::now());
                    }
                    if let Some(waker) = lock_recover(&ctx.wakers, "output wakers").get(&id) {
                        waker();
                    }
                    continue;
                }
                let flow = apply_relay_notification(&state, queues, id, &generation, my_generation, n);
                // Only set on a state that answers queries itself (an older
                // daemon): the reply goes out the same way keystrokes do.
                let reply = lock_recover(&state, "terminal state").take_program_status_reply();
                if let Some(ticket) = reply {
                    let _ = ctx.client.try_send_oneway(Request::Write {
                        pty_id: relay_pty_id.clone(),
                        bytes: PROGRAM_STATUS_REPLY.to_vec(),
                    });
                    ticket.mark_written(Instant::now());
                }
                // Event-driven drain: nudge the UI the instant output is queued
                // so it renders on arrival instead of waiting for the next
                // (macOS-throttled) poll tick. Fired for every notification —
                // an empty drain is cheap; a missed wake is felt lag.
                if let Some(waker) = lock_recover(&ctx.wakers, "output wakers").get(&id) {
                    waker();
                }
                if flow.is_break() {
                    return;
                }
            }
        })
    }
}

/// Rebuild the local grid from the daemon's replay ring after a dropped frame.
/// `true` when it was rebuilt (the pump then waits for the daemon's fence).
///
/// Uses `Replay` rather than a second `Attach`: this session already holds an
/// attachment, and attaching again would add a stale entry to the daemon's
/// smallest-screen-wins size calculation — so recovering from a dropped frame
/// would resize the live process.
///
/// The state is replaced wholesale rather than patched. There is no way to know
/// which bytes were missed, so the grid is rebuilt at the daemon's current dims
/// exactly as `attach` builds it; anything less would leave the hole in place.
/// A failure here is logged and left alone: a stale grid is worse than a correct
/// one but far better than tearing down a live session over one dropped frame,
/// and the next gap retries.
async fn resync_after_gap(
    client: &RelayClient,
    relay_pty_id: &str,
    state: &Arc<Mutex<TerminalState>>,
    queues: &SessionEventQueues,
    id: TerminalSessionId,
) -> bool {
    let resp = client.request(Request::Replay { pty_id: relay_pty_id.to_owned() }).await;
    let (replay, cols, rows) = match resp {
        Ok(Response::ReplayOk { replay, cols, rows }) => (replay, cols.max(1), rows.max(1)),
        Ok(other) => {
            tracing::warn!(?relay_pty_id, ?other, "gap resync: unexpected response");
            return false;
        }
        Err(err) => {
            tracing::warn!(?relay_pty_id, ?err, "gap resync: request failed");
            return false;
        }
    };
    let mut s = lock_recover(state, "terminal state");
    let before = s.program_status_root();
    let mut fresh = TerminalState::new(cols, rows, SCROLLBACK_ROWS);
    // Who answers, and a handshake whose query the ring may have dropped.
    fresh.inherit_program_status(&s);
    *s = fresh;
    // Replay is historical: drop the title/clipboard/bell/colour events it
    // fired so a resync does not re-ring the bell for output the user already
    // saw before the gap. Program-status records are kept, and a status change
    // that fell inside the gap is announced now — nothing else would.
    s.replay(&replay);
    if let Some(ev) = s.program_status_change_since(id, before.as_ref()) {
        drop(s);
        push_event(queues, id, ev);
    }
    true
}
