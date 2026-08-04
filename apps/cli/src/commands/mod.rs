//! One module per verb family. Each returns `(json data, human text)` so the
//! output layer owns the envelope and the verbs own only their content.

pub mod agent_context;
pub mod attach;
pub mod git;
pub mod heartbeat;
pub mod model;
pub mod pair;
pub mod permit;
pub mod run;
pub mod schedule;
pub mod send;
pub mod session_ctl;
pub mod sessions;
pub mod state;
pub mod status;
pub mod team;
pub mod term;
pub mod transcript;
pub mod wait;
pub mod worktree;
