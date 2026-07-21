//! The rewind seam: dropping a session back to an earlier turn, expressed
//! without depending on the desktop that does it.
//!
//! Like [`SessionLauncher`](crate::SessionLauncher), this exists because a
//! rewind is not something `SessionRegistry` can perform. It is a strictly
//! sequenced flow whose middle steps are filesystem and process work —
//! confirm the child is dead, fork the backing session file, optionally restore
//! a git checkpoint, then respawn — and `agent-core` was deliberately kept free
//! of exactly that (the reason it was split out of `crates/agents` at all).
//!
//! So the dispatcher talks to this trait and the app supplies the
//! implementation. Fourth user of a pattern this crate already leans on for
//! PTYs, device persistence, and session launch.
//!
//! **Why the desktop's own flow is not reproduced here.** It would be a second
//! implementation of a sequence whose failure modes are subtle — a fork that
//! lands while the child is still flushing produces a truncated transcript that
//! looks fine — and two implementations would drift. Routing to the one the
//! desktop already runs means a remote rewind and a local one are the same
//! operation, with the same guarantees, verified by the same code.

/// Why a rewind could not happen.
///
/// Coarse for the same reason [`LaunchError`](crate::LaunchError) is: the
/// underlying failures embed host paths (a session file under the user's home,
/// a git object id) and the git handlers already learned not to forward that.
#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    /// No such live session, or the desktop cannot reach it right now.
    #[error("that session is not available to rewind")]
    Unavailable,
    /// The session's backend does not support rewinding at all.
    #[error("this agent does not support rewinding")]
    Unsupported,
    /// The ordinal does not name a user message in the host's transcript.
    ///
    /// Distinct from a generic failure because it is the *client's* view being
    /// stale rather than anything wrong on the host — the phone folded a
    /// transcript that has since moved on. A client seeing this should resync
    /// rather than retry.
    #[error("that message is no longer in the transcript")]
    OrdinalMismatch,
    /// The files axis was requested but this host will not perform it.
    ///
    /// Its own variant rather than a generic refusal so a client can tell "not
    /// offered here" from "the rewind failed", and fall back to a
    /// conversation-only rewind instead of surfacing an error.
    #[error("restoring files is not available from a remote client")]
    FilesUnsupported,
    /// A rewind is already running on this session.
    ///
    /// Refused rather than queued: two rewinds racing would have the second
    /// fork a session file the first is still replacing.
    #[error("a rewind is already in progress")]
    Busy,
    /// The desktop tried and failed. Detail is logged host-side, never returned.
    #[error("the rewind did not complete")]
    Failed,
}

/// Rewinding a session to an earlier turn.
///
/// **Destructive**: the turns after `ordinal` are dropped from the transcript,
/// and with `include_files` the working tree is overwritten too. Gated on
/// `may_write`, like every other state-changing RPC.
#[async_trait::async_trait]
pub trait RewindService: Send + Sync {
    /// Rewind `session_id` to `ordinal`, optionally restoring files.
    ///
    /// Returns once the rewind has been **accepted and validated** — the
    /// session exists, its backend supports rewinding, and the ordinal resolves
    /// — not once it has finished. The truncation reaches clients as a
    /// `ThreadEvent::Rewound` on the session's event stream, which is also how a
    /// desktop-initiated rewind reaches them; a caller that waited for
    /// completion here would still have to handle that event, so waiting would
    /// buy nothing but a longer-held request slot.
    ///
    /// Implementations must validate `ordinal` against their own transcript.
    /// The dispatcher checks authorization, not whether the client's view of
    /// the conversation is current.
    async fn rewind(
        &self,
        session_id: &str,
        ordinal: usize,
        include_files: bool,
    ) -> Result<(), RewindError>;
}
