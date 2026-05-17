//! oximux-git
//!
//! Git CLI wrappers (no gitoxide in v1). Phase 2 step 1+2 lands:
//! - `process::GitCmd` — tokio-based `git` runner with timeout + kill_on_drop.
//! - `repository::Repository` — validated handle to one working tree.
//! - `status::parse_porcelain_v2` — pure parser for `--porcelain=v2 --branch -z`.
//!
//! Domain shapes (`GitState`, `FileStatus`, …) live in `oximux-core`.

pub mod diff;
pub mod error;
pub mod poller;
pub mod process;
pub mod repository;
pub mod stage;
pub mod status;

pub use diff::{DiffParseError, parse_unified_diff};
pub use error::{GitError, Result};
pub use poller::{DEFAULT_TICK, PollState, StatusPoller};
pub use process::{GitCmd, Output, RawOutput};
pub use repository::Repository;
pub use status::parse_porcelain_v2;
