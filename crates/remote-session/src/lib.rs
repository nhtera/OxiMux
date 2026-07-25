//! The client-side remote-control session — the phone's Rust core.
//!
//! Pure Rust, **no FFI**, so it is unit-testable standalone: it speaks the
//! `remote-proto` wire protocol over the abstract
//! [`Transport`](oximux_remote_proto::Transport) seam, so the in-memory loopback
//! drives it against the real `remote-host` dispatcher with no network. iroh
//! becomes one `Transport` impl underneath, and `mobile-core` (uniffi) wraps this
//! to expose a typed API to React Native.
//!
//! Today: the [`ClientSigner`] app identity; [`RemoteSession`] — the
//! Register/Connect/AuthProve handshake, one-shot RPCs, and `subscribe` — driven
//! over a concurrent [`demux`] pump so RPCs and a live event stream share one
//! connection; [`SessionSubscription`] — the client-side fold of a session's
//! `HostEvent` stream into a `ChatThread` with the gap→resync contract; the
//! reconnect policy — the [`Connector`] dial seam plus the pure [`Reconnect`] state
//! machine (backoff + give-up); and [`maintain_connection`] — the runtime-agnostic
//! driver that ties the [`Connector`], the [`Reconnect`] policy, a [`Sleeper`], and
//! the demux pump into a self-healing connection. The iroh transport lands as a
//! [`Connector`] + [`Transport`](oximux_remote_proto::Transport) impl beneath it.

mod connector;
mod demux;
mod driver;
mod error;
mod reconnect;
mod session;
mod signer;
mod subscription;

pub use connector::{ConnectError, Connector};
pub use demux::{DemuxPump, EventStream, SessionsStream, TerminalPush, TerminalStream};
pub use driver::{Bootstrap, Sleeper, maintain_connection};
pub use error::SessionError;
pub use reconnect::{ConnAction, ConnState, Reconnect};
pub use session::RemoteSession;
pub use signer::ClientSigner;
pub use subscription::{FoldOutcome, SessionSubscription};
