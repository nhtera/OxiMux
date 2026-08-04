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
use oximux_remote_proto::messages::{StateEntryWire, StateSetReq};
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

    /// Fan one change out to watchers. No subscriber is normal.
    fn push_state_change(&self, key: &str, entry: Option<&CoordEntry>) {
        if let Some(events) = &self.state_events {
            let _ = events.send((key.to_string(), entry.map(to_wire)));
        }
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
