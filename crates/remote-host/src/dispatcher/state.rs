//! The coordination-state RPC handlers — the shared blackboard.
//!
//! Reads and writes are gated on the caller's *tier*, not on any session: the
//! board is host-wide by construction, because one narrowed to a single session
//! would coordinate nothing. See
//! [`may_read_state`](crate::auth::AuthStore::may_read_state) for why that is
//! defensible — the board holds only what agents deliberately put on it.
//!
//! A refused conditional write is **not** an `Error`: it comes back as the
//! entry the caller lost to, so a losing writer can merge and retry without a
//! second round trip. The CLI turns that into its own exit code.

use oximux_agents::coord::CoordEntry;
use oximux_remote_proto::messages::{
    StateChangeWire, StateEntryWire, StateSetReq, StateWatchStartedWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

/// The longest key the board accepts. Long enough for a namespaced path
/// (`team/run-7/backend/claim`), short enough that a key cannot be used as a
/// value smuggled past the value column.
const MAX_KEY_LEN: usize = 256;

/// The largest value the board accepts, in bytes.
///
/// A blackboard is for facts agents agree on, not for payloads: anything that
/// needs more than this belongs in a file the agents can both read. The cap
/// also keeps a `StateWatch` snapshot inside one transport frame for any
/// plausible number of keys.
const MAX_VALUE_BYTES: usize = 64 * 1024;

impl Dispatcher {
    /// Read one key.
    pub(super) fn state_get(&self, peer: &Peer, key: &str) -> Response {
        if !self.auth.may_read_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.coord.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match store.get(key) {
            Ok(entry) => Response::StateValue(entry.as_ref().map(to_wire)),
            Err(e) => {
                tracing::warn!(error = %e, "reading coordination state failed");
                Response::Error(RpcError::Internal("could not read that key".into()))
            }
        }
    }

    /// Write one key, optionally conditional on its version.
    pub(super) fn state_set(&self, peer: &Peer, req: StateSetReq) -> Response {
        if !self.auth.may_write_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.coord.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        if let Err(why) = validate(&req.key, &req.value_json) {
            return Response::Error(RpcError::BadRequest(why));
        }
        match store.set(&req.key, &req.value_json, req.if_version, self.now_local()) {
            Ok(Ok(entry)) => {
                self.push_state_change(&req.key, Some(&entry));
                Response::StateValue(Some(to_wire(&entry)))
            }
            // The conditional write lost. Its own variant, carrying the current
            // entry: a caller must be able to tell "I wrote" from "someone else
            // did" even when the winning value happens to equal the one it was
            // trying to store.
            Ok(Err(conflict)) => Response::StateConflict(conflict.current.as_ref().map(to_wire)),
            Err(e) => {
                tracing::warn!(error = %e, "writing coordination state failed");
                Response::Error(RpcError::Internal("could not write that key".into()))
            }
        }
    }

    /// Delete one key. Idempotent.
    pub(super) fn state_delete(&self, peer: &Peer, key: &str) -> Response {
        if !self.auth.may_write_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.coord.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match store.delete(key) {
            Ok(()) => {
                self.push_state_change(key, None);
                Response::Ack
            }
            Err(e) => {
                tracing::warn!(error = %e, "deleting coordination state failed");
                Response::Error(RpcError::Internal("could not delete that key".into()))
            }
        }
    }

    /// The baseline a watcher starts from: every matching entry as it stands
    /// now, before any pushed change.
    pub(super) fn state_snapshot(&self, peer: &Peer, prefix: Option<&str>) -> Response {
        if !self.auth.may_read_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.coord.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match store.list(prefix) {
            Ok(entries) => Response::StateSnapshot(entries.iter().map(to_wire).collect()),
            Err(e) => {
                tracing::warn!(error = %e, "listing coordination state failed");
                Response::Error(RpcError::Internal("could not read the board".into()))
            }
        }
    }

    /// The cursor-aware baseline or replay a [`Request::StateWatchFrom`] starts
    /// with.
    ///
    /// `since_seq: None` is a fresh watch: the board, plus the cursor to resume
    /// from. `Some(n)` resumes: the ring replays the gap when it still covers
    /// it, and otherwise the watcher gets a baseline — which is the signal that
    /// it lost transitions, and the thing v18's `StateWatch` could not say.
    pub(super) fn state_watch_from(
        &self,
        peer: &Peer,
        prefix: Option<&str>,
        since_seq: Option<u64>,
    ) -> Response {
        if !self.auth.may_read_state(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(store) = self.coord.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        // The head is read BEFORE the board, so a write landing between the two
        // is replayed by the push stream rather than dropped. The reverse order
        // loses it: it would be absent from the snapshot and already below the
        // cursor. Re-delivering one change is harmless — the entry carries its
        // own version — whereas losing one is the bug this whole cursor exists
        // to prevent.
        let (head, replay) = self.state_log.replay_since(prefix, since_seq);
        if let Some(replay) = replay {
            return Response::StateWatchStarted(StateWatchStartedWire {
                seq: head,
                baseline: None,
                replay,
            });
        }
        match store.list(prefix) {
            Ok(entries) => Response::StateWatchStarted(StateWatchStartedWire {
                seq: head,
                baseline: Some(entries.iter().map(to_wire).collect()),
                replay: Vec::new(),
            }),
            Err(e) => {
                tracing::warn!(error = %e, "listing coordination state failed");
                Response::Error(RpcError::Internal("could not read the board".into()))
            }
        }
    }

    /// Fan one change out to watchers. No subscriber is normal.
    ///
    /// The sequence is assigned here, under the log's lock, so the number a
    /// watcher resumes from and the number recorded in the ring cannot disagree
    /// — two concurrent writers taking their seq from separate places would
    /// interleave the ring out of order and make a replay silently wrong.
    fn push_state_change(&self, key: &str, entry: Option<&CoordEntry>) {
        let change = StateChangeWire {
            seq: 0, // replaced under the lock
            key: key.to_string(),
            entry: entry.map(to_wire),
        };
        let change = self.state_log.record(change);
        if let Some(events) = &self.state_events {
            let _ = events.send(change);
        }
    }
}

/// The host's recent coordination changes, so a reconnecting watcher can resume
/// instead of re-reading the board and hoping.
///
/// Deliberately in memory and bounded. The board itself is the durable record;
/// this is only the *recent transitions*, and keeping it in SQLite would mean a
/// schema, a migration, and tombstones for deletes — for data whose entire
/// value expires within seconds of a reconnect. A cursor that outlives the ring
/// is not an error: it resyncs, exactly like a session stream whose backlog
/// aged out.
pub(super) struct StateLog {
    inner: std::sync::Mutex<StateLogInner>,
}

struct StateLogInner {
    /// Newest last. Bounded by [`RING_CAPACITY`].
    ring: std::collections::VecDeque<StateChangeWire>,
    /// The seq most recently issued. Starts at 0, so the first change is 1 and
    /// a caller resuming from 0 means "everything the ring still holds".
    head: u64,
}

/// How many changes the ring keeps.
///
/// A watcher reconnecting inside a few seconds is the case worth covering, and
/// on a board sized for coordination rather than telemetry that is far fewer
/// than this. Past it, resyncing costs one board read.
const RING_CAPACITY: usize = 1024;

impl Default for StateLog {
    fn default() -> Self {
        Self {
            inner: std::sync::Mutex::new(StateLogInner {
                ring: std::collections::VecDeque::with_capacity(RING_CAPACITY),
                head: 0,
            }),
        }
    }
}

impl StateLog {
    /// Assign the next sequence, record it, and hand back the numbered change.
    fn record(&self, mut change: StateChangeWire) -> StateChangeWire {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.head += 1;
        change.seq = inner.head;
        if inner.ring.len() == RING_CAPACITY {
            inner.ring.pop_front();
        }
        inner.ring.push_back(change.clone());
        change
    }

    /// `(head, replay)` — `replay` is `None` when the caller must be resynced:
    /// it asked for no cursor, or its cursor is older than the ring still holds
    /// (a long absence, or a host restart, which resets the counter).
    ///
    /// A cursor *ahead* of the head also resyncs. That is a cursor from a
    /// previous boot of this host, and honouring it would silently deliver
    /// nothing while the watcher believed it was current.
    fn replay_since(
        &self,
        prefix: Option<&str>,
        since_seq: Option<u64>,
    ) -> (u64, Option<Vec<StateChangeWire>>) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let head = inner.head;
        let Some(since) = since_seq else { return (head, None) };
        if since > head {
            return (head, None);
        }
        // Coverage is about what the ring *holds*, not about how much the
        // caller missed: an oldest entry of `since + 1` is an exact join.
        let oldest = inner.ring.front().map(|c| c.seq);
        let covered = match oldest {
            Some(oldest) => oldest <= since + 1,
            // An empty ring covers any cursor at the head: nothing has happened
            // since, so there is nothing to have missed.
            None => since == head,
        };
        if !covered {
            return (head, None);
        }
        let replay = inner
            .ring
            .iter()
            .filter(|c| c.seq > since)
            .filter(|c| prefix.is_none_or(|p| c.key.starts_with(p)))
            .cloned()
            .collect();
        (head, Some(replay))
    }
}

/// What the board refuses outright, with the reason. Checked here rather than
/// in the store so the store stays a store — and so a local write from the
/// desktop meets the same limits a remote one does.
fn validate(key: &str, value_json: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("a key cannot be empty".into());
    }
    if key.len() > MAX_KEY_LEN {
        return Err(format!("a key may be at most {MAX_KEY_LEN} bytes"));
    }
    if value_json.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "a value may be at most {} KiB — put larger data in a file",
            MAX_VALUE_BYTES / 1024
        ));
    }
    // Parsed, not merely length-checked: the column is documented as JSON, and
    // a watcher decoding the board should not have to defend against a writer
    // that put prose in it.
    serde_json::from_str::<serde_json::Value>(value_json)
        .map(|_| ())
        .map_err(|e| format!("the value must be JSON: {e}"))
}

fn to_wire(entry: &CoordEntry) -> StateEntryWire {
    StateEntryWire {
        key: entry.key.clone(),
        value_json: entry.value_json.clone(),
        version: entry.version,
        updated_at: entry.updated_at.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_json_value_is_refused_with_the_reason() {
        let err = validate("k", "not json").expect_err("prose is not JSON");
        assert!(err.contains("must be JSON"), "{err}");
    }

    #[test]
    fn an_empty_key_is_refused() {
        assert!(validate("", "1").is_err());
    }

    #[test]
    fn an_oversize_value_is_refused_before_it_reaches_the_store() {
        let big = format!("\"{}\"", "x".repeat(MAX_VALUE_BYTES));
        let err = validate("k", &big).expect_err("over the cap");
        assert!(err.contains("KiB"), "{err}");
    }

    #[test]
    fn ordinary_json_passes() {
        validate("team/run-7/claim", r#"{"files":["a.rs"]}"#).expect("valid");
    }
}

#[cfg(test)]
mod state_log_tests {
    use super::*;

    fn change(key: &str) -> StateChangeWire {
        StateChangeWire { seq: 0, key: key.into(), entry: None }
    }

    /// Sequences are dense and start at 1, so 0 is always "before everything"
    /// and a caller can resume from it without a special case.
    #[test]
    fn sequences_start_at_one_and_are_dense() {
        let log = StateLog::default();
        assert_eq!(log.record(change("a")).seq, 1);
        assert_eq!(log.record(change("b")).seq, 2);
        assert_eq!(log.record(change("c")).seq, 3);
    }

    /// No cursor means a baseline: the caller is asking to be caught up from
    /// nothing, and the board is what that means.
    #[test]
    fn no_cursor_asks_for_a_baseline() {
        let log = StateLog::default();
        log.record(change("a"));
        let (head, replay) = log.replay_since(None, None);
        assert_eq!(head, 1);
        assert!(replay.is_none(), "a fresh watch is resynced, not replayed");
    }

    /// A cursor the ring still covers replays exactly the gap — including the
    /// exact-join boundary, where the ring's oldest entry is the very next one
    /// after the cursor. Off by one here and every reconnect resyncs.
    #[test]
    fn a_covered_cursor_replays_exactly_the_gap() {
        let log = StateLog::default();
        for key in ["a", "b", "c"] {
            log.record(change(key));
        }
        let (head, replay) = log.replay_since(None, Some(1));
        assert_eq!(head, 3);
        let replay = replay.expect("covered");
        assert_eq!(replay.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![2, 3]);

        // The exact join: cursor 0, ring starting at 1.
        let (_, replay) = log.replay_since(None, Some(0));
        assert_eq!(replay.expect("covered").len(), 3);
    }

    /// Caught up means an empty replay, not a resync. The two are very
    /// different to a watcher: one says "nothing happened", the other says
    /// "you may have missed something".
    #[test]
    fn a_cursor_at_the_head_replays_nothing_rather_than_resyncing() {
        let log = StateLog::default();
        log.record(change("a"));
        let (head, replay) = log.replay_since(None, Some(1));
        assert_eq!(head, 1);
        assert_eq!(replay.expect("covered").len(), 0);

        // And on a host where nothing has ever been written.
        let empty = StateLog::default();
        let (head, replay) = empty.replay_since(None, Some(0));
        assert_eq!(head, 0);
        assert_eq!(replay.expect("covered at head").len(), 0);
    }

    /// A cursor older than the ring holds must resync — replaying only what
    /// survives would deliver a partial gap while looking complete.
    #[test]
    fn a_cursor_older_than_the_ring_resyncs() {
        let log = StateLog::default();
        for i in 0..(RING_CAPACITY + 10) {
            log.record(change(&format!("k{i}")));
        }
        let (head, replay) = log.replay_since(None, Some(1));
        assert_eq!(head as usize, RING_CAPACITY + 10);
        assert!(replay.is_none(), "the span aged out, so the answer is a baseline");

        // The newest end still replays.
        let (_, replay) = log.replay_since(None, Some(head - 1));
        assert_eq!(replay.expect("covered").len(), 1);
    }

    /// A cursor AHEAD of the head is a cursor from a previous boot of this
    /// host, since the counter is in memory and restarts at zero. Honouring it
    /// would deliver nothing while the watcher believed it was current — the
    /// exact silent staleness the cursor exists to end.
    #[test]
    fn a_cursor_from_a_previous_boot_resyncs() {
        let log = StateLog::default();
        log.record(change("a"));
        let (head, replay) = log.replay_since(None, Some(9_999));
        assert_eq!(head, 1);
        assert!(replay.is_none());
    }

    /// The replay is narrowed to the watcher's prefix, like the live stream.
    #[test]
    fn a_replay_is_narrowed_to_the_prefix() {
        let log = StateLog::default();
        log.record(change("team/a"));
        log.record(change("other/b"));
        log.record(change("team/c"));
        let (_, replay) = log.replay_since(Some("team/"), Some(0));
        let keys: Vec<_> = replay.expect("covered").into_iter().map(|c| c.key).collect();
        assert_eq!(keys, vec!["team/a", "team/c"]);
    }
}
