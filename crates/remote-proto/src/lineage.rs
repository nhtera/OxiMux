//! Wire types for agent delegation (v36): an agent spawning child agents,
//! listing and cancelling them, and asking the host who it is.
//!
//! Append-only like every other wire type here. Types carried inside a `Vec`
//! (`LineageWire`, `AgentRosterWire`) never gain fields: postcard has no field
//! tags, so an older peer would misread every element after the first.

use serde::{Deserialize, Serialize};

/// Which session a spawned child hangs under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnParentWire {
    /// The caller's own session (an agent spawning its own child).
    Caller,
    /// A named session. Full scope only: an agent cannot graft children onto
    /// a tree it does not own.
    Session(String),
}

/// Where a child works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnCwdWire {
    /// The parent's working directory.
    Inherit,
    /// A new worktree of the parent's repository, optionally named.
    NewWorktree { name: Option<String> },
    /// A path inside the parent's repository.
    Path(String),
}

/// Which of a child's events reach its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotifyParentWire {
    /// Every event.
    #[default]
    Auto,
    /// Only failed, interrupted, needs permission, needs input.
    Stuck,
    /// None.
    Off,
}

/// What a child is allowed to do, fixed at spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SpawnPostureWire {
    /// What the user configured for that agent.
    #[default]
    Default,
    /// Reads only; never writes, runs commands or prompts.
    Review,
    /// No source edits; may build and run tests; never prompts.
    Verify,
}

/// Start a child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAgentReq {
    /// The caller's idempotency key: the same key under the same parent
    /// returns the same child instead of starting another.
    pub request_id: String,
    pub parent: SpawnParentWire,
    /// Which agent (an id from [`WhoamiWire::agents`]).
    pub agent_id: String,
    pub model: Option<String>,
    /// The child's first message.
    pub prompt: String,
    pub cwd: SpawnCwdWire,
    /// A JSON schema the child's final answer should follow, when the caller
    /// wants structured output back.
    pub output_schema: Option<String>,
    pub notify_parent: NotifyParentWire,
    /// Let the parent answer this child's (non-Bash, non-write) permission
    /// prompts with Deny or an unchanged Allow.
    pub allow_parent_permits: bool,
    pub posture: SpawnPostureWire,
}

/// The child a [`SpawnAgentReq`] produced (or, on a replay, already had).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnAgentResult {
    pub session_id: String,
    pub parent_id: String,
    pub root_id: String,
    pub depth: u32,
    /// `true` when this was a replay of an earlier request: nothing new started.
    pub replayed: bool,
    /// Whether the child was given its prompt. `false` means it opened but is
    /// sitting idle: send it the task, or cancel it.
    pub prompt_delivered: bool,
}

/// Where a child stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineageStateWire {
    Reserved,
    /// Working (running, blocked on a prompt, rate-limited, queued).
    Active,
    /// Idle with nothing pending, or failed and not yet cancelled.
    Parked,
    /// Cancelled or ended with its run. Terminal.
    Ended,
}

/// One row of the spawn tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageWire {
    pub session_id: String,
    pub parent_id: String,
    pub root_id: String,
    pub depth: u32,
    pub posture: SpawnPostureWire,
    pub state: LineageStateWire,
    pub notify_parent: NotifyParentWire,
    /// The child's identity colour slot in its parent's UI, once assigned.
    pub identity_slot: Option<u32>,
    /// RFC 3339.
    pub created_at: String,
}

/// One agent a caller may spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRosterWire {
    pub agent_id: String,
    pub display: String,
    /// The hint to use when `models` is empty (a cold catalog).
    pub default_model: Option<String>,
    pub models: Vec<String>,
    /// The postures this host can hold that agent to.
    pub postures: Vec<SpawnPostureWire>,
    pub installed: bool,
}

/// Who the caller is, to the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhoamiWire {
    /// `full` (the operator), `session` (an agent's confined credential),
    /// `mcp` (an outside MCP client) or `device` (a paired phone).
    pub scope: String,
    /// The caller's own session, for a session-scoped caller.
    pub session: Option<String>,
    pub depth: u32,
    pub root_id: Option<String>,
    /// Whether the caller may spawn at all right now (delegation on, a chat
    /// agent, shallower than the depth cap, a `Default` posture).
    pub spawn_allowed: bool,
    /// Why not, when it may not.
    pub reason: Option<String>,
    pub agents: Vec<AgentRosterWire>,
}

/// What kind of spawn refusal [`SpawnErrorWire`] carries, for a caller that
/// branches on it; the message is the human (and agent) readable part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnErrorKind {
    DepthExceeded,
    FanoutExceeded,
    OpenExceeded,
    BudgetExceeded,
    StartFailing,
    OutsideRepo,
    IdempotencyConflict,
    InProgress,
    NotInSubtree,
    PostureUnenforceable,
    UnknownAgent,
    /// Delegation is off, the caller is not a chat agent, or this host does
    /// not own the data directory.
    NotAllowed,
}

/// A refused delegation call. The message always ends with what to do instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnErrorWire {
    pub kind: SpawnErrorKind,
    pub message: String,
}
