//! One truthful state for a chat session, computed from facts.
//!
//! A chat agent can be idle, working, waiting on a person, rate-limited,
//! failed, stopped, or simply unknown to us. Each of those used to be read off
//! a different flag in a different place, and the gaps showed: a session whose
//! process died read idle, a session working through background-task
//! continuations read idle, a restored chat read as if it were running.
//!
//! [`compute`] is pure: whoever drives the session gathers [`HealthInputs`]
//! (the fold, the send gate, the retry, the process) and publishes them; the
//! host derives the same [`SessionHealth`] from them every time. Two rules
//! carry the weight:
//!
//! - **Truth needs a proof.** A session is `Interrupted` or `Failed` only on an
//!   observed fact (a user Stop, an exit, the host lock proving the previous
//!   host is gone). With no process to ask, it is `Unverifiable`, never a guess.
//! - **Silence is not death.** No output for a long time marks a working
//!   session `stalled_since` (informational); it never becomes `Failed`.

/// What a session is doing, as a person needs to know it. Ordered by what
/// wins when several hold: a dead process outranks a pending prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthStatus {
    /// Nothing running and nothing waiting.
    Idle,
    /// A turn or a continuation is running, or a send has not been answered.
    Working,
    /// A permission prompt waits for an answer.
    NeedsPermission,
    /// A question (`AskUserQuestion`) waits for an answer.
    NeedsAnswer,
    /// A provider limit closed the last turn; a retry is armed.
    Limited,
    /// The process died without being asked to, or the last turn failed.
    Failed,
    /// Stopped on purpose (a user Stop), or provably cut off by a restart.
    Interrupted,
}

/// Why a process is gone, with the fact that proves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitProof {
    /// It exited and reported this code.
    ExitCode(i32),
    /// The user stopped it.
    UserStop,
    /// The host restarted: winning the host lock over a previous record means
    /// every process the previous host ran is gone.
    HostLock,
    /// Its output closed; no exit code could be read.
    ConnectionClosed,
}

/// How sure we are that the session's process is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Liveness {
    /// A process or connection we hold is delivering frames.
    Live,
    /// We hold nothing that can tell (a dormant chat with no process).
    Unverifiable,
    /// Rebuilt from disk after a restart; nothing has confirmed it yet.
    RestoredUnconfirmed,
    /// Gone, with the proof.
    Exited(ExitProof),
}

impl Liveness {
    /// Wire name, as the health frame carries it.
    pub fn wire_name(self) -> &'static str {
        match self {
            Liveness::Live => "live",
            Liveness::Unverifiable => "unverifiable",
            Liveness::RestoredUnconfirmed => "restoredUnconfirmed",
            Liveness::Exited(_) => "exited",
        }
    }
}

/// The session's process, as its driver knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Process {
    /// No process: a dormant chat (restored, or not yet sent to). Not death.
    #[default]
    Dormant,
    /// Running, and it has produced a frame on this connection.
    Live,
    /// Gone.
    Exited(ExitProof),
}

/// What a restart left behind for a restored session, until its first frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Restored {
    /// The session was mid-turn (or had background tasks) when the previous
    /// host stopped.
    pub was_mid_turn: bool,
    /// This host won the host lock over a previous record: that host's
    /// processes are provably gone.
    pub prior_host_gone: bool,
}

/// A prompt that waits for a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Permission,
    Question,
}

/// The facts [`compute`] reads. Times are Unix milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealthInputs {
    /// A host-started turn is running.
    pub turn_active: bool,
    /// Continuation turns the provider runs on its own after a turn ended.
    pub continuations_open: u32,
    /// A send was written (or accepted) and no provider frame has followed.
    pub pending_unanswered_send: bool,
    /// When the provider started on the last send without a turn following
    /// (accepted, not started): set while that lasts.
    pub accepted_unstarted_since: Option<i64>,
    /// Background tasks (subagents, background shells) still running.
    pub background_active: u32,
    /// Children of this session that are working.
    pub children_active: u32,
    /// A prompt waits for a person.
    pub prompt: Option<PromptKind>,
    /// The request that prompt is for, so a host that decided it (a phone's or
    /// the CLI's answer) knows it is answered before the driver hears.
    pub prompt_request: Option<String>,
    /// When an armed retry fires because a provider limit closed the turn.
    pub limited_until: Option<i64>,
    /// Why that retry is armed ("Usage limit reached", "Provider overloaded").
    pub limit_reason: Option<String>,
    /// The last turn failed (its message), and nothing has run since.
    pub last_turn_failure: Option<String>,
    /// The user stopped the session.
    pub interrupted: bool,
    pub process: Process,
    /// Set from a restart until the first frame on the new connection.
    pub restored: Option<Restored>,
    /// Messages restored from before a restart, held for the user.
    pub held_restored: u32,
    /// Messages queued behind the running turn.
    pub queued: u32,
    /// The last sign of life: provider or tool output, background progress, a
    /// working child, or a change to the session's own prompt.
    pub last_activity_at: i64,
    /// The current or last turn (`{incarnation}:{n}`), for deduping edges.
    pub turn_id: Option<String>,
    /// The last process exit (`{incarnation}:{n}`), for deduping edges.
    pub exit_seq: Option<String>,
}

/// Thresholds. `stall_after_ms == 0` turns the stall signal off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    pub stall_after_ms: i64,
    /// How long an accepted send may go without a turn before the session
    /// says so.
    pub accepted_unstarted_ms: i64,
}

/// Default stall threshold. Informational until a drill measures the real
/// silent gaps (long builds, extended thinking, a subagent wait).
pub const DEFAULT_STALL_AFTER_MS: i64 = 10 * 60 * 1000;

impl Default for HealthConfig {
    fn default() -> Self {
        Self { stall_after_ms: DEFAULT_STALL_AFTER_MS, accepted_unstarted_ms: 120_000 }
    }
}

/// The derived state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHealth {
    pub status: HealthStatus,
    pub liveness: Liveness,
    /// While `Limited`: when the retry fires.
    pub limited_until: Option<i64>,
    /// While working: when the session went quiet past the threshold.
    pub stalled_since: Option<i64>,
    /// The process died without a Stop.
    pub exited_unexpectedly: bool,
    /// One line for a tooltip or a list row.
    pub reason: Option<String>,
    pub last_activity_at: i64,
    pub pending_unanswered_send: bool,
    pub held_restored: u32,
    pub turn_id: Option<String>,
    pub exit_seq: Option<String>,
}

/// Whether the session counts as doing something: a turn, a continuation, or
/// a send nobody has answered.
pub fn is_working(i: &HealthInputs) -> bool {
    i.turn_active || i.continuations_open > 0 || i.pending_unanswered_send
}

/// Derive the session's state. Pure: same inputs, same answer.
pub fn compute(i: &HealthInputs, now: i64, cfg: &HealthConfig) -> SessionHealth {
    let (status, liveness, reason, exited_unexpectedly) = classify(i, now, cfg);
    let stalled_since = (status == HealthStatus::Working && cfg.stall_after_ms > 0)
        .then_some(i.last_activity_at)
        .filter(|&since| now.saturating_sub(since) >= cfg.stall_after_ms);
    SessionHealth {
        status,
        liveness,
        limited_until: (status == HealthStatus::Limited).then_some(i.limited_until).flatten(),
        stalled_since,
        exited_unexpectedly,
        reason,
        last_activity_at: i.last_activity_at,
        pending_unanswered_send: i.pending_unanswered_send,
        held_restored: i.held_restored,
        turn_id: i.turn_id.clone(),
        exit_seq: i.exit_seq.clone(),
    }
}

fn classify(i: &HealthInputs, now: i64, cfg: &HealthConfig) -> (HealthStatus, Liveness, Option<String>, bool) {
    use HealthStatus as S;
    match i.process {
        Process::Exited(proof) => {
            if i.interrupted || proof == ExitProof::UserStop {
                return (S::Interrupted, Liveness::Exited(ExitProof::UserStop), None, false);
            }
            if proof == ExitProof::HostLock {
                return (S::Interrupted, Liveness::Exited(proof), Some("OxiMux restarted".into()), false);
            }
            let reason = match proof {
                ExitProof::ExitCode(code) => format!("agent process exited (code {code})"),
                _ => "agent process exited".into(),
            };
            return (S::Failed, Liveness::Exited(proof), Some(reason), true);
        }
        Process::Dormant => {
            // No process to ask. A restart that provably cut a running turn
            // short is an interruption; anything else is unknown, not idle.
            if let Some(r) = i.restored {
                if r.was_mid_turn && r.prior_host_gone {
                    return (
                        S::Interrupted,
                        Liveness::Exited(ExitProof::HostLock),
                        Some("OxiMux restarted during a turn".into()),
                        false,
                    );
                }
                return (S::Idle, Liveness::RestoredUnconfirmed, Some("restored, not confirmed".into()), false);
            }
            if i.interrupted {
                return (S::Interrupted, Liveness::Unverifiable, None, false);
            }
            if let Some(failure) = &i.last_turn_failure {
                return (S::Failed, Liveness::Unverifiable, Some(failure.clone()), false);
            }
            return (S::Idle, Liveness::Unverifiable, None, false);
        }
        Process::Live => {}
    }
    // A restored chat whose new connection has not produced a frame yet: the
    // process is ours, but nothing has confirmed the session it resumed.
    let live = if i.restored.is_some() { Liveness::RestoredUnconfirmed } else { Liveness::Live };
    if i.interrupted {
        return (S::Interrupted, live, None, false);
    }
    match i.prompt {
        Some(PromptKind::Permission) => return (S::NeedsPermission, live, None, false),
        Some(PromptKind::Question) => return (S::NeedsAnswer, live, None, false),
        None => {}
    }
    if i.limited_until.is_some() {
        return (S::Limited, live, Some(i.limit_reason.clone().unwrap_or_else(|| "rate limited".into())), false);
    }
    if is_working(i) {
        let reason = i
            .accepted_unstarted_since
            .filter(|&since| now.saturating_sub(since) >= cfg.accepted_unstarted_ms)
            .map(|_| "Accepted, not started yet".to_string());
        return (S::Working, live, reason, false);
    }
    if let Some(failure) = &i.last_turn_failure {
        return (S::Failed, live, Some(failure.clone()), false);
    }
    (S::Idle, live, None, false)
}

/// A clock that sleeps (a laptop lid, App Nap) is not a silent agent. Given
/// how much time passed since the last tick on a monotonic clock and on the
/// wall clock, and the interval the caller ticks at, says whether the machine
/// slept in between: the wall clock ran ahead of the monotonic one (macOS and
/// Linux stop the monotonic clock in suspend) or the tick came far too late.
/// The caller then forgives the silence so far, so waking never reads as a
/// burst of stalls.
pub fn slept(mono_elapsed_ms: i64, wall_elapsed_ms: i64, interval_ms: i64) -> bool {
    wall_elapsed_ms.saturating_sub(mono_elapsed_ms) > interval_ms
        || mono_elapsed_ms > interval_ms.saturating_mul(2)
}

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
