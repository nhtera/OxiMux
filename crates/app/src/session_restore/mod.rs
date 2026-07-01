//! Session restore — bringing terminals/agents back after a restart.
//!
//! `relay_cold_restore` + `relay_supervisor` drive the daemon-backed cold
//! restore path, `restore_fallback` the degraded path, `persisted_terminals`
//! the on-disk checkpoint, and `git_state_cache` the seeded SCM snapshot.
//! Several of these are `impl WorkspaceRoot` blocks and so must stay in the
//! `app` crate — foldering keeps them in `app`; only a cross-crate move would
//! hit the orphan rule. Re-exported at the crate root to preserve paths.

pub mod git_state_cache;
pub mod persisted_chat;
pub mod persisted_terminals;
pub mod relay_cold_restore;
pub mod relay_supervisor;
pub(crate) mod restore_fallback;
