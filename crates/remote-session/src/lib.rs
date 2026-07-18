//! The client-side remote-control session — the phone's Rust core.
//!
//! Pure Rust, **no FFI**, so it is unit-testable standalone: it speaks the
//! `remote-proto` wire protocol over the abstract
//! [`Transport`](oximux_remote_proto::Transport) seam, so the in-memory loopback
//! drives it against the real `remote-host` dispatcher with no network. iroh
//! becomes one `Transport` impl underneath, and `mobile-core` (uniffi) wraps this
//! to expose a typed API to React Native.
//!
//! Today: the [`ClientSigner`] app identity, and [`RemoteSession`] — the
//! Register/Connect/AuthProve handshake + one-shot RPCs. Later slices layer the
//! live subscription consumer (folding `HostEvent`s into a `ChatThread`) and the
//! reconnect state machine on top.

mod error;
mod session;
mod signer;

pub use error::SessionError;
pub use session::RemoteSession;
pub use signer::ClientSigner;
