//! The desktop's worktree service — now just a re-export.
//!
//! The implementation moved to `oximux-worktree-ops` so `oximux serve` hosts
//! the same one: a headless host that could not create worktrees answered
//! `Unsupported` to `oximux worktree create`, `run --worktree`, and
//! `team run --worktree-each`, which is most of the reason to run one.
//!
//! Kept as a module rather than deleted because this path is what
//! `remote_control` wires, and the indirection costs nothing.

pub use oximux_worktree_ops::RepoWorktrees;
