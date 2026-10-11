//! The verify loop: an action that waits for the screen to go idle and says
//! what changed (`--settle`), and `@eN` refs (`ax -i`, `tap @e7`). `wait` and
//! `assert` are in [`super::checks`].
//!
//! Idle has one rule per device class. On a simulator an accessibility read is
//! quick, so after the screen stops changing ([`FRAME_QUIET`] with no motion
//! on its frame clock) two reads at least [`AX_GAP`] apart must agree. On
//! Android and a real iPhone a read costs a second or more, so one read after
//! the screen goes quiet gives the diff (`settled (frame)`). Either way the
//! wait is capped, and a cap that runs out is reported, never an error: the
//! action happened.
//!
//! Every read goes through the hub's coalescer, and every read on this path
//! renumbers the caller's ref table, so the refs a reply prints are the ones
//! the next verb resolves.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{
    SimAxNodeWire, SimCmdWire, SimDiffLineWire, SimDiffOpWire, SimErrorWire, SimPointWire, SimRefNodeWire, SimReplyWire,
    SimSettleOutcomeWire, SimSettleWire,
};
use oximux_simulator::DeviceId;
use oximux_simulator::ax::{self, AxNode, Query};
use oximux_simulator::ax_refs::{self, DiffOp, MOVED_PT, RefError, Snapshot, TableKey};
use oximux_simulator::session::StreamSession;

use super::Target;
use super::ax_lookup::{self, Shown, read, read_once, touch_point, units_per_point};
use super::verbs::{self, Out, live_session};
use crate::shell::simulator::hub::SimulatorHub;

/// Always wait this long after an action before looking.
const MIN_WAIT: Duration = Duration::from_millis(150);
/// No motion on screen for this long counts as quiet.
const FRAME_QUIET: Duration = Duration::from_millis(250);
/// Two reads that agree must be at least this far apart (simulator).
const AX_GAP: Duration = Duration::from_millis(150);
/// How often the frame clock is looked at while the screen moves.
const TICK: Duration = Duration::from_millis(50);
/// Settle caps: a simulator, Android or a real iPhone, and app verbs.
const SIMULATOR_CAP: Duration = Duration::from_millis(3000);
const SLOW_CAP: Duration = Duration::from_millis(6000);
const APP_CAP: Duration = Duration::from_millis(8000);
/// The longest an agent may ask any wait to be.
pub(super) const MAX_WAIT: Duration = Duration::from_secs(60);
/// A table's last read stands in for an action's "before" while nothing has
/// moved on screen since, for at most this long.
const SNAPSHOT_AGE: Duration = Duration::from_secs(60);

/// What an action did, for its report.
#[derive(Debug, Default)]
pub(super) struct Acted {
    pub target: Option<SimAxNodeWire>,
    pub at: Option<SimPointWire>,
    pub warnings: Vec<String>,
    /// A read known to show the screen as the action starts (a settle's
    /// "before"): a tap by label aims with it rather than reading again.
    pub prior: Option<Arc<Vec<AxNode>>>,
}

impl Acted {
    pub(super) fn report(self, outcome: Option<SimSettleOutcomeWire>, started: Instant) -> SimSettleWire {
        SimSettleWire {
            outcome,
            elapsed_ms: millis(started.elapsed()),
            diff: Vec::new(),
            unchanged: 0,
            warnings: self.warnings,
            target: self.target,
            at: self.at,
        }
    }
}

pub(super) fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// Android and a real iPhone: an accessibility read takes a second or more.
pub(super) fn slow(session: &StreamSession) -> bool {
    session.android().is_some() || session.ios_device().is_some()
}

/// How long `action` may wait to settle (`timeout_ms` 0: its class default),
/// on a device whose reads are `slow` or not.
fn cap_for(action: &SimCmdWire, slow: bool, timeout_ms: u32) -> Duration {
    match (timeout_ms, action) {
        (0, SimCmdWire::Launch { .. } | SimCmdWire::OpenUrl { .. }) => APP_CAP,
        (0, _) if slow => SLOW_CAP,
        (0, _) => SIMULATOR_CAP,
        (ms, _) => Duration::from_millis(u64::from(ms)).clamp(Duration::from_millis(500), MAX_WAIT),
    }
}

/// One read of the tree, with the stream it came from (see [`Snapshot`]).
pub(super) struct Read {
    pub nodes: Arc<Vec<AxNode>>,
    pub at: Instant,
    pid: u32,
    epoch: u64,
}

impl Read {
    pub(super) fn snapshot(&self) -> Snapshot {
        Snapshot { taken_at: self.at, nodes: self.nodes.clone(), pid: self.pid, epoch: self.epoch }
    }
}

/// A read now: once (joining one in flight that started no earlier than
/// `not_before`), or `retrying` while the tree is empty.
pub(super) async fn read_now(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    not_before: Option<Instant>,
    retrying: bool,
    cx: &mut AsyncApp,
) -> Result<Read, SimErrorWire> {
    let (at, pid, epoch) = (Instant::now(), session.pid(), session.frame_clock().epoch);
    let nodes = if retrying { read(hub, udid, session, cx).await? } else { read_once(hub, udid, session, not_before, cx).await? };
    Ok(Read { nodes, at, pid, epoch })
}

/// Run `action`, wait for the screen to go idle, and report what changed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn settled(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    name: &str,
    action: SimCmdWire,
    timeout_ms: u32,
    refs: &TableKey,
    target: &Target,
    cx: &mut AsyncApp,
) -> Out {
    let session = live_session(hub, udid, cx).await?;
    let slow = slow(&session);
    let cap = cap_for(&action, slow, timeout_ms);
    let before = before(hub, udid, &session, refs, cx).await?;
    let (started, mut acted) = (Instant::now(), Acted { prior: Some(before.clone()), ..Acted::default() });
    verbs::execute(hub, udid, name, action, refs, &mut acted, target, cx).await?;
    let acted_at = Instant::now();
    // The action happened: from here nothing fails the verb (an agent's
    // retry would act twice); what goes wrong is a warning in the report.
    // An app verb may have woken the device: settle on the session now live.
    let session = match live_session(hub, udid, cx).await {
        Ok(session) => session,
        Err(e) => {
            acted.warnings.push(format!("could not watch the screen settle: {e}"));
            return Ok(SimReplyWire::Settled(acted.report(Some(SimSettleOutcomeWire::FrameBusy), started)));
        }
    };
    let pt = units_per_point(&session);
    let deadline = acted_at + cap;
    cx.background_executor().timer(MIN_WAIT).await;
    let quiet = |now| session.frame_clock().quiet_for(acted_at, now, FRAME_QUIET);
    let (mut after, mut failed): (Option<Read>, Option<SimErrorWire>) = (None, None);
    let mut last_hash: Option<(u64, Instant)> = None;
    let outcome = loop {
        let now = Instant::now();
        if now >= deadline {
            break if quiet(now) && !slow { SimSettleOutcomeWire::AxUnstable } else { SimSettleOutcomeWire::FrameBusy };
        }
        if !quiet(now) {
            cx.background_executor().timer(TICK).await;
            continue;
        }
        if slow {
            match read_now(hub, udid, &session, Some(now), false, cx).await {
                Ok(read) => after = Some(read),
                Err(e) => failed = Some(e),
            }
            break SimSettleOutcomeWire::SettledFrame;
        }
        if let Some((_, at)) = last_hash
            && now - at < AX_GAP
        {
            cx.background_executor().timer(AX_GAP - (now - at)).await;
            continue;
        }
        let read = match read_now(hub, udid, &session, Some(now), false, cx).await {
            Ok(read) => read,
            Err(e) => {
                failed = Some(e);
                break SimSettleOutcomeWire::AxUnstable;
            }
        };
        let hash = ax_refs::ax_hash(&read.nodes, pt);
        let agreed = last_hash.is_some_and(|(h, _)| h == hash);
        (last_hash, after) = (Some((hash, now)), Some(read));
        if agreed {
            break SimSettleOutcomeWire::Settled;
        }
    };
    // Out of time on a simulator: one more (quick) read, for the diff so far.
    if after.is_none() && failed.is_none() && !slow {
        match read_now(hub, udid, &session, Some(Instant::now()), false, cx).await {
            Ok(read) => after = Some(read),
            Err(e) => failed = Some(e),
        }
    }
    let mut report = acted.report(Some(outcome), started);
    match (after, failed) {
        (_, Some(e)) => report.warnings.push(e.to_string()),
        (None, None) => report.warnings.push("no accessibility read: the screen kept changing".into()),
        (Some(after), None) => match (Shown::new(hub, udid, &session, &before, cx), Shown::new(hub, udid, &session, &after.nodes, cx)) {
            (Ok(shown_before), Ok(shown_after)) => hub.update(cx, |hub, _| {
                let table = hub.ref_table(refs);
                table.assign(&after.nodes);
                let diff = ax_refs::diff(&before, &after.nodes, table);
                report.diff = diff
                    .lines
                    .iter()
                    .map(|l| {
                        let (op, shown) = match l.op {
                            DiffOp::Added => (SimDiffOpWire::Added, &shown_after),
                            DiffOp::Removed => (SimDiffOpWire::Removed, &shown_before),
                        };
                        SimDiffLineWire { op, r: l.r, node: shown.wire(l.node, 0) }
                    })
                    .collect();
                report.unchanged = diff.unchanged;
                table.last = Some(after.snapshot());
            }),
            (Err(e), _) | (_, Err(e)) => report.warnings.push(format!("could not map the accessibility tree: {e}")),
        },
    }
    Ok(SimReplyWire::Settled(report))
}

/// The caller's last read, while it still shows the screen: same stream, no
/// pause or resume since (the clock saw every picture), no motion since it
/// began, and not old. As good as a read now, without its cost.
fn still_current(hub: &Entity<SimulatorHub>, session: &StreamSession, refs: &TableKey, cx: &mut AsyncApp) -> Option<Snapshot> {
    let (clock, pid) = (session.frame_clock(), session.pid());
    hub.update(cx, |hub, _| hub.ref_table(refs).last.clone()).filter(|snap| {
        snap.pid == pid
            && snap.epoch == clock.epoch
            && snap.taken_at.elapsed() < SNAPSHOT_AGE
            && clock.last_motion_at.is_none_or(|motion| motion < snap.taken_at)
    })
}

/// The screen before an action: the caller's last read while still current,
/// else a read now (joining one in flight), kept for the refs that follow.
async fn before(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, refs: &TableKey, cx: &mut AsyncApp) -> Result<Arc<Vec<AxNode>>, SimErrorWire> {
    if let Some(snap) = still_current(hub, session, refs, cx) {
        return Ok(snap.nodes);
    }
    let read = read_now(hub, udid, session, None, false, cx).await?;
    hub.update(cx, |hub, _| {
        let table = hub.ref_table(refs);
        table.assign(&read.nodes);
        table.last = Some(read.snapshot());
    });
    Ok(read.nodes)
}

/// Tap by label or identifier: the element's touch point, noted in `acted`
/// (found in the settle's "before" read when there is one).
pub(super) async fn aim(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    query: Query<'_>,
    what: &str,
    acted: &mut Acted,
    cx: &mut AsyncApp,
) -> Result<(f64, f64), SimErrorWire> {
    if let Some(prior) = acted.prior.clone()
        && let Some(node) = ax::find(&prior, query).cloned()
    {
        note_target(hub, udid, session, &prior, &node, acted, cx);
        return Ok(touch_point(session, &prior, &node));
    }
    let (node, nodes) = ax_lookup::element(hub, udid, session, query, what, cx).await?;
    note_target(hub, udid, session, &nodes, &node, acted, cx);
    Ok(touch_point(session, &nodes, &node))
}

/// Tap by ref: its element found by its key, in the caller's last read while
/// still current (a ref from `ax -i` or a settle a moment ago), else in a
/// fresh one.
pub(super) async fn aim_ref(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    r: u32,
    refs: &TableKey,
    acted: &mut Acted,
    cx: &mut AsyncApp,
) -> Result<(f64, f64), SimErrorWire> {
    let read = match still_current(hub, session, refs, cx) {
        Some(snap) => Read { nodes: snap.nodes, at: snap.taken_at, pid: snap.pid, epoch: snap.epoch },
        None => read_now(hub, udid, session, None, true, cx).await?,
    };
    let pt = units_per_point(session);
    let found = hub.update(cx, |hub, _| {
        let table = hub.ref_table(refs);
        let found = table.resolve(r, &read.nodes, pt).map(|f| (f.node.clone(), f.moved_pt));
        table.assign(&read.nodes);
        table.last = Some(read.snapshot());
        found
    });
    let (node, moved_pt) = found.map_err(|e| stale(r, e))?;
    note_target(hub, udid, session, &read.nodes, &node, acted, cx);
    if moved_pt > MOVED_PT {
        acted.warnings.push(format!("moved: @e{r} was {moved_pt:.0} pt from where it was last seen; tapped where it is now"));
    }
    Ok(touch_point(session, &read.nodes, &node))
}

/// Best effort: a tap still happens if its report cannot be mapped.
fn note_target(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, nodes: &[AxNode], node: &AxNode, acted: &mut Acted, cx: &mut AsyncApp) {
    if let Ok(shown) = Shown::new(hub, udid, session, nodes, cx) {
        (acted.target, acted.at) = (Some(shown.wire(node, 0)), Some(shown.center(node)));
    }
}

pub(super) fn stale(r: u32, e: RefError) -> SimErrorWire {
    SimErrorWire::StaleRef(match e {
        RefError::Unknown => format!("no element has ref @e{r}; run `oximux sim ax -i`"),
        RefError::Gone | RefError::Ambiguous => format!("stale ref @e{r}, run `oximux sim ax -i`"),
    })
}

/// `ax -i`: the elements that can be acted on, numbered.
pub(super) async fn ax_refs(hub: &Entity<SimulatorHub>, udid: &DeviceId, max: u32, refs: &TableKey, cx: &mut AsyncApp) -> Out {
    let session = live_session(hub, udid, cx).await?;
    let read = read_now(hub, udid, &session, None, true, cx).await?;
    let shown = Shown::new(hub, udid, &session, &read.nodes, cx)?;
    let listed = hub.update(cx, |hub, _| {
        let table = hub.ref_table(refs);
        let listed: Vec<SimRefNodeWire> =
            table.assign(&read.nodes).into_iter().take(max.clamp(1, 5000) as usize).map(|(r, n)| SimRefNodeWire { r, node: shown.wire(n, 0) }).collect();
        table.last = Some(read.snapshot());
        listed
    });
    Ok(SimReplyWire::AxRefs(listed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::simulator::SimTargetWire;

    /// 3 s on a simulator, 6 s where reads are slow, 8 s for app verbs on
    /// any class; an explicit ask is clamped to 0.5–60 s.
    #[test]
    fn caps_follow_the_class_the_verb_and_the_ask() {
        let tap = SimCmdWire::Tap(SimTargetWire::Ref(1));
        let launch = SimCmdWire::Launch { bundle_id: "a.b".into(), relaunch: false };
        let url = SimCmdWire::OpenUrl { url: "https://a".into() };
        assert_eq!(cap_for(&tap, false, 0), Duration::from_millis(3000));
        assert_eq!(cap_for(&tap, true, 0), Duration::from_millis(6000));
        assert_eq!((cap_for(&launch, false, 0), cap_for(&url, true, 0)), (APP_CAP, APP_CAP));
        assert_eq!(cap_for(&tap, true, 100), Duration::from_millis(500));
        assert_eq!(cap_for(&launch, false, 120_000), MAX_WAIT);
        assert_eq!(cap_for(&tap, false, 4500), Duration::from_millis(4500));
    }
}
