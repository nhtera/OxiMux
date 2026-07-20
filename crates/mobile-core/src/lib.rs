//! The phone's Rust core — a uniffi binding over the pure-Rust `remote-session`
//! client and the `remote-iroh` transport.
//!
//! Exposes a typed, async + streamed-callback surface the React Native app drives
//! over generated Swift/Kotlin/JSI bindings:
//! - [`MobileClient`] — connect to a paired host (iroh), then list sessions, send
//!   prompts, resolve permissions, and steer/cancel, all as async methods.
//! - [`ThreadSink`] — a foreign callback the core pushes a [`ThreadSnapshot`]
//!   into for a subscribed session: the transcript folded by `agent-core`, the
//!   same fold the desktop renders, so the app never reimplements one.
//! - [`ConnStateListener`] — connection-state transitions for the UI.
//!
//! The core owns the connection: the demux pump and the event dispatcher run on a
//! dedicated runtime, while the async FFI methods run under uniffi's tokio bridge.

mod callbacks;
mod client;
mod ffi_types;
mod runtime;
mod snapshot;
mod subscription;

uniffi::setup_scaffolding!();

pub use callbacks::{ConnStateListener, ThreadSink};
pub use client::MobileClient;
pub use ffi_types::{ChatImage, ConnState, MobileError, PermissionReply, SessionSummary};
pub use snapshot::ThreadSnapshot;
