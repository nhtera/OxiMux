//! What a host tracks about a session beyond its fold, and the one mapping
//! from (fold + tracker) to [`HealthInputs`].
//!
//! The fold ([`ChatThread`]) knows what the agent said. It does not know when
//! the agent last showed a sign of life, whether a send is still unanswered,
//! how the process ended, or what a restart left behind — those are facts
//! about the *connection*, kept here.
//!
//! **One mapping, because two would disagree.** The desktop and the headless
//! host fold the same events into the same `ChatThread`, so there is exactly
//! one correct way to read a session's health out of it. Written twice, the
//! two would drift the way every other pair in this project has: not all at
//! once, but one field at a time, each change plausible on its own. What a
//! host alone knows — whether its process is live, whether a retry is armed,
//! whether the user stopped it — it passes in as [`HealthExtras`].

use crate::health::{ExitProof, HealthInputs, Process, PromptKind, Restored};
use crate::thread::{ChatThread, TaskStatus, ThreadEvent};

/// The connection facts a fold does not keep.
///
/// Fields are public because this is a record, not an invariant: the methods
/// are the ones that have to be used together, and the mapping below reads
/// every field.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HealthTrack {
    /// The last sign of life (any provider frame, or a send).
    pub last_activity_at: i64,
    /// A send went out and no frame has answered it yet.
    pub sent_unanswered: bool,
    /// The id that send went out under.
    pub sent_id: Option<String>,
    /// The agent took the last send (receipt) and has produced nothing since.
    pub accepted_at: Option<i64>,
    /// How the process ended, once it has.
    pub exit: Option<ExitProof>,
    /// Sends and process exits so far, for the edge ids.
    pub turns: u32,
    pub exits: u32,
    /// Set on restore until the first frame on the new connection.
    pub restored: Option<Restored>,
    /// The last turn ended in error (its message); cleared by the next send
    /// or a turn that ends well. Only a turn's own result, not a failed fork
    /// or rewind, makes a session read failed.
    pub failed_turn: Option<String>,
    /// The driver's queue: restored messages held, and messages waiting to
    /// drain. Owned by whatever drives the session (a composer, a pump).
    pub held: u32,
    pub queued: u32,
    /// A permission or question was open at the last publish.
    pub prompt_open: bool,
}

impl HealthTrack {
    /// A restored session: unconfirmed until its first frame. `was_mid_turn`:
    /// its saved state had a turn or background work running.
    pub fn restored(was_mid_turn: bool, prior_host_gone: bool) -> Self {
        Self { restored: Some(Restored { was_mid_turn, prior_host_gone }), ..Self::default() }
    }

    /// A frame from the agent. Receipts are bookkeeping, not a sign of life,
    /// except that acceptance starts the "accepted, not started" clock.
    pub fn note_event(&mut self, ev: &ThreadEvent, now: i64) {
        if ev.is_receipt() {
            if matches!(ev, ThreadEvent::UserMessageAccepted { .. }) && self.sent_unanswered {
                self.accepted_at.get_or_insert(now);
            }
            return;
        }
        self.last_activity_at = now;
        self.sent_unanswered = false;
        self.accepted_at = None;
        self.restored = None;
        if let ThreadEvent::TurnEnded { is_error, result, .. } = ev {
            self.failed_turn = is_error.then(|| result.clone().unwrap_or_else(|| "Turn failed".into()));
        }
    }

    /// The last turn failed and nothing has been sent since: the queue waits
    /// for the user rather than draining into a failing session.
    pub fn holds_queue(&self) -> bool {
        self.failed_turn.is_some()
    }

    /// The conversation was cleared: its failed turn no longer holds anything.
    pub fn forget_failed_turn(&mut self) {
        self.failed_turn = None;
    }

    /// The id of the last send, when it was tagged.
    pub fn sent_id(&self) -> Option<String> {
        self.sent_id.clone()
    }

    /// A send was written to the agent, under `id` when it was tagged.
    pub fn note_sent(&mut self, now: i64, id: Option<String>) {
        self.sent_id = id;
        self.turns += 1;
        self.sent_unanswered = true;
        self.accepted_at = None;
        self.last_activity_at = now;
        self.exit = None;
        self.failed_turn = None;
    }

    /// Waiting on a person is not silence: an answer, while the process
    /// lives, is the last sign of life (the stall clock restarts there).
    pub fn note_prompt(&mut self, open: bool, now: i64) {
        if std::mem::replace(&mut self.prompt_open, open) && !open && self.exit.is_none() {
            self.last_activity_at = self.last_activity_at.max(now);
        }
    }

    /// The process ended. Returns the proof recorded.
    pub fn note_exit(&mut self, stopped: bool, code: Option<i32>) -> ExitProof {
        let proof = match (stopped, code) {
            (true, _) => ExitProof::UserStop,
            (false, Some(code)) => ExitProof::ExitCode(code),
            (false, None) => ExitProof::ConnectionClosed,
        };
        self.exits += 1;
        self.exit = Some(proof);
        self.sent_unanswered = false;
        self.accepted_at = None;
        proof
    }

    /// A new process took over (a respawn): its exit is not the old one's.
    pub fn note_connected(&mut self) {
        self.exit = None;
    }
}

/// What a host knows and a fold does not.
#[derive(Debug, Default, Clone)]
pub struct HealthExtras {
    /// Whether the provider process is live, dormant or gone. The tracker's
    /// own `exit` wins when it has one, so this says only what to report
    /// while it does not: `Live` for a connected driver, `Dormant` for none.
    pub process_while_running: Process,
    /// The connection generation, for the edge ids.
    pub incarnation: String,
    /// When an armed retry fires because a provider limit closed the turn.
    pub limited_until: Option<i64>,
    pub limit_reason: Option<String>,
    /// The user stopped the session.
    pub interrupted: bool,
    /// A restart cut a turn short and the agent has not been told yet.
    pub recovery_note_owed: bool,
}

// No `children_active` here, deliberately. A driver cannot usefully supply it:
// `SessionHandle::refresh_health` overwrites it from the host's own periodic
// look at the lineage (`session_health.rs`: `inputs.children_active =
// children.working`), which is the only place that knows. A field a caller can
// set and nothing can read is worse than no field — the next person to pass it
// carefully would be wasting their care.

/// The one reading of a session's health from its fold and its tracker.
pub fn inputs_from(fold: &ChatThread, track: &HealthTrack, extras: HealthExtras) -> HealthInputs {
    let process = match track.exit {
        Some(proof) => Process::Exited(proof),
        None => extras.process_while_running,
    };
    HealthInputs {
        turn_active: fold.turn_active,
        continuations_open: fold.continuations_open,
        pending_unanswered_send: track.sent_unanswered,
        accepted_unstarted_since: track.accepted_at,
        background_active: fold
            .background_tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
            .count() as u32,
        // Overwritten by the host's own look (see the note on `HealthExtras`).
        children_active: 0,
        prompt: if fold.pending_permission().is_some() {
            Some(PromptKind::Permission)
        } else if fold.pending_question().is_some() {
            Some(PromptKind::Question)
        } else {
            None
        },
        prompt_request: fold
            .pending_permission()
            .map(|(_, req)| req.request_id.clone())
            .or_else(|| fold.pending_question().map(|(_, req)| req.request_id.clone())),
        limited_until: extras.limited_until,
        limit_reason: extras.limit_reason,
        // The fold's message when it has one: it folds in the stderr
        // diagnostic the raw result lacks. Only while no turn runs — a turn
        // that started has superseded the last one's failure.
        last_turn_failure: track
            .failed_turn
            .as_ref()
            .filter(|_| !fold.turn_active)
            .map(|raw| fold.last_error.clone().unwrap_or_else(|| raw.clone())),
        interrupted: extras.interrupted,
        process,
        restored: track.restored,
        recovery_note_owed: extras.recovery_note_owed,
        held_restored: track.held,
        queued: track.queued,
        last_activity_at: track.last_activity_at,
        turn_id: (track.turns > 0).then(|| format!("{}:{}", extras.incarnation, track.turns)),
        exit_seq: (track.exits > 0).then(|| format!("{}:{}", extras.incarnation, track.exits)),
    }
}

/// The error a dead process leaves on a session.
pub fn exit_message(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("Agent process exited (code {code})."),
        None => "Agent process exited.".into(),
    }
}
