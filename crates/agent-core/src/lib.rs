//! `oximux-agent-core` — the portable, dependency-minimal agent-chat core.
//!
//! Holds the `ThreadEvent` wire vocabulary, the `stream-json` decoder, and the
//! `ChatThread` fold shared by the desktop app (`oximux-agents`) and the phone's
//! Rust core (`mobile-core` / `remote-session`). No pty / sqlite / ACP / gpui.

pub mod thread;

/// Screenshot redaction for transcripts on their way off this machine.
///
/// Lives here rather than next to the screen-control driver that produces the
/// screenshots, because the two have opposite platform shapes: capture is
/// macOS-only, but a transcript containing captures can be *read* anywhere —
/// agent CLIs keep their session stores in the user's home directory, and those
/// get synced between machines. A Windows build that dropped the filter along
/// with the driver would serve those screenshots to a paired phone. Scrubbing
/// is pure JSON work with no platform surface, so it simply always compiles.
pub mod redact;

/// The screen-control MCP server's tool-naming contract — what `redact` matches
/// on. Separated from the driver for the same reason as `redact` itself.
pub mod screen_tools;
