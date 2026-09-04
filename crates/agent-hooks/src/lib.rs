//! Agent status hooks: the dialect table, the installer, and the readers.
//!
//! Four modules that only make sense together, so they moved together:
//!
//! * [`agent_hook_dialects`] — the table. One row per agent CLI, saying where
//!   its hooks file lives, how one entry is spelled in it, what its lifecycle
//!   events are called, and which key on the payload carries the agent's reply.
//! * [`agent_hooks_global`] — the installer that merges OxiMux's entries into
//!   that file and prunes them back out, without disturbing anything the user
//!   put there.
//! * [`agent_status_hooks`] — the other end: what the hook process does with
//!   the event JSON an agent hands it on stdin, and the per-spawn `--settings`
//!   injection that is the global install's byte-identical twin.
//! * [`pi_status_extension`] — the one dialect whose "hook" is a source file
//!   the agent loads and runs itself, rather than a command it shells out to.
//!
//! The module names are unchanged from where they lived in the desktop app, so
//! every `crate::agent_hook_dialects::…` path inside them still resolves and
//! the extraction moved no code at all. The desktop re-exports all four under
//! their historical `oximux_app::…` paths for the same reason.
//!
//! **No gpui, no platform cfgs.** The whole crate is `std`, paths, and JSON —
//! which is what made it extractable, and what lets `oximux agent hooks` work
//! on a headless host with no app running.

pub mod agent_hook_dialects;
pub mod agent_hooks_global;
pub mod agent_status_hooks;
pub mod pi_status_extension;
/// What is installed right now, read back off disk.
pub mod inspect;
/// What the hook process decides, apart from how it delivers it.
pub mod report;
