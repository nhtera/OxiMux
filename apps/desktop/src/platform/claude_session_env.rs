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

/// Remove inherited Claude Code session markers from this process.
///
/// The marker list and the scrub itself live in `oximux-shell-env` — one
/// list, three consumers (this app, the relay daemon, `oximux serve`) — so a
/// marker Claude Code adds later is fixed in one place. This wrapper keeps
/// the app-side logging.
///
/// Call before any thread exists — env mutation is only sound while the
/// process is single-threaded (the shared fn documents the same contract).
pub fn scrub_inherited_claude_session_markers() {
    for marker in oximux_shell_env::scrub_inherited_claude_session_markers() {
        tracing::info!(marker, "dropped an inherited Claude Code session marker");
    }
}
