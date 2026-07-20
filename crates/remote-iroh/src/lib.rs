//! The iroh P2P transport for OxiMux remote control.
//!
//! Provides the production [`Transport`](oximux_remote_proto::transport::Transport)
//! and [`Connector`](oximux_remote_session::Connector) impls that sit beneath the
//! pure-Rust `remote-session` client and the transport-agnostic `remote-host`
//! dispatcher:
//! - [`IrohTransport`] — a framed message channel over one iroh QUIC bi-stream.
//! - [`IrohConnector`] — the client dial seam (resolve host by endpoint id → open
//!   a bi-stream), driven by `remote_session::maintain_connection`.
//! - [`bind_client`] / [`bind_host`] / [`accept`] — endpoint setup + the host
//!   accept loop that hands each connection's transport to the dispatcher.
//!
//! Kept separate from `remote-session` (which stays iroh-free and loopback-
//! testable) and from `remote-proto` (which stays dependency-minimal), so both
//! the desktop host and the mobile core share one framing implementation.

mod connector;
mod endpoint;
#[cfg(feature = "host")]
mod host;
mod transport;

pub use connector::IrohConnector;
pub use endpoint::{accept, bind_client, bind_host};
#[cfg(feature = "host")]
pub use host::{HostHandle, serve_host, start_host};
pub use transport::IrohTransport;

/// The ALPN both sides negotiate. Bump the trailing version on any breaking wire
/// change (kept in lock-step with `remote_proto::PROTOCOL_VERSION`).
pub const OXIMUX_ALPN: &[u8] = b"oximux/remote/1";
