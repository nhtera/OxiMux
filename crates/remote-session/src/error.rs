//! The client session's failure taxonomy — distinguishes a transport drop from a
//! codec fault from a protocol-level rejection the host reported.

use oximux_remote_proto::RpcError;

/// A remote-session call failure.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The transport failed (closed mid-call, I/O error).
    #[error("transport error: {0}")]
    Transport(String),
    /// A wire encode/decode fault (never expected on a healthy connection).
    #[error("wire codec error: {0}")]
    Wire(String),
    /// The host closed the connection (a clean `None` on receive).
    #[error("host closed the connection")]
    Closed,
    /// The host's reply was too large for one frame, so it could not be
    /// assembled. Distinct from [`Self::Transport`] because the connection
    /// survives it — only this one call failed — and distinct from
    /// [`Self::Closed`], which is what this used to surface as, sending everyone
    /// hunting for a dropped link that never happened.
    #[error("the reply was {len} bytes, over the {cap}-byte limit for one message")]
    OversizeReply { len: usize, cap: usize },
    /// The host reported a protocol-level failure.
    #[error("host reported: {0:?}")]
    Rpc(RpcError),
    /// The host sent a response that doesn't fit the request — a protocol
    /// desync, so the field names which reply was expected.
    #[error("unexpected response (expected {expected})")]
    Unexpected { expected: &'static str },
    /// The two ends cannot understand each other. Distinct from
    /// [`Self::Rpc`]`(RpcError::IncompatibleVersion)`, which is the host
    /// refusing *us*; this is the client refusing the *host*. Both numbers are
    /// carried so the UI can say which side needs updating rather than showing a
    /// generic connection failure — the actionable difference for the user.
    #[error("incompatible protocol (this build speaks {ours}, host speaks {theirs})")]
    IncompatibleVersion { ours: u32, theirs: u32 },
}
