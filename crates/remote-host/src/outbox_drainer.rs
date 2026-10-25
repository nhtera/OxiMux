//! Delivers what children owe a parent, through the parent's send gate.
//!
//! One task per host. It moves a parent's pending outbox rows into that
//! parent's [`SendGate`] as **one** batch (one envelope, one key), so a Stop,
//! a running turn, an armed retry or an open prompt holds them like any other
//! send; the gate's owner writes the line when it may. Rows are settled from
//! what comes back, never from the write itself:
//! - the owner reports the write (`sent`), or its failure (the batch backs off),
//!   or a line it took back at a Stop (pending again, held for the user);
//! - the CLI's receipt says it started on the line (`accepted`) and which turn
//!   carried it (`delivered`);
//! - no receipt within [`ACCEPT_WITHIN`] of the write: `unconfirmed`, shown and
//!   never resent on its own.
//!
//! A batch still waiting in the gate when a newer event arrives is taken back
//! and re-sent together with it: one envelope per run of child activity.
//!
//! A parent with no session (dormant: its tab is not open) is never opened to
//! deliver; its rows wait for the next time it binds.

use std::collections::HashMap;
use std::sync::{Mutex, Weak};
use std::time::Duration;

use oximux_agents::envelope;
use oximux_agents::lineage::LineageState;
use oximux_agents::send_gate::{DeliveryMode, SendGate, SendSource};
use oximux_agents::session_registry::SessionHandle;
use oximux_agents::thread::ThreadEvent;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

use crate::host_core::HostCore;

/// How long a written line may go without the CLI saying it started on it.
pub const ACCEPT_WITHIN: Duration = Duration::from_secs(30);

/// How often pending rows are looked at again without a nudge (backoff
/// expiry, a parent that bound while nothing else happened).
const SWEEP: Duration = Duration::from_secs(5);

/// What reaches the drainer.
pub(crate) enum DrainMsg {
    /// A child event was recorded.
    Wake,
    /// The gate's owner wrote the item queued under `uuid` (or failed to).
    Sent { uuid: Uuid, ok: bool },
    /// A receipt from a parent's CLI.
    Receipt(ThreadEvent),
}

/// A parent whose gate and receipts the drainer is attached to.
struct Link {
    incarnation: String,
    receipts: tokio::task::AbortHandle,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.receipts.abort();
    }
}

pub(crate) async fn run(core: Weak<HostCore>, tx: mpsc::UnboundedSender<DrainMsg>, mut rx: mpsc::UnboundedReceiver<DrainMsg>) {
    let mut changes = match core.upgrade() {
        Some(core) => core.registry().subscribe_changes(),
        None => return,
    };
    let mut links: HashMap<String, Link> = HashMap::new();
    let mut awaiting: HashMap<Uuid, Instant> = HashMap::new();
    let mut due: Option<Instant> = Some(Instant::now());
    loop {
        let next_deadline = awaiting.values().min().copied();
        let wake_at = [due, next_deadline, Some(Instant::now() + SWEEP)].into_iter().flatten().min().unwrap();
        tokio::select! {
            msg = rx.recv() => {
                let Some(msg) = msg else { return };
                let Some(core) = core.upgrade() else { return };
                handle(&core, msg, &mut awaiting, &mut due);
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
                // A parent may have bound. Never cuts a debounce short: the
                // events it is gathering would go out one by one.
                due.get_or_insert_with(Instant::now);
            }
            () = tokio::time::sleep_until(wake_at) => {}
        }
        let Some(core) = core.upgrade() else { return };
        let now = Instant::now();
        awaiting.retain(|uuid, deadline| {
            if *deadline > now {
                return true;
            }
            if let Err(err) = core.outbox().mark_unconfirmed(*uuid) {
                tracing::warn!(%err, "outbox: could not mark a line unconfirmed");
            }
            false
        });
        unlink_gone(&core, &mut links);
        link_in_flight(&core, &tx, &mut links);
        if due.is_some_and(|d| d <= now) || wake_at <= now {
            due = None;
            drain(&core, &tx, &mut links);
        }
    }
}

fn handle(core: &HostCore, msg: DrainMsg, awaiting: &mut HashMap<Uuid, Instant>, due: &mut Option<Instant>) {
    let outbox = core.outbox();
    let now = chrono::Local::now();
    let result = match msg {
        DrainMsg::Wake => {
            // Debounce: children that finish together go out together.
            due.get_or_insert_with(|| Instant::now() + core.event_debounce());
            Ok(())
        }
        DrainMsg::Sent { uuid, ok: true } => outbox.mark_sent(uuid).map(|n| {
            if n > 0 {
                awaiting.insert(uuid, Instant::now() + ACCEPT_WITHIN);
            }
        }),
        // Not written (backs off, same key), or written and then taken back
        // at a Stop before the agent started on it (pending again, new key).
        DrainMsg::Sent { uuid, ok: false } => {
            awaiting.remove(&uuid);
            outbox.release_with_backoff(uuid, now).and_then(|()| outbox.unclaim(uuid).map(drop))
        }
        DrainMsg::Receipt(ThreadEvent::UserMessageAccepted { uuid }) => match Uuid::parse_str(&uuid) {
            Ok(uuid) => {
                awaiting.remove(&uuid);
                outbox.mark_accepted(uuid, now).map(drop)
            }
            Err(_) => Ok(()),
        },
        DrainMsg::Receipt(ThreadEvent::UserMessagesDelivered { uuids }) => {
            let mut result = Ok(());
            for uuid in uuids.iter().filter_map(|u| Uuid::parse_str(u).ok()) {
                awaiting.remove(&uuid);
                result = result.and(outbox.mark_delivered(uuid, now).map(drop));
            }
            result
        }
        // The CLI dropped the line unstarted: the rows go out again (new key).
        DrainMsg::Receipt(ThreadEvent::UserMessageDropped { uuid }) => match Uuid::parse_str(&uuid) {
            Ok(uuid) => {
                awaiting.remove(&uuid);
                outbox.unclaim(uuid).map(drop)
            }
            Err(_) => Ok(()),
        },
        DrainMsg::Receipt(_) => Ok(()),
    };
    if let Err(err) = result {
        tracing::warn!(%err, "outbox: settling a delivery failed");
    }
}

/// Let go of parents whose session went away: what their gate still held
/// goes back to pending, what was written but not accepted is unconfirmed.
fn unlink_gone(core: &HostCore, links: &mut HashMap<String, Link>) {
    links.retain(|parent, link| {
        let alive = core.registry().get(parent).is_some_and(|h| h.incarnation() == link.incarnation);
        if !alive && let Err(err) = core.outbox().release_parent(parent) {
            tracing::warn!(%err, %parent, "outbox: releasing a parent's batches failed");
        }
        alive
    });
}

/// Hear what becomes of what is queued in `parent`'s gate: the owner's write
/// reports and the CLI's receipts. Done before anything is queued there, and
/// again for any parent with a batch in flight (a rekey moves a held batch to
/// a new session handle), so no batch is in a gate nobody listens to.
fn link(
    core: &HostCore,
    tx: &mpsc::UnboundedSender<DrainMsg>,
    links: &mut HashMap<String, Link>,
    parent: &str,
    handle: &SessionHandle,
    gate: &Mutex<SendGate>,
) {
    if links.get(parent).is_some_and(|l| l.incarnation == handle.incarnation()) {
        return;
    }
    let sent = tx.clone();
    gate.lock().unwrap().set_sent_hook(move |uuid, ok| {
        let _ = sent.send(DrainMsg::Sent { uuid, ok });
    });
    let mut receipts = handle.subscribe_receipts();
    let forward = tx.clone();
    let task = core.runtime().spawn(async move {
        loop {
            match receipts.recv().await {
                Ok(receipt) => {
                    if forward.send(DrainMsg::Receipt(receipt)).is_err() {
                        return;
                    }
                }
                // Missed some: the 30 s window and late proof cover them.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            }
        }
    });
    links.insert(parent.to_string(), Link { incarnation: handle.incarnation().to_string(), receipts: task.abort_handle() });
}

/// Listen to every registered parent with a batch in its gate or on the wire.
fn link_in_flight(core: &HostCore, tx: &mpsc::UnboundedSender<DrainMsg>, links: &mut HashMap<String, Link>) {
    let parents = match core.outbox().parents_in_flight() {
        Ok(parents) => parents,
        Err(err) => {
            tracing::warn!(%err, "outbox: read failed");
            return;
        }
    };
    for parent in parents {
        if let Some(handle) = core.registry().get(&parent)
            && let Some(gate) = handle.gate()
        {
            link(core, tx, links, &parent, &handle, &gate);
        }
    }
}

/// Move every parent's due rows into its gate, one batch per parent. The
/// gate's lock is never held across a database write: the UI takes it on
/// every repaint.
fn drain(core: &HostCore, tx: &mpsc::UnboundedSender<DrainMsg>, links: &mut HashMap<String, Link>) {
    if !core.is_owner() {
        return;
    }
    let outbox = core.outbox();
    let parents = match outbox.parents_with_pending(chrono::Local::now()) {
        Ok(parents) => parents,
        Err(err) => {
            tracing::warn!(%err, "outbox: read failed");
            return;
        }
    };
    let mode = if core.config().steer_events { DeliveryMode::Steer } else { DeliveryMode::Queue };
    for parent in parents {
        if core.lineage().row(&parent).ok().flatten().is_some_and(|r| r.state == LineageState::Ended) {
            if let Err(err) = outbox.settle_undeliverable(&parent) {
                tracing::warn!(%err, %parent, "outbox: settling an ended parent failed");
            }
            continue;
        }
        let Some(handle) = core.registry().get(&parent) else { continue };
        let Some(gate) = handle.gate() else { continue };
        // A batch still waiting in the gate goes out again with the new rows.
        let held: Vec<Uuid> = {
            let mut gate = gate.lock().unwrap();
            if !gate.has_waker() {
                continue;
            }
            let held: Vec<(u64, Uuid)> = gate
                .queued()
                .filter(|q| q.source == SendSource::ParentEvent)
                .map(|q| (q.id, q.uuid))
                .collect();
            held.into_iter().map(|(id, uuid)| {
                gate.remove(id);
                uuid
            }).collect()
        };
        link(core, tx, links, &parent, &handle, &gate);
        for uuid in held {
            if let Err(err) = outbox.unclaim(uuid) {
                tracing::warn!(%err, "outbox: taking a held batch back failed");
            }
        }
        let batch = match outbox.claim(&parent, core.instance(), chrono::Local::now()) {
            Ok(Some(batch)) => batch,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(%err, %parent, "outbox: claim failed");
                continue;
            }
        };
        let text = envelope::render(&batch.events());
        gate.lock().unwrap().push(text, Vec::new(), SendSource::ParentEvent, mode, Some(batch.key));
    }
}
