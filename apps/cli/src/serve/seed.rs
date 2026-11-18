//! The one reading of a persisted blob into the state a session starts from.
//!
//! **Two readers, and they must not disagree.** The pump seeds the session it
//! is about to run from this; the catalog answers
//! [`SessionCatalog::verdict`](oximux_remote_host::catalog::SessionCatalog::verdict)
//! from it for a session nothing is running, straight off disk. A dormant
//! session that reported one verdict and then reported another the moment a
//! message opened it would be worth less than no verdict at all — so both come
//! from the same state, by the same rules, and the only differences are the
//! ones a missing process forces (see [`crate::serve::catalog`]).
//!
//! Written twice these would drift the way every other pair in this project
//! has: not all at once, but one field at a time, each change plausible on its
//! own.

use oximux_agents::health_track::{self, HealthTrack};
use oximux_agents::thread::{ChatThread, TaskStatus};

use super::blob::{BlobQueuedSend, ChatBlob};

/// The state a persisted session starts from.
pub struct Seeded {
    /// Its transcript, rehydrated.
    pub fold: ChatThread,
    /// The connection facts a fold does not keep, as the last host left them.
    pub track: HealthTrack,
    /// The note a restart owes the agent, when it owes one. Goes with the next
    /// message; an earlier restart's unsent note is kept rather than replaced,
    /// because the agent needs to hear about the restart that interrupted it
    /// and not the latest one.
    pub note: Option<String>,
    /// What the last host had queued and never wrote, to go back into the gate
    /// **held**.
    pub held: Vec<BlobQueuedSend>,
    /// The blob itself, for whoever goes on to write it. Drained of the parts
    /// above (the fold owns the entries now, the gate owns the queue) and
    /// every save refills them from the fold, so what it still carries is the
    /// launch facts and — through `extra` — whatever a newer build wrote that
    /// this one must not strip.
    pub blob: ChatBlob,
}

/// Read one persisted blob into the state above.
///
/// `restored` is set for a session with history behind it, because its backend
/// replays nothing: until a frame arrives, nothing has confirmed that the
/// conversation on disk is the one the process is now holding. That is
/// `RestoredUnconfirmed`, a truer first answer than `Idle`, and the first frame
/// clears it (`note_event`).
///
/// `prior_host_gone` stays false. It is a claim that the previous host's
/// processes are provably gone, which only the won host lock and its record can
/// say, and neither reader has that. Claiming it would turn a reopened session
/// that had been mid-turn into `Interrupted` on no evidence. The proof half is
/// F14's.
pub fn seeded(session_id: &str, mut blob: ChatBlob) -> Seeded {
    let mut fold = ChatThread::rehydrated(
        Some(session_id.to_string()),
        blob.model.clone(),
        std::mem::take(&mut blob.entries),
        std::mem::take(&mut blob.slash_commands),
    );
    fold.session_meta = blob.session_meta.clone();
    let reopened = !fold.entries.is_empty();
    // The blob's own answer is preferred over the fold's: `mid_turn` was
    // written by the host that was actually interrupted, while the fold only
    // shows a turn left marked running on disk. They agree in the ordinary
    // case; where they differ, the writer knew.
    let was_mid_turn = blob.mid_turn || mid_turn(&fold);
    let mut track = if reopened {
        HealthTrack::restored(was_mid_turn, /* prior_host_gone */ false)
    } else {
        HealthTrack::default()
    };
    // A session that died failing comes back failing. The fold keeps no
    // durable trace of it — `last_error` is transient and an errored
    // `TurnEnded` leaves no entry — so the failure is restored from the blob
    // into both the tracker (which decides the health and holds the queue)
    // and the fold (whose text the health reports). Restored into only one of
    // the two, the session would read failed with no reason, or carry a reason
    // nothing reads.
    //
    // The next send clears both: `note_sent` drops the tracker's, and a user
    // message clears the fold's.
    if let Some(failure) = blob.last_failure.clone() {
        health_track::record_failure(&mut fold, &mut track, failure);
    }
    let held = std::mem::take(&mut blob.queued);
    // The count beside the lines that justify it. A pump publishes from its
    // gate instead — one read, beside the publish, so it cannot drift from
    // what is about to go out — and sets this same number over the top of this
    // one. A reader with no gate (the dormant verdict) has only this.
    track.held = held.len() as u32;
    let note = blob.recovery_note.clone().or_else(|| {
        let at = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        was_mid_turn.then(|| oximux_agents::recovery_note::build(&at, &blob.stopped_tasks))
    });
    Seeded { fold, track, note, held, blob }
}

/// What a session with nothing running it is, read from its persisted blob.
///
/// The last gap in reading a session without starting it. A verdict — failed,
/// and why; a restart's note still owed; messages held from before it — is
/// published by whatever drives the session, so with nothing running there was
/// no publisher and a client asking after a *dormant* session learned nothing.
/// That is exactly the session a person most needs an answer about: one whose
/// provider died in the night, which nothing has touched since.
///
/// Computed from [`seeded`], so this is the verdict the session will report
/// once something opens it rather than a second opinion formed on the way past
/// — barring the one difference below, which a missing process forces.
pub fn verdict(session_id: &str, blob: ChatBlob) -> oximux_agents::health::SessionHealth {
    use oximux_agents::health::{HealthConfig, Process, compute};
    use oximux_agents::health_track::{HealthExtras, inputs_from};

    let Seeded { fold, mut track, note, .. } = seeded(session_id, blob);
    // The one deliberate difference, and it is about the process rather than
    // the session: `restored` is the claim that something rebuilt this
    // conversation and no frame has confirmed it yet (`RestoredUnconfirmed`).
    // Nothing has rebuilt anything here. With no process at all the honest
    // liveness is `Unverifiable` — "we hold nothing that can tell" — which is
    // what dropping it leaves.
    //
    // What the restart left behind is still read: the failure, the owed note,
    // the held queue. Those are facts about the session, and they are what
    // this answer is for.
    track.restored = None;
    compute(
        &inputs_from(
            &fold,
            &track,
            HealthExtras {
                // No process, which is not death: `Dormant`. The tracker
                // carries no exit either, because an exit is something a host
                // watched happen and this host watched nothing — a failure the
                // last one recorded arrives as the failed turn instead, which
                // is the fact it actually persisted.
                process_while_running: Process::Dormant,
                recovery_note_owed: note.is_some(),
                // An armed retry lives in the retry driver's memory and is not
                // persisted, so a restart loses it and claiming one here would
                // be inventing it. It would also be inert: nothing reads
                // `limited_until` for a session with no process. `interrupted`
                // is the same — serve has no Stop of its own yet (F7's
                // `Cancel`), so it stays unclaimed rather than guessed.
                ..HealthExtras::default()
            },
        ),
        oximux_agents::session_health::now_ms(),
        // Thresholds cannot bite: the only one that reads the clock marks a
        // *working* session stalled, and a session with no process never reads
        // working.
        &HealthConfig::default(),
    )
}

/// Whether a restart stopping now would cut something short.
///
/// One expression, used by the save that records it, the restore that reads it
/// back, and the seeding above: a session that saved `mid_turn` one way and
/// judged it another would owe the agent a note on one host and not the other.
pub fn mid_turn(fold: &ChatThread) -> bool {
    fold.turn_active
        || fold.continuations_open > 0
        || fold.background_tasks.iter().any(|t| t.status == TaskStatus::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::health::{HealthStatus, Liveness};
    use oximux_agents::thread::ThreadEntry;

    fn with_history(session_id: &str) -> ChatBlob {
        let mut blob = ChatBlob::new(session_id.into());
        blob.entries = vec![ThreadEntry::User {
            text: "ship it".into(),
            images: Vec::new(),
            checkpoint: None,
        }];
        blob
    }

    fn queued(text: &str) -> BlobQueuedSend {
        BlobQueuedSend { text: text.into(), source: "user".into(), send_key: None }
    }

    /// The whole point of the dormant verdict: a session whose provider died
    /// in the night says so before anything reopens it, in the words the host
    /// recorded rather than a paraphrase of them.
    #[test]
    fn a_failure_the_last_host_recorded_is_what_a_dormant_session_reports() {
        let mut blob = with_history("s-1");
        blob.last_failure = Some("agent process exited (code 137)".into());

        let v = verdict("s-1", blob);

        assert_eq!(v.status, HealthStatus::Failed);
        assert_eq!(
            v.reason.as_deref(),
            Some("agent process exited (code 137)"),
            "the sentence the host persisted, so the session explains itself the same way \
             before and after a restart",
        );
    }

    /// And it claims no process while doing it. `Unverifiable` is "we hold
    /// nothing that can tell"; `RestoredUnconfirmed` would say something
    /// rebuilt this conversation, and nothing has, while `Exited` would claim
    /// this host watched it die.
    #[test]
    fn a_dormant_verdict_claims_no_process_of_its_own() {
        let mut blob = with_history("s-1");
        blob.last_failure = Some("boom".into());

        let v = verdict("s-1", blob);

        assert_eq!(v.liveness, Liveness::Unverifiable);
        assert!(!v.exited_unexpectedly, "nothing here watched a process end");
    }

    /// A session nothing recorded a failure for is not made to look broken —
    /// and not made to look busy either. Idle is the honest reading, and the
    /// liveness beside it says we cannot confirm even that.
    #[test]
    fn a_session_that_ended_well_reads_idle_rather_than_failed() {
        let v = verdict("s-1", with_history("s-1"));

        assert_eq!((v.status, v.liveness), (HealthStatus::Idle, Liveness::Unverifiable));
        assert_eq!(v.reason, None, "no reason to give is better than one invented");
    }

    /// The hold and the owed note reach a dormant reader too. They are the
    /// other two things a restart leaves behind, and unlike the verdict itself
    /// each asks the person reading it to do something: release the hold, or
    /// know that their next message will carry a paragraph they did not write.
    #[test]
    fn the_hold_and_the_owed_note_reach_a_dormant_reader() {
        let mut blob = with_history("s-1");
        blob.mid_turn = true;
        blob.queued = vec![queued("look at this later"), queued("and this")];

        let v = verdict("s-1", blob);

        assert_eq!(v.held_restored, 2, "both lines the last host never wrote");
        assert_eq!(
            v.queued, 0,
            "and nothing is on its way: there is no gate and no turn to queue behind, so \
             reporting these as waiting would invite a wait that never ends",
        );
        assert!(v.recovery_note_owed, "the restart cut a turn short and nothing has sent the note");
    }

    /// A session with no history behind it was never interrupted by anything,
    /// so it reads as what it is rather than as a restored one.
    #[test]
    fn a_session_with_nothing_behind_it_reads_idle() {
        let v = verdict("s-1", ChatBlob::new("s-1".into()));

        assert_eq!((v.status, v.liveness), (HealthStatus::Idle, Liveness::Unverifiable));
        assert!(!v.recovery_note_owed);
    }
}
