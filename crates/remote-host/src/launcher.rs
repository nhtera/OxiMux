//! The session-launch seam: starting a new agent session, expressed without
//! depending on the desktop that does it.
//!
//! Every other session RPC reaches an *existing* `SessionHandle` and drives the
//! `AgentConnection` it already holds. Creating one is different in kind: it
//! needs a working directory, a resolved backend, and a new child process — all
//! of which live in the GPUI view layer, and none of which `SessionRegistry` can
//! reach. The registry is deliberately process-spawn-free (that is why
//! `agent-core` was split out of `crates/agents` at all), so pulling launch logic
//! into it would undo the split.
//!
//! So the dispatcher talks to this trait and the app supplies the
//! implementation, exactly as [`TerminalSource`](crate::TerminalSource) does for
//! PTYs and [`DeviceStore`](crate::auth::DeviceStore) does for device
//! persistence. Nothing new architecturally — the third user of a pattern this
//! crate already leans on twice.

/// Why a launch could not happen.
///
/// Coarse on purpose. A launch failure routinely embeds absolute host paths (a
/// missing directory, a binary that is not on `PATH`), and the git handlers
/// already learned not to forward that text to a client.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The desktop cannot start a session right now — no window open, no
    /// workspace to put a tab in.
    #[error("the desktop cannot start a session right now")]
    Unavailable,
    /// The request named a working directory that does not exist or is not a
    /// directory.
    #[error("that working directory is not usable")]
    BadWorkingDirectory,
    /// The desktop tried and failed. Detail is logged host-side, never returned.
    #[error("the session could not be started")]
    Failed,
    /// A model was named for an agent that cannot be given one at spawn.
    ///
    /// Its own variant rather than [`Self::Failed`] because the caller can act
    /// on it — drop the model, or pick an agent that takes one — and because
    /// the alternative is worse than any error: an ACP agent silently ignores a
    /// model at spawn, so the session would run its default while the team
    /// board recorded the model that was asked for. A record nothing ever
    /// contradicts is the one outcome worth refusing a launch over.
    #[error("this agent cannot be given a model when it starts")]
    ModelUnsupported,
}

/// Starting a new agent session on the desktop.
///
/// **This is remote-triggered process spawning — the highest-privilege call on
/// this protocol**, and the only one that creates rather than drives. It is gated
/// on `may_write`, so a read-only device is refused.
///
/// The working directory is whatever the phone sends, per an explicit product
/// decision. That is defensible rather than reckless for one specific reason: a
/// paired device already has full shell access through the terminal RPCs
/// (pairing grants read-write by default), so the same device could `cd`
/// anywhere and launch an agent from a terminal already. This adds convenience,
/// not a new capability class. It does **not** follow that the path can go
/// unchecked — see [`Self::create`].
#[async_trait::async_trait]
pub trait SessionLauncher: Send + Sync {
    /// Start a session in `cwd`, optionally with a named agent.
    ///
    /// Returns the new session's id, so the caller can subscribe to it directly
    /// rather than polling `ListSessions` and guessing which row is new. The id
    /// is the same one the registry will register it under.
    ///
    /// `agent_id` selects which configured agent to launch; `None` means the
    /// desktop's default. An unknown id is a [`LaunchError::Failed`], not a
    /// silent fallback to the default — starting a *different* agent than the
    /// one asked for is a worse outcome than refusing.
    ///
    /// `model` fixes the session's model **at spawn**, and is a parameter here
    /// rather than a switch afterwards because that is the only moment the
    /// common backends accept one: Claude and Codex take `--model` on the
    /// command line and refuse to change it at runtime, so a post-launch switch
    /// reaches the trait-default `set_model`, which bails. The desktop papers
    /// over that by respawning through its view, but a headless host has no view
    /// — so a model applied after the fact works on ACP backends and silently
    /// fails on the two most common ones. Applying it at spawn works everywhere
    /// and costs a parameter. `None` = the agent's own default.
    ///
    /// Implementations must validate `cwd` themselves: the dispatcher checks
    /// authorization, not filesystem reality.
    async fn create(
        &self,
        cwd: &str,
        agent_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<String, LaunchError>;
}
