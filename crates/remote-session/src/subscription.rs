//! The client-side view of one subscribed session: the folded [`ChatThread`] plus
//! a resume cursor.
//!
//! Feed it the backlog (from `subscribe`/`events_since`) and then the live
//! [`HostEvent`] frames (`next_event`); it decodes each event and folds it into the
//! thread with the same `agent-core` fold the desktop uses. It also enforces the
//! wire's gap contract: a `seq` that skips ahead means the host's live ring lapped
//! (or a frame was dropped), so rather than fold out of order it reports a
//! [`FoldOutcome::Gap`] and the caller resyncs via `events_since(resume_from)`.

use oximux_agent_core::thread::ChatThread;
use oximux_remote_proto::HostEvent;

use crate::error::SessionError;

/// The result of folding one frame into a [`SessionSubscription`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldOutcome {
    /// Folded in order (or a duplicate that was ignored). `seq` is the subscription
    /// **cursor** afterwards — for a duplicate (`frame.seq <= cursor`) that is the
    /// unchanged cursor, NOT the fed frame's seq, so don't use it as a per-frame
    /// ack of the exact frame passed in.
    Applied { seq: u64 },
    /// A `seq` gap — one or more events were missed. The frame was **not** folded
    /// (applying out of order would corrupt the thread). The caller backfills with
    /// `events_since(resume_from)` and re-feeds those frames in order.
    ///
    /// **Permanent-gap caveat:** if `resume_from` predates the host's retained
    /// backlog (the bounded ring aged the missed span out), `events_since` returns
    /// a batch that *itself* starts ahead of the cursor, so re-feeding it yields
    /// `Gap { resume_from }` with the **same** `resume_from` again. A driver must
    /// treat an unchanged `resume_from` across an `events_since` round-trip as
    /// unrecoverable history loss and reset the subscription from a fresh snapshot
    /// (a full re-subscribe) rather than retrying forever. That recovery lives with
    /// the (deferred) reconnect/demux driver.
    Gap { resume_from: u64 },
}

/// One session as the client sees it: the folded [`ChatThread`] and the highest
/// `seq` folded so far.
pub struct SessionSubscription {
    session_id: String,
    thread: ChatThread,
    last_seq: u64,
}

impl SessionSubscription {
    /// A fresh subscription whose cursor is 0 — feed it from `after_seq: 0` (full
    /// backlog) or seed the cursor by folding a known-contiguous backlog first.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self { session_id: session_id.into(), thread: ChatThread::new(), last_seq: 0 }
    }

    /// The session this subscription tracks.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The folded thread — the read model a UI renders.
    pub fn thread(&self) -> &ChatThread {
        &self.thread
    }

    /// The highest `seq` folded so far — the resume cursor for `events_since`.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Fold one frame.
    ///
    /// - `frame.seq <= cursor` — a benign duplicate (a backlog entry re-sent live);
    ///   ignored, reported `Applied` at the current cursor.
    /// - `frame.seq == cursor + 1` — the next in order; decoded, folded, cursor
    ///   advances.
    /// - `frame.seq > cursor + 1` — a gap; **not** folded, returns
    ///   [`FoldOutcome::Gap`] so the caller resyncs from the cursor.
    pub fn apply(&mut self, frame: &HostEvent) -> Result<FoldOutcome, SessionError> {
        if frame.seq <= self.last_seq {
            return Ok(FoldOutcome::Applied { seq: self.last_seq });
        }
        if frame.seq > self.last_seq + 1 {
            return Ok(FoldOutcome::Gap { resume_from: self.last_seq });
        }
        let event = frame.event().map_err(|e| SessionError::Wire(e.to_string()))?;
        self.thread.apply(&event);
        self.last_seq = frame.seq;
        Ok(FoldOutcome::Applied { seq: frame.seq })
    }

    /// Fold a contiguous batch (a `subscribe`/`events_since` backlog reply),
    /// stopping at the first gap. Returns the last outcome.
    ///
    /// `events_since` is gapless **only within the host's retained window**: its
    /// reply is contiguous among the frames it returns, but its first frame can
    /// still sit ahead of this cursor if the missed span aged out of the host's
    /// bounded backlog — in which case this returns `Gap` with the same
    /// `resume_from`, the permanent-loss signal (see [`FoldOutcome::Gap`]).
    pub fn apply_batch(&mut self, frames: &[HostEvent]) -> Result<FoldOutcome, SessionError> {
        let mut last = FoldOutcome::Applied { seq: self.last_seq };
        for frame in frames {
            last = self.apply(frame)?;
            if matches!(last, FoldOutcome::Gap { .. }) {
                break;
            }
        }
        Ok(last)
    }
}
