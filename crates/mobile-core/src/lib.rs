//! The phone's Rust core — a uniffi binding over the pure-Rust `remote-session`
//! client and the `remote-iroh` transport.
//!
//! Exposes a typed, async + streamed-callback surface the React Native app drives
//! over generated Swift/Kotlin/JSI bindings:
//! - [`MobileClient`] — connect to a paired host (iroh), then list sessions, send
//!   prompts, resolve permissions, and steer/cancel, all as async methods.
//! - [`EventSink`] — a foreign callback the core pushes folded [`RemoteEvent`]s
//!   into for a subscribed session (the live `HostEvent` stream).
//! - [`ConnStateListener`] — connection-state transitions for the UI.
//!
//! The core owns the connection: the demux pump and the event dispatcher run on a
//! dedicated runtime, while the async FFI methods run under uniffi's tokio bridge.

mod callbacks;
mod client;
mod ffi_types;
mod runtime;
mod subscription;

uniffi::setup_scaffolding!();

pub use callbacks::{ConnStateListener, EventSink};
pub use client::MobileClient;
pub use ffi_types::{ConnState, MobileError, PermissionReply, RemoteEvent, SessionSummary};
