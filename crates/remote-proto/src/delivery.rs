//! Wire types for safe delivery and session health (v37): a send that is
//! never delivered twice under the same key, and a pushed frame saying what a
//! session is doing now.
//!
//! The postcard types here are frozen once shipped, like every other wire
//! type. The health itself crosses as JSON inside [`SessionHealthFrame`], so a
//! later version can add fields (and status words) without a bump: an older
//! decoder skips a field it does not know and defaults one it does not get.

use serde::{Deserialize, Serialize};

use oximux_agent_core::thread::ChatImage;

/// The longest idempotency key a host takes, in bytes.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// [`Request::SendV2`](crate::proto::Request::SendV2) payload: a prompt with a
/// caller-chosen key. A send under a key the host has seen is not sent again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SendV2Req {
    pub session_id: String,
    pub text: String,
    pub images: Vec<ChatImage>,
    /// Unique per message, reused on every retry of that message. Stored only
    /// as a hash; non-empty, at most [`MAX_IDEMPOTENCY_KEY_BYTES`].
    pub idempotency_key: String,
    /// Client-minted correlation id, as [`SendPromptReq`](crate::messages::SendPromptReq).
    pub corr_id: u64,
}

/// Where an earlier send under the same key stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceiptStateWire {
    /// Taken, not yet started on (or still waiting in the queue).
    Pending,
    /// The agent started on it.
    Accepted,
    /// The provider refused it before producing anything.
    Rejected,
    /// The host went away before it knew: the agent may or may not have it.
    Unknown,
}

/// [`Response::SendOutcome`](crate::proto::Response::SendOutcome) payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendOutcomeWire {
    /// New key: the send is queued (or written) and the key is recorded.
    Queued,
    /// The key was seen before: nothing was sent. Sending the same key again
    /// is how a caller asks where the first send stands.
    Duplicate(ReceiptStateWire),
}

/// [`Response::SessionHealth`](crate::proto::Response::SessionHealth): pushed
/// to a v37 subscriber when it subscribes and whenever the session's health
/// changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHealthFrame {
    pub session_id: String,
    /// Random per host process. The same id after a reconnect means the same
    /// host (its sessions may still run); a new one means it restarted.
    pub host_instance_id: String,
    /// A [`SessionHealthWire`] as JSON. Private so a frame is only built from
    /// one: see [`SessionHealthFrame::new`].
    health_json: String,
}

impl SessionHealthFrame {
    pub fn new(session_id: impl Into<String>, host_instance_id: impl Into<String>, health: &SessionHealthWire) -> Self {
        Self {
            session_id: session_id.into(),
            host_instance_id: host_instance_id.into(),
            // A struct of strings, numbers and options: encoding cannot fail.
            health_json: serde_json::to_string(health).unwrap_or_default(),
        }
    }

    pub fn health(&self) -> serde_json::Result<SessionHealthWire> {
        serde_json::from_str(&self.health_json)
    }
}

/// A session's health. Times are Unix milliseconds. Every field defaults, so
/// a frame from a newer host with fewer or more fields still decodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionHealthWire {
    /// `idle | working | needsPermission | needsAnswer | limited | failed |
    /// interrupted`; a word a client does not know reads as working.
    pub status: String,
    /// `live | unverifiable | restoredUnconfirmed | exited`.
    pub liveness: String,
    /// With `exited`: `exitCode | userStop | hostLock | connectionClosed`.
    pub exit_proof: Option<String>,
    /// With `exitCode`.
    pub exit_code: Option<i32>,
    pub limited_until: Option<i64>,
    pub stalled_since: Option<i64>,
    pub exited_unexpectedly: bool,
    pub reason: Option<String>,
    pub last_activity_at: i64,
    pub pending_unanswered_send: bool,
    pub held_restored: u32,
    /// Edge identities: a client notifies once per turn and per exit.
    pub turn_id: Option<String>,
    pub exit_seq: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_round_trips_and_tolerates_fields_either_side_lacks() {
        let wire = SessionHealthWire {
            status: "limited".into(),
            liveness: "live".into(),
            limited_until: Some(1_700_000_000_000),
            turn_id: Some("1:3".into()),
            ..Default::default()
        };
        let frame = SessionHealthFrame::new("s", "host-a", &wire);
        assert_eq!(frame.health().unwrap(), wire);

        // A newer host's extra field is skipped; a missing one defaults.
        let newer = SessionHealthFrame {
            session_id: "s".into(),
            host_instance_id: "h".into(),
            health_json: r#"{"status":"working","attention":"high"}"#.into(),
        };
        let read = newer.health().unwrap();
        assert_eq!(read.status, "working");
        assert_eq!(read.liveness, "");
        assert!(!read.exited_unexpectedly);
    }
}
