//! oximux-git
//!
//! Git CLI wrappers (no gitoxide in v1). Hosts the `StatusPoller`, diff parser,
//! stash / merge / worktree commands, and the producer/consumer event types
//! consumed by the UI.
//!
//! Phase 0 = skeleton; status + diff land in Phase 2.

use anyhow::Result;
use std::path::Path;

/// Working tree status snapshot. Phase 2 fills this from `git status --porcelain=v2`.
#[derive(Debug, Default, Clone)]
pub struct StatusSnapshot {
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
}

pub fn status(_repo: &Path) -> Result<StatusSnapshot> {
    anyhow::bail!("oximux-git::status is not implemented until Phase 2");
}
