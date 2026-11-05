//! Recovery turns for children a restart cut short.
//!
//! When this host wins the data directory over a previous one, the children
//! that one had working (the rows [`LineageStore::boot_reconcile`] parked)
//! lost their process mid-turn. A child spawned by an agent is told so by a
//! recovery turn of its own, because its parent is waiting on it; a root, or a
//! child the operator spawned, instead gets the note on its next message.
//!
//! A recovery turn builds the child's session (spawning its agent) and is
//! billed, so it waits until the parent's chat is open, at most
//! [`HostConfig::max_resumes`](crate::HostConfig) run at once (the rest wait
//! in order), and each is sent under a key derived from the child and the
//! restart, reserved in the host's receipts first: a second pass over the same
//! restart finds the key taken and sends nothing.
//!
//! A child is sent one only while its chat still owes the restart's note (a
//! lock given up and won back cut nothing short; a message someone else sent
//! it first carried the note already). It is sent as
//! [`SendSource::Recovery`], so a view that queued it behind a message the
//! user wrote meanwhile drops it there. The text is a bare continue line: the
//! view that drives the child prepends the note (what was cut short, which
//! background tasks stopped), so the agent reads both as one message.
//!
//! [`LineageStore::boot_reconcile`]: oximux_agents::lineage::LineageStore::boot_reconcile

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use oximux_agents::lineage::{LineageState, SpawnOrigin, WorkingSessions};
use oximux_agents::resume_governor::{Grant, ResumeGovernor, continuation_id};
use oximux_agents::send_gate::SendSource;
use oximux_agents::send_receipts::{KeyKind, ReceiptState, key_hash};

use crate::dispatcher::send_v2::send_uuid;
use crate::host_core::{HostCore, PriorOwner};

/// What a recovery turn says. The child's view puts its owed note ahead of it.
pub const CONTINUE_LINE: &str = "Continue the task you were working on.";

/// A recovery turn that never shows the child working in this long is given
/// up on, and its slot goes to the next child.
const NEVER_STARTED: Duration = Duration::from_secs(600);

/// How often the driver looks again with nothing changing (for
/// [`NEVER_STARTED`]).
const RECHECK: Duration = Duration::from_secs(30);

/// The restart these recoveries recover from: when the host that was cut short
/// started, or its pid when it did not record that. `None` (no previous host)
/// means nothing was cut short.
pub(crate) fn restart_epoch(prior: &PriorOwner) -> Option<u64> {
    match prior {
        PriorOwner::None => None,
        PriorOwner::Gone { record, .. } | PriorOwner::Unknown { record } => {
            Some(record.started_at_unix_s.unwrap_or(u64::from(record.pid)))
        }
    }
}

/// Start recovering `cut` (the children the previous host had working), on
/// the core's runtime, for as long as this process keeps the ownership
/// `generation` it won. Only children an agent spawned qualify; the rest are
/// left parked.
pub(crate) fn begin(core: &Arc<HostCore>, cut: Vec<String>, epoch: u64, generation: u64) {
    let candidates: Vec<String> = cut.into_iter().filter(|child| qualifies(core, child)).collect();
    if candidates.is_empty() {
        return;
    }
    tracing::info!(n = candidates.len(), "children cut short by the restart wait for recovery turns");
    let weak = Arc::downgrade(core);
    core.runtime().spawn(drive(weak, candidates, Restart { epoch, generation }));
}

/// The restart being recovered from, and the ownership recovering it.
#[derive(Clone, Copy)]
struct Restart {
    epoch: u64,
    generation: u64,
}

/// Spawned by an agent, still parked, its parent not ended (nobody waits on
/// it then), not working here already, and with no retry of its own armed
/// (the retry resumes it).
fn qualifies(core: &HostCore, child: &str) -> bool {
    let Ok(Some(row)) = core.lineage().row(child) else { return false };
    let parent_ended =
        core.lineage().row(&row.parent_id).ok().flatten().is_some_and(|p| p.state == LineageState::Ended);
    row.origin == SpawnOrigin::Agent
        && row.state == LineageState::Parked
        && !parent_ended
        && !core.working().is_working(child)
        && core.lineage().retries().live(child).ok().flatten().is_none()
}

/// A recovery turn that was sent.
struct Running {
    sent_at: Instant,
    seen_working: bool,
    /// The receipt of the line it went out on: settled once the agent took
    /// it, however briefly it then worked.
    line: String,
}

async fn drive(weak: Weak<HostCore>, mut waiting_parent: Vec<String>, restart: Restart) {
    let Some(core) = weak.upgrade() else { return };
    let mut changes = core.registry().subscribe_changes();
    let mut governor = ResumeGovernor::new(core.config().max_resumes);
    drop(core);
    let mut granted: VecDeque<String> = VecDeque::new();
    let mut running: HashMap<String, Running> = HashMap::new();
    loop {
        let Some(core) = weak.upgrade() else { return };
        // The data directory was given up (even if won back since): what is
        // owed now is the new ownership's to decide.
        if core.ownership() != Some(restart.generation) {
            return;
        }
        // A child waits until its parent's chat is open; one no longer owed a
        // turn (its parent ended, say) leaves the line.
        waiting_parent.retain(|child| {
            if !qualifies(&core, child) {
                return false;
            }
            let Ok(Some(row)) = core.lineage().row(child) else { return false };
            if core.registry().get(&row.parent_id).is_none() {
                return true;
            }
            if governor.request(child) == Grant::Granted {
                granted.push_back(child.clone());
            }
            false
        });
        // A sent turn is over once the child is idle after the agent took it
        // (or after it was seen working), when its session goes, or when it
        // never starts at all. The receipt is read, not only the working
        // flag: registry changes coalesce, so a short turn can start and end
        // between two looks.
        let receipts = core.lineage().receipts();
        let finished: Vec<String> = running
            .iter_mut()
            .filter_map(|(child, run)| {
                let working = core.working().is_working(child);
                run.seen_working |= working;
                let taken = receipts
                    .state(child, &run.line)
                    .ok()
                    .flatten()
                    .is_some_and(|s| matches!(s, ReceiptState::Accepted | ReceiptState::Rejected));
                let gone = core.registry().get(child).is_none();
                let started = run.seen_working || taken;
                let over = (started && !working) || gone || (!started && run.sent_at.elapsed() > NEVER_STARTED);
                over.then(|| child.clone())
            })
            .collect();
        for child in finished {
            running.remove(&child);
            granted.extend(governor.release(&child));
        }
        // One at a time: building a child's session builds its project's
        // panes on the desktop's UI thread.
        if let Some(child) = granted.pop_front() {
            match resume_one(&core, &child, restart).await {
                Some(line) => {
                    running.insert(child, Running { sent_at: Instant::now(), seen_working: false, line });
                }
                None => granted.extend(governor.release(&child)),
            }
            continue;
        }
        if governor.is_idle() && waiting_parent.is_empty() {
            return;
        }
        drop(core);
        tokio::select! {
            changed = changes.changed() => if changed.is_err() { return },
            _ = tokio::time::sleep(RECHECK) => {}
        }
    }
}

/// Send `child` its recovery turn; the receipt key of the line it went out
/// on. `None` when it is no longer owed one, was already sent one for this
/// restart, or could not be reached.
async fn resume_one(core: &HostCore, child: &str, restart: Restart) -> Option<String> {
    // Looked at again now: it may be working in this process already.
    if !qualifies(core, child) {
        return None;
    }
    let continuation = continuation_id(child, restart.epoch);
    let key = key_hash(KeyKind::V2, &continuation);
    let receipts = core.lineage().receipts();
    match receipts.reserve(child, &key, core.instance(), chrono::Local::now()) {
        Ok(None) => {}
        Ok(Some(_)) => return None,
        Err(err) => {
            tracing::warn!(%err, child, "recovery turn not reserved");
            return None;
        }
    }
    let forget = || {
        let _ = receipts.forget_pending(child, &key);
    };
    if let Some(catalog) = core.catalog()
        && let Err(err) = catalog.open(child).await
    {
        tracing::warn!(%err, child, "recovery turn: the child could not be opened");
        forget();
        return None;
    }
    // Opening can take a while; the directory may have changed hands since.
    if core.ownership() != Some(restart.generation) {
        forget();
        return None;
    }
    let Some(handle) = core.registry().get(child) else {
        forget();
        return None;
    };
    // Only while its chat still owes the note: a restart really cut its turn
    // short (not a lock given up and won back), and nobody has written to it
    // since (the note went with that message).
    if !handle.health_inputs().recovery_note_owed {
        forget();
        return None;
    }
    let uuid = send_uuid(&continuation);
    match handle.send_prompt_tagged(CONTINUE_LINE, &[], SendSource::Recovery, Some(uuid)) {
        Ok(()) => {
            tracing::info!(child, "recovery turn sent");
            Some(key_hash(KeyKind::Gate, &uuid.to_string()))
        }
        Err(err) => {
            tracing::warn!(%err, child, "recovery turn not sent");
            forget();
            None
        }
    }
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
