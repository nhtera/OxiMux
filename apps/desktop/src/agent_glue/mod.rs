//! Agent glue — the host-side wiring that keeps live agents coherent.
//!
//! `agent_awake` (sleep-assertion / App-Nap suppression while agents run),
//! `agent_hooks_global` (global hook registry), and `agent_status_hooks`
//! (status-line / OSC hook plumbing). Grouped for traversal; re-exported at
//! the crate root so existing `crate::agent_awake::…` paths keep resolving.

pub mod agent_awake;
pub mod agent_hooks_global;
pub mod agent_status_hooks;
#[cfg(target_os = "macos")]
pub mod screen_control_watch;
