//! Remote-control wire protocol shared by the OxiMux desktop host and the
//! phone's Rust core.
//!
//! Two concerns, no transport:
//! - [`proto`] — the append-only RPC envelope (`Request`/`Response`), the
//!   [`HostEvent`](proto::HostEvent) stream frame, and their postcard codec.
//! - [`pairing`] — the [`PairingTicket`](pairing::PairingTicket) a QR carries
//!   and the `oximux://connect?ticket=` URL helpers.
//!
//! **Wire codec = postcard** (compact, non-self-describing). The envelope is
//! versioned and extended by appending variants — never by reordering or
//! removing them (postcard encodes enums by ordinal), mirroring the relay
//! protocol's [`PROTOCOL_VERSION`](proto::PROTOCOL_VERSION) discipline.
//!
//! **Why event/decision payloads ride as JSON strings:** `ThreadEvent` and
//! `PermissionDecision` carry `serde_json::Value` (arbitrary tool input/output).
//! postcard cannot deserialize a `Value` — it is non-self-describing, so
//! `Value`'s `deserialize_any` fails with `WontImplement`. Rather than fork a
//! shadow event type (and rather than string-wrapping the shared types, which
//! would silently change their already-shipped persisted JSON format), the
//! envelope carries these payloads as a `serde_json` string. JSON is
//! self-describing, handles `Value` natively, and is already the persistence
//! representation, so encoding is reused rather than reinvented.

pub mod messages;
pub mod pairing;
pub mod proto;

pub use messages::{
    AuthProveReq, ConnectReq, HostEvent, RegisterReq, ResolvePermissionReq, SendPromptReq,
    SessionInfoWire, SessionStatusWire, SessionSummary,
};
pub use pairing::{PairingError, PairingTicket};
pub use proto::{PROTOCOL_VERSION, Request, Response, RpcError, WireError};
