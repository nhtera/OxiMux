//! `oximux-agent-core` — the portable, dependency-minimal agent-chat core.
//!
//! Holds the `ThreadEvent` wire vocabulary, the `stream-json` decoder, and the
//! `ChatThread` fold shared by the desktop app (`oximux-agents`) and the phone's
//! Rust core (`mobile-core` / `remote-session`). No pty / sqlite / ACP / gpui.

pub mod thread;
