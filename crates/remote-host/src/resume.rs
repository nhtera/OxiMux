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
//! The text is a bare continue line. The view that drives the child prepends
//! the note it owes (what was cut short, which background tasks stopped), so
//! the agent reads both as one message; without a note the line still reads
//! right on its own.
//!
//! [`LineageStore::boot_reconcile`]: oximux_agents::lineage::LineageStore::boot_reconcile

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use oximux_agents::lineage::{LineageState, SpawnOrigin, WorkingSessions};
use oximux_agents::resume_governor::{Grant, ResumeGovernor, continuation_id};
use oximux_agents::send_gate::SendSource;
use oximux_agents::send_receipts::{KeyKind, key_hash};

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
/// the core's runtime. Only children an agent spawned qualify; the rest are
/// left parked.
pub(crate) fn begin(core: &Arc<HostCore>, cut: Vec<String>, epoch: u64) {
    let candidates: Vec<String> = cut.into_iter().filter(|child| qualifies(core, child)).collect();
    if candidates.is_empty() {
        return;
    }
    tracing::info!(n = candidates.len(), "children cut short by the restart wait for recovery turns");
    let weak = Arc::downgrade(core);
    core.runtime().spawn(drive(weak, candidates, epoch));
}

/// Spawned by an agent, still parked, not working here already, and with no
/// retry of its own armed (the retry resumes it).
fn qualifies(core: &HostCore, child: &str) -> bool {
    let Ok(Some(row)) = core.lineage().row(child) else { return false };
    row.origin == SpawnOrigin::Agent
        && row.state == LineageState::Parked
        && !core.working().is_working(child)
        && core.lineage().retries().live(child).ok().flatten().is_none()
}

/// A recovery turn that was sent.
struct Running {
    sent_at: Instant,
    seen_working: bool,
}

async fn drive(weak: Weak<HostCore>, mut waiting_parent: Vec<String>, epoch: u64) {
    let Some(core) = weak.upgrade() else { return };
    let mut changes = core.registry().subscribe_changes();
    let mut governor = ResumeGovernor::new(core.config().max_resumes);
    drop(core);
    let mut granted: VecDeque<String> = VecDeque::new();
    let mut running: HashMap<String, Running> = HashMap::new();
    loop {
        let Some(core) = weak.upgrade() else { return };
        // The data directory went to another host: its recoveries are its own.
        if !core.is_owner() {
            return;
        }
        // A child waits until its parent's chat is open; one whose parent
        // ended is not owed a turn.
        waiting_parent.retain(|child| {
            let Ok(Some(row)) = core.lineage().row(child) else { return false };
            let parent_ended =
                core.lineage().row(&row.parent_id).ok().flatten().is_some_and(|p| p.state == LineageState::Ended);
            if parent_ended || row.state != LineageState::Parked {
                return false;
            }
            if core.registry().get(&row.parent_id).is_none() {
                return true;
            }
            if governor.request(child) == Grant::Granted {
                granted.push_back(child.clone());
            }
            false
        });
        // A sent turn ends when the child stops working after it started, its
        // session goes, or it never starts at all.
        let finished: Vec<String> = running
            .iter_mut()
            .filter_map(|(child, run)| {
                let working = core.working().is_working(child);
                run.seen_working |= working;
                let gone = core.registry().get(child).is_none();
                let over = (run.seen_working && !working) || gone || (!run.seen_working && run.sent_at.elapsed() > NEVER_STARTED);
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
            if resume_one(&core, &child, epoch).await {
                running.insert(child, Running { sent_at: Instant::now(), seen_working: false });
            } else {
                granted.extend(governor.release(&child));
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

/// Send `child` its recovery turn. `false` when it is no longer owed one, was
/// already sent one for this restart, or could not be reached.
async fn resume_one(core: &HostCore, child: &str, epoch: u64) -> bool {
    // Looked at again now: the user may have reached it first (and with it
    // the note), or it may be working in this process already.
    if !qualifies(core, child) {
        return false;
    }
    let continuation = continuation_id(child, epoch);
    let key = key_hash(KeyKind::V2, &continuation);
    let receipts = core.lineage().receipts();
    match receipts.reserve(child, &key, core.instance(), chrono::Local::now()) {
        Ok(None) => {}
        Ok(Some(_)) => return false,
        Err(err) => {
            tracing::warn!(%err, child, "recovery turn not reserved");
            return false;
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
        return false;
    }
    let Some(handle) = core.registry().get(child) else {
        forget();
        return false;
    };
    // The view knows better than the row whether a turn was cut short (a
    // Stop just before the quit parks the row a moment late).
    if handle.health_inputs().restored.is_some_and(|r| !r.was_mid_turn) {
        forget();
        return false;
    }
    match handle.send_prompt_tagged(CONTINUE_LINE, &[], SendSource::Agent, Some(send_uuid(&continuation))) {
        Ok(()) => {
            tracing::info!(child, "recovery turn sent");
            true
        }
        Err(err) => {
            tracing::warn!(%err, child, "recovery turn not sent");
            forget();
            false
        }
    }
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
