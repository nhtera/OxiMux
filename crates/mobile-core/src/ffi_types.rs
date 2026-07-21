//! The FFI value types crossing to Swift/Kotlin/JS, and their conversions from
//! the wire types. Kept a thin, stable projection of `remote-proto` so the RN app
//! never sees the postcard/JSON envelope details.

use oximux_remote_proto::messages::SessionSummary as WireSummary;
use oximux_remote_proto::proto::{Choice as WireChoice, SessionChoices as WireChoices};

/// One selectable model or permission mode.
///
/// `id` is what goes back over the wire; `label` is what a person reads. They
/// stay separate because a backend's identifier is usually not presentable
/// (`claude-opus-4-8` against "Opus 4.8").
#[derive(Debug, Clone, uniffi::Record)]
pub struct Choice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

impl From<WireChoice> for Choice {
    fn from(w: WireChoice) -> Self {
        Self { id: w.id, label: w.label, description: w.description }
    }
}

/// What a session's backend offers its pickers, plus what is active now so the
/// app can mark the current entry without a second round trip.
///
/// Both lists may legitimately be empty — that means "nothing to choose
/// between", not a failure.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SessionChoices {
    pub models: Vec<Choice>,
    pub modes: Vec<Choice>,
    pub current_model: Option<String>,
    pub current_mode: Option<String>,
}

impl From<WireChoices> for SessionChoices {
    fn from(w: WireChoices) -> Self {
        Self {
            models: w.models.into_iter().map(Choice::from).collect(),
            modes: w.modes.into_iter().map(Choice::from).collect(),
            current_model: w.current_model,
            current_mode: w.current_mode,
        }
    }
}

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

/// An image attached to a prompt.
///
/// A typed record rather than a JSON string (the shape `answer_question` uses):
/// there are only two fields and neither is dynamic, so the app gets real types
/// and a malformed attachment becomes impossible instead of a parse error.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ChatImage {
    /// An image MIME type, e.g. `image/jpeg`. Passed through to the agent, which
    /// decides what it accepts.
    pub media_type: String,
    /// The image bytes, base64. Note this inflates the payload by ~4/3, which is
    /// what the send-size ceiling is measured against.
    pub data: String,
}

impl From<ChatImage> for oximux_agent_core::thread::ChatImage {
    fn from(i: ChatImage) -> Self {
        Self { media_type: i.media_type, data: i.data }
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
    /// Allow *and* apply the shortcut the agent offered alongside the request —
    /// the "always allow edits this session" class of button.
    ///
    /// `suggestion_json` is one of the `suggestions` the app received on the
    /// `PermissionRequested` event, quoted back **verbatim**. Its `raw` payload
    /// is opaque to both the phone and the host; only the agent that offered it
    /// interprets it. Echoing rather than reconstructing means the phone cannot
    /// invent a suggestion the agent never made, so this adds no trust boundary
    /// beyond the existing resolve path.
    AllowWithSuggestion { updated_input_json: String, suggestion_json: String },
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

/// One terminal the phone can list and attach to.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalInfo {
    pub pty_id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
}

impl From<oximux_remote_proto::messages::TerminalSummary> for TerminalInfo {
    fn from(t: oximux_remote_proto::messages::TerminalSummary) -> Self {
        Self { pty_id: t.pty_id, cwd: t.cwd, cols: t.cols, rows: t.rows }
    }
}

/// A terminal's replay snapshot and the grid it was drawn at.
///
/// The dims are not advisory. The replay bytes carry absolute-position escape
/// sequences produced by a process drawing into a grid of exactly this size, so
/// an emulator built at any other size renders them into the wrong cells. The
/// app MUST size its emulator from these before feeding it `replay`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TerminalScreen {
    pub replay: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}
