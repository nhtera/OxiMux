//! Agent glue — the host-side wiring that keeps live agents coherent.
//!
//! What is still *here* is what needs the app: `agent_awake` (sleep-assertion
//! and App-Nap suppression while agents run) and `screen_control_watch`.
//!
//! The status-hook half moved to the `oximux-agent-hooks` crate, because the
//! CLI has to install and inspect the same hooks the app does and a verb that
//! only worked while the GUI was up would be useless exactly when it is
//! reached for. It is re-exported below under its historical names, so every
//! `crate::agent_hook_dialects::…` / `oximux_app::agent_status_hooks::…` call
//! site resolves unchanged.

pub mod agent_awake;

#[cfg(any(target_os = "macos", windows))]
pub mod screen_control_watch;

#[doc(inline)]
pub use oximux_agent_hooks::{
    agent_hook_dialects, agent_hooks_global, agent_status_hooks, pi_status_extension,
};

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
