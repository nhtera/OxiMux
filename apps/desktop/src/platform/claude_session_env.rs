//! Drop inherited Claude Code session markers before anything spawns.
//!
//! # The failure this exists to stop
//!
//! When OxiMux is launched from inside a Claude Code session — a dev shell,
//! a build script, an agent running `open` — the process inherits the
//! variables Claude Code stamps on its children (`CLAUDE_CODE_CHILD_SESSION`
//! and friends). Every terminal PTY and agent CLI OxiMux spawns inherits them
//! in turn, and a `claude` started there concludes it is a nested child of
//! that outer session: it switches transcript saving off ("Transcript saving
//! is off — inherited CLAUDE_CODE_CHILD_SESSION marker"), so the session
//! never reaches `~/.claude/projects` and the chat UI has nothing to import
//! or resume.
//!
//! OxiMux is a standalone cockpit, not a child agent session — the markers
//! are a launch artifact and never legitimate anywhere in this process tree.
//!
//! # Why the whole process, rather than each spawn site
//!
//! Same reasoning as [`super::login_path`]: scrubbing once at the top of
//! `main` covers every present and future `Command::new` and PTY spawn. The
//! relay daemon repeats the scrub in its own `main`
//! (`crates/relay/src/main.rs`) because it outlives the app that started it
//! and is the direct parent of every terminal PTY.

/// Variables Claude Code sets on child processes to mark them as part of a
/// running session. Identity-of-launch only — never user configuration, so
/// dropping them cannot lose a setting the user chose.
const SESSION_MARKERS: [&str; 12] = [
    // Makes a spawned `claude` treat itself as a nested child session and
    // disable transcript saving.
    "CLAUDE_CODE_CHILD_SESSION",
    // The generic "running inside Claude Code" flags; the CLI and other
    // tools change behavior when they detect nesting through them, and some
    // versions refuse to start at all ("cannot be launched inside another
    // session").
    "CLAUDECODE",
    "CLAUDE_CODE",
    // The outer session's identity: which session, its parent, its bridge,
    // how it was started, and which binary runs it — all meaningless once
    // inherited across an app boundary.
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_PARENT_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_HOST_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
    // An inherited "already sandboxed" claim makes `claude` skip its folder
    // trust prompt — a security gate, not just bookkeeping.
    "CLAUDE_CODE_SANDBOXED",
];

/// Remove inherited Claude Code session markers from this process.
///
/// Call before any thread exists — see the `unsafe` note below.
pub fn scrub_inherited_claude_session_markers() {
    for name in SESSION_MARKERS {
        if std::env::var_os(name).is_none() {
            continue;
        }
        // SAFETY: called from the top of `main`, before the tokio runtime and
        // the GPUI executor exist, so no other thread can be reading the
        // environment. `remove_var` is only unsound when it races a
        // concurrent read.
        unsafe { std::env::remove_var(name) };
        tracing::info!(marker = name, "dropped an inherited Claude Code session marker");
    }
}
