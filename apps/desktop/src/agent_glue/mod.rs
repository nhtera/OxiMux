//! Agent glue — the host-side wiring that keeps live agents coherent.
//!
//! `agent_awake` (sleep-assertion / App-Nap suppression while agents run),
//! `agent_hooks_global` (global hook registry), and `agent_status_hooks`
//! (status-line / OSC hook plumbing). Grouped for traversal; re-exported at
//! the crate root so existing `crate::agent_awake::…` paths keep resolving.

pub mod agent_awake;
pub mod agent_hook_dialects;
pub mod agent_hooks_global;
pub mod agent_status_hooks;
pub mod pi_status_extension;
#[cfg(any(target_os = "macos", windows))]
pub mod screen_control_watch;

/// Start the watch that drives the "an agent is driving" indicator, on the
/// platforms that have one.
///
/// Exists so no caller carries a `cfg` of its own. `main.rs` used to spell the
/// platform predicate a second time at the call site, and when screen control
/// was turned on for Windows only the module's copy was updated — so the tray
/// icon was compiled, correct, and never started. Nothing failed: a missing call
/// produces no warning, and `install` stayed reachable from macOS so it was not
/// even dead code.
///
/// The two arms below are adjacent and exhaustive, so adding a platform is one
/// edit in one place rather than a predicate to keep in step across files.
#[cfg(any(target_os = "macos", windows))]
pub fn install_screen_control_watch(cx: &mut gpui::App) {
    screen_control_watch::install(cx);
}

/// No screen control here, so nothing to indicate.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn install_screen_control_watch(_cx: &mut gpui::App) {}
