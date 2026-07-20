//! The FFI value types crossing to Swift/Kotlin/JS, and their conversions from
//! the wire types. Kept a thin, stable projection of `remote-proto` so the RN app
//! never sees the postcard/JSON envelope details.

use oximux_remote_proto::messages::SessionSummary as WireSummary;

/// One agent session the phone lists.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: String,
    pub model: Option<String>,
    pub last_seq: u64,
    pub awaiting_permission: bool,
}

impl From<WireSummary> for SessionSummary {
    fn from(w: WireSummary) -> Self {
        Self {
            session_id: w.session_id,
            title: w.title,
            model: w.model,
            last_seq: w.last_seq,
            awaiting_permission: w.awaiting_permission,
        }
    }
}

/// The connection state the UI reflects, mirroring `remote_session::ConnState`.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
    Unreachable { cause: String },
}

/// The user's answer to a permission request (the FFI-simplified decision).
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PermissionReply {
    /// Allow this one call. `updated_input_json` MUST echo the tool's input (the
    /// app passes back the `input` it received on the `PermissionRequested`
    /// event, optionally edited) — the CLI treats an allow without it as
    /// malformed and denies the tool, so this is required, not optional.
    Allow { updated_input_json: String },
    /// Deny; `message` is shown to the model.
    Deny { message: String },
}

/// Every fallible FFI method surfaces one of these.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    #[error("not connected to a host")]
    NotConnected,
    #[error("the pairing ticket is invalid: {0}")]
    BadTicket(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("the transport could not be established: {0}")]
    Transport(String),
    #[error("the request failed: {0}")]
    Rpc(String),
}
