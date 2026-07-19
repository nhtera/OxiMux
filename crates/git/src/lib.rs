//! oximux-git
//!
//! Git CLI wrappers (no gitoxide in v1). Phase 2 step 1+2 lands:
//! - `process::GitCmd` — tokio-based `git` runner with timeout + kill_on_drop.
//! - `repository::Repository` — validated handle to one working tree.
//! - `status::parse_porcelain_v2` — pure parser for `--porcelain=v2 --branch -z`.
//!
//! Domain shapes (`GitState`, `FileStatus`, …) live in `oximux-core`.

pub mod branch;
pub mod branch_diff;
pub mod checkpoint;
pub mod clone;
pub mod commit;
pub mod diff;
pub mod error;
pub(crate) mod forge_cli;
pub mod gh;
pub mod glab;
pub mod log;
pub mod merge;
pub mod numstat;
pub mod operation;
pub mod path_guard;
pub mod poller;
pub mod pr_context;
pub mod process;
pub mod remote;
pub mod repository;
pub mod stage;
pub mod staged_context;
pub mod stash;
pub mod status;
pub mod worktree;

pub use checkpoint::{CheckpointEngine, CheckpointError, CheckpointSha};
pub use clone::{clone_repo, repo_name_from_url};
pub use diff::{DiffParseError, parse_unified_diff};
pub use error::{GitError, Result};
pub use gh::GhCmd;
pub use numstat::{diff_numstat_commit, diff_numstat_head, parse_numstat_z};
pub use poller::{DEFAULT_TICK, PollState, StatusPoller};
pub use process::{GitCmd, Output, RawOutput};
pub use remote::LeaseStatus;
pub use repository::Repository;
pub use status::parse_porcelain_v2;
pub use worktree::{derive_slug, validate_slug};
