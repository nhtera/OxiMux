//! A PTY's read side: the thread that drains its output into the replay log
//! and out to every attached subscriber, and the log itself.
//!
//! The [`ReplayLog`] is the ring of recent output plus the [`ModeTracker`] that
//! follows it, kept under one lock so a snapshot and the modes in force at that
//! snapshot always agree. [`ReplayLog::replay`] turns them into what a client
//! attaching (or resyncing) receives: the ring from a clean line start, and a
//! postamble restoring the modes set before it (see `vt_modes`).

use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oximux_relay_proto::{Notification, UNROUTED_ATTACHMENT};
use tokio::sync::mpsc::error::TrySendError;

use crate::checkpoint::CheckpointStore;
use crate::program_status_query::ProgramStatusQueryDetector;
use crate::pty_writer::PtyWriter;
use crate::registry::{EXIT_CODE_NONE, READ_CHUNK_BYTES, Subscriber};
use crate::ring_buffer::RingBuffer;
use crate::vt_modes::{ModeTracker, align_start, postamble};

/// Recent output plus the terminal modes it leaves in force.
pub(crate) struct ReplayLog {
    ring: RingBuffer,
    modes: ModeTracker,
}

/// What a client is sent to rebuild the screen.
pub(crate) struct Replay {
    pub bytes: Vec<u8>,
    /// The program is in the alternate screen: its contents are the program's
    /// to repaint.
    pub alt_screen: bool,
}

impl ReplayLog {
    /// A log seeded with restored history (`prefill`), which counts as output
    /// for the modes it sets but was never sent to anyone.
    pub(crate) fn new(capacity: usize, prefill: &[u8]) -> Self {
        let mut log = Self { ring: RingBuffer::new(capacity), modes: ModeTracker::new() };
        log.push(prefill);
        log
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
        self.modes.feed(bytes);
    }

    /// The raw ring, for the disk checkpoint.
    pub(crate) fn raw(&self) -> Vec<u8> {
        self.ring.snapshot()
    }

    /// The replay for a client: the ring from its first clean line start once
    /// it has wrapped (an unwrapped ring starts at the true beginning, which a
    /// restored capture header relies on), then a postamble for the modes the
    /// body does not set itself. In the alternate screen with its entry
    /// already evicted the body is dropped: those bytes would draw a partial
    /// frame on the primary screen; the program repaints instead.
    ///
    /// Two steps so the lock covers only the copy: [`snapshot`](Self::snapshot)
    /// under it, [`ReplaySnapshot::build`] after.
    pub(crate) fn snapshot(&self) -> ReplaySnapshot {
        ReplaySnapshot {
            snap: self.ring.snapshot(),
            offset: self.modes.offset(),
            live: self.modes.modes().clone(),
            alt_entry_end: self.modes.alt_entry_end(),
        }
    }
}

/// A [`ReplayLog`] frozen at one instant; turning it into a [`Replay`] (the
/// alignment scan, the body's own modes, the postamble) needs no lock.
pub(crate) struct ReplaySnapshot {
    snap: Vec<u8>,
    offset: u64,
    live: crate::vt_modes::Modes,
    alt_entry_end: Option<u64>,
}

impl ReplaySnapshot {
    pub(crate) fn build(mut self) -> Replay {
        let len = self.snap.len();
        let wrapped = self.offset > len as u64;
        let body_start = if wrapped { align_start(&self.snap) } else { 0 };
        let body_start_offset = self.offset - (len - body_start) as u64;
        let evicted = self.alt_entry_end.is_some_and(|end| end <= body_start_offset);
        let mut bytes = if evicted { Vec::new() } else { self.snap.split_off(body_start) };
        let mut body_modes = ModeTracker::new();
        body_modes.feed(&bytes);
        bytes.extend_from_slice(&postamble(body_modes.modes(), &self.live));
        Replay { bytes, alt_screen: self.live.alt_screen.is_some() }
    }
}

/// How long [`reader_loop`] keeps draining output after the child has been
/// reaped, before it publishes `Exit`.
///
/// The daemon cannot key exit off the reader seeing EOF, because on Windows it
/// never does: a ConPTY's output pipe belongs to the pseudoconsole, which
/// outlives the child and is released only when the master is dropped at
/// `close`. A shell that exits on its own — `exit`, a crash, a finished agent —
/// produces no EOF at all, so an EOF-only loop would never fan out
/// `Notification::Exit`, never drop the checkpoint, and leave the entry looking
/// live to `list` forever.
///
/// The window covers the hand-off race: `child.wait()` can return while the last
/// bytes the child wrote are still in flight, and those belong in the ring ahead
/// of `Exit`. It only ever delays a session that has already ended.
///
/// A deadline from the moment of reaping rather than an idle timeout, for the same
/// reason as its twin in `oximux-pty`: a detached grandchild that inherits the pty
/// and keeps writing would hold an idle timeout open forever, and the session
/// would never report `Exit` — the original bug, reintroduced.
const POST_EXIT_DRAIN: Duration = Duration::from_millis(200);

/// Wake-up granularity for the drain loop while no output is arriving.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(20);

// The reader task owns the PTY's full I/O state (reader, child handle, ring,
// subscribers, exit flags); threading it as discrete args keeps the spawn site
// explicit rather than hiding it behind a bag struct.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reader_loop(
    pty_id: String,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    log: Arc<Mutex<ReplayLog>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    child_exited: Arc<AtomicBool>,
    exit_code: Arc<AtomicI32>,
    bytes_out: Arc<AtomicU64>,
    checkpoints: Option<Arc<CheckpointStore>>,
    restarting: Arc<AtomicBool>,
    writer: PtyWriter,
) {
    // Reap on a thread of its own rather than after the read loop, and read on a
    // thread of its own rather than inline, so the loop below can end the session
    // on whichever comes first: EOF, or the child being reaped plus a short
    // drain. Waiting for EOF alone is what does not port — see `POST_EXIT_DRAIN`.
    let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel::<Option<i32>>(1);
    std::thread::spawn(move || {
        let code = child.wait().ok().map(|s| s.exit_code() as i32);
        let _ = exit_tx.send(code);
    });

    // Dropping `bytes_tx` is how the reader reports EOF, keeping the unix path's
    // "pty closed, session over" behaviour exactly as it was.
    let (bytes_tx, bytes_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(8);
    let pty_id_for_reader = pty_id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_CHUNK_BYTES];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    // Blocking send: these bytes are the scrollback, so applying
                    // backpressure to the pty is right and dropping them is not.
                    if bytes_tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(?e, pty_id = pty_id_for_reader, "reader EOF/err");
                    return;
                }
            }
        }
    });

    // Live output only: replayed history never reaches here, so it is never answered.
    let mut queries = ProgramStatusQueryDetector::new();
    let mut publish = |bytes: &[u8]| {
        queries.feed(bytes, Instant::now(), |ticket| writer.try_reply(ticket));
        bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let notif = Notification::Output {
            pty_id: pty_id.clone(),
            attachment_id: UNROUTED_ATTACHMENT,
            bytes: bytes.to_vec(),
        };
        // The log stays locked through the fan-out (ring before subscribers,
        // the order `attach` and `replay` use). That is what makes an attach
        // exact: each byte is either in the snapshot it takes or delivered to
        // the subscriber it adds, never both and never neither. `fan_out` only
        // `try_send`s, so the hold is short.
        let mut log = log.lock().expect("replay log poisoned");
        log.push(bytes);
        fan_out(&subscribers, notif);
        drop(log);
    };

    let mut reaped: Option<Option<i32>> = None;
    let mut drain_deadline: Option<Instant> = None;
    // Whether a daemon restart ended this session, decided the moment the end
    // is first seen — not after the drain, when a restart that began since
    // would claim a session that had already ended on its own.
    let mut ended_by_restart: Option<bool> = None;
    loop {
        match bytes_rx.recv_timeout(DRAIN_POLL_INTERVAL) {
            Ok(chunk) => publish(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                ended_by_restart.get_or_insert_with(|| restarting.load(Ordering::SeqCst));
                break;
            }
        }
        if reaped.is_none()
            && let Ok(code) = exit_rx.try_recv()
        {
            ended_by_restart.get_or_insert_with(|| restarting.load(Ordering::SeqCst));
            reaped = Some(code);
            drain_deadline = Some(Instant::now() + POST_EXIT_DRAIN);
        }
        if let Some(deadline) = drain_deadline
            && Instant::now() >= deadline
        {
            break;
        }
    }

    // Output the reader already handed over belongs in the ring ahead of `Exit`.
    while let Ok(chunk) = bytes_rx.try_recv() {
        publish(&chunk);
    }

    // On the EOF route the reaper has not been consulted yet, and its status is
    // worth blocking for: EOF means the pty is gone, so `wait` is about to
    // return. On the reaped route the code is already in hand.
    let code = match reaped {
        Some(code) => code,
        None => exit_rx.recv().unwrap_or(None),
    };
    // Stash the status BEFORE flipping `child_exited`, so any reader that
    // observes the flag (here or in `attach`) also sees the final code.
    exit_code.store(code.unwrap_or(EXIT_CODE_NONE), Ordering::Release);
    // Release-ordered so the corresponding Acquire load in `close`
    // observes the flag flip without sequencing the fan_out below.
    child_exited.store(true, Ordering::Release);
    // Ended by a daemon restart, not by itself: keep the checkpoint the app
    // restores from, and stay quiet — see `PtyRegistry::restarting`.
    if ended_by_restart.unwrap_or_else(|| restarting.load(Ordering::SeqCst)) {
        tracing::debug!(pty_id, "session ended by a daemon restart; keeping its checkpoint");
        return;
    }
    // Natural child exit is a clean end — drop the disk checkpoint so
    // an exited shell never cold-restores on the next launch. (The
    // `close` path removes too; remove is idempotent.)
    if let Some(store) = &checkpoints {
        let _ = store.remove(&pty_id);
    }
    fan_out(
        &subscribers,
        Notification::Exit {
            pty_id: pty_id.clone(),
            attachment_id: UNROUTED_ATTACHMENT,
            code,
        },
    );
}

pub(crate) fn fan_out(subscribers: &Arc<Mutex<Vec<Subscriber>>>, notif: Notification) {
    let mut subs = subscribers.lock().expect("subs poisoned");
    // Drop subscribers whose receiver has been dropped. For "full" we keep the
    // subscriber but discard the message — the replay ring is the source of
    // truth for a slow client. Discarding *silently* is what this used to do,
    // and it made that recovery unreachable: the client had no way to learn it
    // had missed anything, so it never re-attached and kept rendering a
    // terminal with a hole in it.
    subs.retain_mut(|sub| {
        // A pending gap is announced before any further output, so the client
        // re-attaches rather than compositing new bytes onto a stale grid.
        // This cannot be sent at the moment the gap opens: the queue being
        // full is precisely what caused it, so the signal waits for room.
        if sub.gapped {
            match sub.tx.try_send(Notification::Gapped {
                pty_id: pty_id_of(&notif).to_owned(),
                attachment_id: sub.attachment_id,
            }) {
                Ok(()) => sub.gapped = false,
                // Still backed up. Keep the flag and try again next time
                // rather than dropping the notice along with the bytes.
                Err(TrySendError::Full(_)) => return true,
                Err(TrySendError::Closed(_)) => return false,
            }
        }
        // Address each copy to the attachment it is going to. The daemon
        // already sent one per attachment; naming it is what lets a client
        // holding several on one PTY route them to the right subscriber
        // instead of showing every byte once per attachment it holds.
        match sub.tx.try_send(notif.clone().addressed_to(sub.attachment_id)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::warn!("subscriber queue full; dropping notification");
                sub.gapped = true;
                true
            }
            Err(TrySendError::Closed(_)) => false,
        }
    });
}

/// The session a notification belongs to. Every variant carries one, so a gap
/// notice can be addressed to the same session as the output it displaced.
fn pty_id_of(notif: &Notification) -> &str {
    match notif {
        Notification::Output { pty_id, .. }
        | Notification::Exit { pty_id, .. }
        | Notification::Attention { pty_id, .. }
        | Notification::Gapped { pty_id, .. } => pty_id,
    }
}

