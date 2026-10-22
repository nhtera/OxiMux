//! Per-terminal program-status bookkeeping: the record store, whether this
//! terminal answers feature queries, the handshake bit, and which root changes
//! are worth an event. `TerminalState` owns one and routes the scanner's hits
//! here; it holds no I/O — a reply it admits is written by the backend.

use std::sync::Arc;
use std::time::Instant;

use super::{ProgramRecord, ProgramReport, ProgramStatusStore, ReplyGate, ReplyTicket};

/// What a hit means for the terminal's consumers: the root they should now
/// act on (`None` = nobody is reporting), or nothing new.
pub(crate) type RootChange = Option<Option<ProgramRecord>>;

#[derive(Default)]
pub(crate) struct ProgramStatusTracker {
    store: ProgramStatusStore,
    /// A feature query was seen in this stream, live or replayed.
    handshake: bool,
    /// `Some` when this terminal owns the PTY's replies (see `set_answers_queries`).
    gate: Option<ReplyGate>,
    /// A reply admitted by the gate and not yet taken by the backend.
    pending_reply: Option<Arc<ReplyTicket>>,
    /// At this point in the stream a program is reporting: the last root
    /// consumers were (or, for a replay, would have been) handed is `Some`.
    reported: bool,
}

impl ProgramStatusTracker {
    /// Whether this terminal answers OSC 7501 queries itself. Exactly one party
    /// per PTY may: the daemon for a current relay daemon, else the backend
    /// that owns the PTY. Never a mirror of someone else's stream.
    pub(crate) fn set_answers_queries(&mut self, answers: bool) {
        if answers != self.gate.is_some() {
            self.gate = answers.then(ReplyGate::default);
        }
    }

    pub(crate) fn answers_queries(&self) -> bool {
        self.gate.is_some()
    }

    pub(crate) fn handshake(&self) -> bool {
        self.handshake
    }

    #[cfg(test)]
    pub(crate) fn root(&self) -> Option<&ProgramRecord> {
        self.store.root()
    }

    /// Carry what a rebuilt terminal cannot re-derive from a replay: who
    /// answers, and a handshake whose query may have left the replay ring.
    pub(crate) fn inherit(&mut self, from: &Self) {
        self.set_answers_queries(from.answers_queries());
        self.handshake |= from.handshake;
    }

    /// A feature query. Only a live one, on an owning terminal, is answered.
    pub(crate) fn on_query(&mut self, live: bool, now: Instant) {
        self.handshake = true;
        if live
            && self.pending_reply.is_none()
            && let Some(ticket) = self.gate.as_mut().and_then(|g| g.admit(now))
        {
            self.pending_reply = Some(ticket);
        }
    }

    pub(crate) fn take_reply(&mut self) -> Option<Arc<ReplyTicket>> {
        self.pending_reply.take()
    }

    pub(crate) fn on_report(&mut self, report: ProgramReport) -> RootChange {
        // A report identical to the record a release left behind (the same
        // `done` again, after a prompt) is still news to consumers.
        if self.store.apply(report) || (!self.reported && self.store.root().is_some()) {
            self.publish()
        } else {
            None
        }
    }

    /// RIS: every record goes, and so does the handshake — a reset terminal
    /// starts over.
    pub(crate) fn on_reset(&mut self) -> RootChange {
        self.handshake = false;
        self.store.reset();
        self.release()
    }

    /// OSC 133 A: the shell is back, so whatever reported has finished. Its
    /// `done`/`error` record stays in the store, but consumers are told nobody
    /// is reporting.
    pub(crate) fn on_prompt(&mut self) -> RootChange {
        self.store.end_activity();
        self.release()
    }

    /// The root consumers should hold right now: the store's root, unless a
    /// release (prompt, reset) has come since. After a replay this is where
    /// consumers start from.
    pub(crate) fn consumer_root(&self) -> Option<ProgramRecord> {
        if self.reported { self.store.root().cloned() } else { None }
    }

    fn publish(&mut self) -> RootChange {
        let root = self.store.root().cloned();
        if root.is_none() && !self.reported {
            return None;
        }
        self.reported = root.is_some();
        Some(root)
    }

    fn release(&mut self) -> RootChange {
        if !self.reported {
            return None;
        }
        self.reported = false;
        Some(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program_status::{ProgramState, parse_report};

    fn report(p: &str) -> ProgramReport {
        parse_report(p.as_bytes()).unwrap()
    }

    #[test]
    fn only_a_live_query_on_an_owner_is_answered() {
        let now = Instant::now();
        let mut mirror = ProgramStatusTracker::default();
        mirror.on_query(true, now);
        assert!(mirror.take_reply().is_none());
        assert!(mirror.handshake());

        let mut owner = ProgramStatusTracker::default();
        owner.set_answers_queries(true);
        owner.on_query(false, now);
        assert!(owner.take_reply().is_none(), "replayed queries are never answered");
        owner.on_query(true, now);
        assert!(owner.take_reply().is_some());
    }

    #[test]
    fn changes_are_published_and_a_prompt_releases_once() {
        let mut t = ProgramStatusTracker::default();
        let change = t.on_report(report("state=done"));
        assert_eq!(change.unwrap().unwrap().state, ProgramState::Done);
        assert_eq!(t.on_prompt(), Some(None));
        assert_eq!(t.on_prompt(), None, "nothing new to release");
        // `done` survives in the store per the protocol.
        assert_eq!(t.root().unwrap().state, ProgramState::Done);
        assert_eq!(t.on_report(report("state=working")).unwrap().unwrap().state, ProgramState::Working);
    }

    #[test]
    fn the_same_report_after_a_release_is_published_again() {
        let mut t = ProgramStatusTracker::default();
        t.on_report(report("state=done"));
        t.on_prompt();
        assert_eq!(t.consumer_root(), None);
        assert_eq!(t.on_report(report("state=done")).unwrap().unwrap().state, ProgramState::Done);
        assert_eq!(t.on_report(report("state=done")), None);
    }

    #[test]
    fn reset_drops_records_and_handshake() {
        let mut t = ProgramStatusTracker::default();
        t.on_query(true, Instant::now());
        t.on_report(report("state=working"));
        assert_eq!(t.on_reset(), Some(None));
        assert!(!t.handshake());
        assert!(t.root().is_none());
    }

    #[test]
    fn inherit_keeps_the_owner_and_the_handshake() {
        let mut old = ProgramStatusTracker::default();
        old.set_answers_queries(true);
        old.on_query(true, Instant::now());
        let mut fresh = ProgramStatusTracker::default();
        fresh.inherit(&old);
        assert!(fresh.answers_queries());
        assert!(fresh.handshake());
    }
}
