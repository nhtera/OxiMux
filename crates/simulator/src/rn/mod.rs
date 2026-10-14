//! React Native apps through Metro's debugger (React Native 0.83 or later):
//! finding the app on a device ([`targets`]), and reading what the debugger
//! reports — requests ([`network`]) and CPU profiles ([`profile`]) — with
//! secrets redacted ([`redact`]).
//!
//! Pure, apart from [`targets::discover`]'s HTTP probes: the WebSocket
//! session itself is the desktop's (this crate has no async runtime).

pub mod network;
pub mod profile;
pub mod redact;
pub mod targets;
