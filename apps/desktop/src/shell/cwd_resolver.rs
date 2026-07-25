//! Resolve a process's current working directory from its pid.
//!
//! The implementation lives in the shared `oximux-proc-cwd` crate so the
//! relay daemon can use the same syscall for its checkpoint live-cwd
//! refresh; this module keeps the app-internal path stable for callers.

pub use oximux_proc_cwd::cwd_of_pid;
