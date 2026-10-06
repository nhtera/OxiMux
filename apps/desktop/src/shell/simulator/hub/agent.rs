//! The hub's side of agent control (`oximux sim …`): per-device consent, the
//! "Agent is using this device" badge, the device's pixel scale, and waking a
//! device an agent needs.
//!
//! Ownership, stated once because P7's recording bugs came from mixing them:
//! **consent and the badge belong to the device** (an approval covers every
//! worktree; the screen that moves is the device's), while **a consent request
//! belongs to the worktree that asked** (it shows where that worktree's panel
//! is, and nowhere else).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_simulator::{DeviceId, Source};
use oximux_simulator::consent::{Consent, State, Verdict};
use oximux_simulator::registry::{Phase, WorktreeKey};
use oximux_storage::{SimApproval, SimApprovalRepo};

use super::{HubEvent, SimulatorHub};
use crate::app_settings::sim_state_keys;

/// How long the badge stays after an agent's last verb.
pub(crate) const AGENT_BADGE: Duration = Duration::from_secs(2);

/// How long an "Install … on this phone?" question waits for the user.
pub(crate) const INSTALL_ASK_TTL: Duration = Duration::from_secs(90);

/// An agent's install on a real device, waiting for the user's yes. Asked
/// every time: approval to control a phone is not approval to put apps on it.
struct InstallAsk {
    id: u64,
    udid: DeviceId,
    worktree: WorktreeKey,
    device_name: String,
    app: String,
    asked: Instant,
    answer: Option<bool>,
}

/// Where an install question stands, for the waiting agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallAnswer {
    Pending,
    Allowed,
    Refused,
    /// Expired or dropped (the worktree moved on): not allowed.
    Gone,
}

/// Agent-control state the hub keeps.
pub(crate) struct AgentState {
    consent: Consent,
    /// Where approvals persist. `None` in tests; grants then live in memory.
    approvals: Option<SimApprovalRepo>,
    /// The approvals with their names and dates, for Settings (kept here so
    /// the pane never reads the database while it paints).
    granted: Vec<SimApproval>,
    /// Devices the user shut down from the panel: an agent may not boot them
    /// again until the user reconnects or someone attaches explicitly.
    /// Persisted: terminal agents outlive a relaunch through the relay.
    pub(super) stopped: HashSet<DeviceId>,
    /// Per device: the badge shows until this instant.
    active_until: HashMap<DeviceId, Instant>,
    /// Per device: agent verbs still running (a long one — a boot, an
    /// install — outlives the badge's two seconds).
    in_flight: HashMap<DeviceId, u32>,
    /// Per device: pixels per point, read once from its AX tree.
    scales: HashMap<DeviceId, f64>,
    /// Per device: input verbs take turns, so two agents' taps and swipes
    /// (or one agent's parallel calls) never interleave into one gesture.
    input: HashMap<DeviceId, Arc<futures::lock::Mutex<()>>>,
    /// Orders the approval writes (see [`LatestWrites`]).
    writes: LatestWrites,
    /// Install questions about real devices (see [`InstallAsk`]).
    installs: Vec<InstallAsk>,
    next_install: u64,
}

/// Approval writes land in the order the user decided them. The writes run
/// off the UI thread, so an Allow followed at once by a Revoke could
/// otherwise reach the database the other way round and re-approve the
/// device at the next launch. Each decision takes a number as it is made;
/// writes run one at a time, and one a newer decision for the same device
/// overtook is skipped (the newer one writes).
#[derive(Clone, Default)]
struct LatestWrites(Arc<WriteOrder>);

#[derive(Default)]
struct WriteOrder {
    /// Per device: the number of the newest decision.
    latest: Mutex<HashMap<String, u64>>,
    /// Held across a check and its write.
    one_at_a_time: Mutex<()>,
}

impl LatestWrites {
    /// Number a decision about `key`.
    fn begin(&self, key: &str) -> u64 {
        let mut latest = self.0.latest.lock().unwrap_or_else(PoisonError::into_inner);
        let n = latest.entry(key.to_owned()).or_default();
        *n += 1;
        *n
    }

    /// Run `write` unless a newer decision about `key` was made since `seq`.
    fn run_if_latest(&self, key: &str, seq: u64, write: impl FnOnce()) -> bool {
        let _one_at_a_time = self.0.one_at_a_time.lock().unwrap_or_else(PoisonError::into_inner);
        if self.0.latest.lock().unwrap_or_else(PoisonError::into_inner).get(key) != Some(&seq) {
            return false;
        }
        write();
        true
    }
}

impl AgentState {
    /// Start from the persisted approvals. A database that cannot be read
    /// grants nothing: every device asks again. A real device's approval
    /// lasts only until OxiMux quits, so a saved one (an older build wrote
    /// it) grants nothing either.
    pub(crate) fn load(approvals: Option<SimApprovalRepo>) -> Self {
        let granted: Vec<SimApproval> = approvals
            .as_ref()
            .and_then(|repo| repo.list().inspect_err(|e| tracing::warn!("simulator approvals unreadable: {e}")).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !DeviceId(a.udid.clone()).is_physical())
            .collect();
        Self {
            consent: Consent::new(granted.iter().map(|a| DeviceId(a.udid.clone()))),
            approvals,
            granted,
            stopped: HashSet::new(),
            active_until: HashMap::new(),
            in_flight: HashMap::new(),
            scales: HashMap::new(),
            input: HashMap::new(),
            writes: LatestWrites::default(),
            installs: Vec::new(),
            next_install: 0,
        }
    }

    /// A verb on `udid` started at `now`. Returns whether the agent just became
    /// active there — decided *before* counting this verb, which would
    /// otherwise make it look active already.
    fn begin(&mut self, udid: &DeviceId, now: Instant) -> bool {
        let newly = !self.is_active(udid, now);
        *self.in_flight.entry(udid.clone()).or_default() += 1;
        self.active_until.insert(udid.clone(), now + AGENT_BADGE);
        newly
    }

    /// A verb on `udid` finished at `now`; the badge runs on from here.
    fn end(&mut self, udid: &DeviceId, now: Instant) {
        if let Some(n) = self.in_flight.get_mut(udid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.in_flight.remove(udid);
            }
        }
        self.active_until.insert(udid.clone(), now + AGENT_BADGE);
    }

    /// The badge's timer fired at `now`: whether the agent just went idle on
    /// `udid` (no verb running and none for [`AGENT_BADGE`]).
    fn settle(&mut self, udid: &DeviceId, now: Instant) -> bool {
        let due = self.active_until.get(udid).is_some_and(|until| *until <= now);
        if due && !self.in_flight.contains_key(udid) {
            self.active_until.remove(udid);
            return true;
        }
        false
    }

    fn is_active(&self, udid: &DeviceId, now: Instant) -> bool {
        self.in_flight.contains_key(udid) || self.active_until.get(udid).is_some_and(|until| *until > now)
    }
}

/// Where a device an agent needs stands after [`SimulatorHub::wake_for_agent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Streaming: input and the AX tree work now.
    Live,
    /// Booting or starting: retry shortly.
    Starting,
    /// Failed; the panel shows why and offers Retry.
    Failed(String),
    /// The user shut it down from the panel; an agent may not boot it again.
    StoppedByUser,
}

impl SimulatorHub {
    /// A control verb wants `udid` for `worktree`: the verdict, and whether
    /// the question should be shown now (new, or left open a while).
    pub fn consent_check(&mut self, udid: &DeviceId, worktree: &Path, device_name: &str, cx: &mut Context<Self>) -> (Verdict, bool) {
        let checked = self.agent.consent.check(udid, &WorktreeKey::from_path(worktree), device_name, Instant::now());
        if checked.1 {
            cx.emit(HubEvent::Consent);
        }
        checked
    }

    /// Where `udid` stands for `worktree` (polling keeps its request alive).
    pub fn consent_state(&mut self, udid: &DeviceId, worktree: &Path) -> State {
        self.agent.consent.state(udid, &WorktreeKey::from_path(worktree), Instant::now())
    }

    /// The question `worktree`'s agent is waiting on about the device
    /// attached there now, and that device's name.
    pub fn consent_request(&self, worktree: &Path) -> Option<(DeviceId, String)> {
        let udid = self.device_for(worktree)?;
        let request = self.agent.consent.pending_for(&WorktreeKey::from_path(worktree), &udid)?;
        Some((request.udid.clone(), request.device_name.clone()))
    }

    /// `worktree` detached or changed device: drop its open questions.
    pub(super) fn forget_consent_requests(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        let key = WorktreeKey::from_path(worktree);
        let asks = self.agent.installs.len();
        self.agent.installs.retain(|a| a.worktree != key);
        if self.agent.consent.forget_worktree(&key) || self.agent.installs.len() != asks {
            cx.emit(HubEvent::Consent);
        }
    }

    /// Whether an agent may attach `udid` itself. A real device only once the
    /// user allowed agents on it this run (they attach it in the panel, an
    /// agent asks, they say yes): an agent must never pick someone's phone
    /// on its own — whichever CLI, old or new, chose it.
    pub(crate) fn agent_may_attach(&self, udid: &DeviceId) -> Result<(), String> {
        if !udid.is_physical() || self.agent.consent.is_approved(udid) {
            return Ok(());
        }
        Err(format!(
            "{udid} is a real device: agents may attach it only after the user attached it in the Mobile Emulator panel and allowed agents on it"
        ))
    }

    /// Ask the user whether an agent may install `app` on the real device
    /// `udid`. Returns the question's number, for [`Self::install_answer`].
    pub(crate) fn ask_install(&mut self, udid: &DeviceId, worktree: &Path, device_name: &str, app: &str, cx: &mut Context<Self>) -> u64 {
        self.agent.next_install += 1;
        let id = self.agent.next_install;
        self.agent.installs.push(InstallAsk {
            id,
            udid: udid.clone(),
            worktree: WorktreeKey::from_path(worktree),
            device_name: device_name.to_owned(),
            app: app.to_owned(),
            asked: Instant::now(),
            answer: None,
        });
        cx.emit(HubEvent::Consent);
        id
    }

    /// The user's answer to install question `id`, once (an answered or
    /// expired question is dropped as it is read).
    pub(crate) fn install_answer(&mut self, id: u64) -> InstallAnswer {
        let Some(at) = self.agent.installs.iter().position(|a| a.id == id) else { return InstallAnswer::Gone };
        let answer = match self.agent.installs[at].answer {
            Some(true) => InstallAnswer::Allowed,
            Some(false) => InstallAnswer::Refused,
            None if self.agent.installs[at].asked.elapsed() >= INSTALL_ASK_TTL => InstallAnswer::Gone,
            None => return InstallAnswer::Pending,
        };
        self.agent.installs.remove(at);
        answer
    }

    /// The install question `worktree`'s agent is waiting on about the device
    /// attached there now: its number, the device's name and the app's.
    pub fn install_request(&self, worktree: &Path) -> Option<(u64, String, String)> {
        let (udid, key) = (self.device_for(worktree)?, WorktreeKey::from_path(worktree));
        let ask = self.agent.installs.iter().find(|a| a.worktree == key && a.udid == udid && a.answer.is_none())?;
        Some((ask.id, ask.device_name.clone(), ask.app.clone()))
    }

    /// The banner's Install / Don't install.
    pub fn answer_install(&mut self, id: u64, allow: bool, cx: &mut Context<Self>) {
        if let Some(ask) = self.agent.installs.iter_mut().find(|a| a.id == id) {
            ask.answer = Some(allow);
            cx.emit(HubEvent::Consent);
        }
    }

    /// The turn-taking lock for input on `udid` (see `AgentState::input`).
    pub(crate) fn input_lock(&mut self, udid: &DeviceId) -> Arc<futures::lock::Mutex<()>> {
        self.agent.input.entry(udid.clone()).or_default().clone()
    }

    /// The user allowed agents to control `udid` (the banner's Allow — the
    /// only writer of an approval).
    pub fn allow_agents(&mut self, udid: &DeviceId, device_name: String, cx: &mut Context<Self>) {
        self.agent.consent.allow(udid);
        self.agent.granted.retain(|a| a.udid != udid.as_str());
        self.agent.granted.push(SimApproval {
            udid: udid.to_string(),
            device_name: device_name.clone(),
            granted_at: chrono::Utc::now().to_rfc3339(),
        });
        self.persist_approval(udid, Some(device_name), cx);
        cx.emit(HubEvent::Consent);
    }

    /// The user refused `udid` (for `consent::DENY_COOLDOWN`).
    pub fn deny_agents(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.agent.consent.deny(udid, Instant::now());
        cx.emit(HubEvent::Consent);
    }

    /// The approved devices, oldest first (the Settings list).
    pub fn approvals(&self) -> &[SimApproval] {
        &self.agent.granted
    }

    /// Settings withdrew an approval: in memory at once, and from the
    /// database (never revoke through the repository alone, or the grant in
    /// memory would outlive it until a restart).
    pub fn revoke_agents(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.agent.consent.revoke(udid);
        self.agent.granted.retain(|a| a.udid != udid.as_str());
        self.persist_approval(udid, None, cx);
        cx.emit(HubEvent::Consent);
    }

    /// Forget the approvals and "stopped by the user" latches of simulators
    /// the device menu's listing no longer has: deleted in Xcode, so nothing
    /// else would ever drop them. Called only after a listing in which
    /// `simctl` answered, which lists every simulator (shut down ones too).
    /// Android is left alone: its listing leaves AVDs out when `-list-avds`
    /// fails, and a phone that is not listed is only unplugged.
    pub(super) fn forget_deleted_simulators(&mut self, cx: &mut Context<Self>) {
        let listed: HashSet<&DeviceId> = self.devices.iter().map(|d| &d.udid).collect();
        let attached = self.registry.attached_devices();
        let gone = |udid: &DeviceId| udid.source() == Source::Simctl && !listed.contains(udid) && !attached.contains(udid);
        let revoked: Vec<DeviceId> =
            self.agent.granted.iter().map(|a| DeviceId(a.udid.clone())).filter(|udid| gone(udid)).collect();
        let latched = self.agent.stopped.len();
        self.agent.stopped.retain(|udid| !gone(udid));
        if self.agent.stopped.len() != latched {
            sim_state_keys::save_stopped(&self.repo, &self.agent.stopped);
        }
        for udid in revoked {
            tracing::info!(%udid, "simulator deleted: its agent approval is dropped");
            self.revoke_agents(&udid, cx);
        }
    }

    /// Save a grant (`Some(name)`) or a revoke of `udid`, off the UI thread and
    /// in decision order. A real device's grant is never saved: it lasts
    /// until OxiMux quits.
    fn persist_approval(&mut self, udid: &DeviceId, grant: Option<String>, cx: &mut Context<Self>) {
        if grant.is_some() && udid.is_physical() {
            return;
        }
        let Some(repo) = self.agent.approvals.clone() else { return };
        let writes = self.agent.writes.clone();
        let seq = writes.begin(udid.as_str());
        let udid = udid.clone();
        cx.background_executor()
            .spawn(async move {
                writes.run_if_latest(udid.as_str(), seq, || {
                    let saved = match &grant {
                        Some(name) => repo.grant(udid.as_str(), name),
                        None => repo.revoke(udid.as_str()),
                    };
                    if let Err(e) = saved {
                        tracing::warn!(%udid, "could not save the simulator approval: {e}");
                    }
                });
            })
            .detach();
    }

    /// Drop consent requests nobody polls any more, and install questions
    /// nobody answered in time — or whose answer no agent came back for (its
    /// CLI gave up) — from the tick.
    pub(super) fn expire_consent(&mut self, cx: &mut Context<Self>) {
        let asks = self.agent.installs.len();
        // An answer gets a little longer than the question: the waiting agent
        // reads it within a quarter second.
        let grace = |a: &InstallAsk| if a.answer.is_some() { Duration::from_secs(15) } else { Duration::ZERO };
        self.agent.installs.retain(|a| a.asked.elapsed() < INSTALL_ASK_TTL + grace(a));
        if self.agent.consent.expire(Instant::now()) || self.agent.installs.len() != asks {
            cx.emit(HubEvent::Consent);
        }
    }

    /// An agent verb on `udid` started. Until it finishes, and for
    /// [`AGENT_BADGE`] after, the badge shows and the agent counts as a
    /// viewer: a device no panel shows streams (a paused helper captures
    /// nothing) and is not parked under it.
    pub fn agent_verb_started(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self.agent.begin(udid, now) {
            let effects = self.registry.set_agent_active(udid, true, now);
            self.run(effects, cx);
            cx.emit(HubEvent::AgentActivity(udid.clone()));
        }
        self.settle_agent_later(udid, cx);
    }

    /// An agent verb on `udid` finished (successfully or not).
    pub fn agent_verb_finished(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.agent.end(udid, Instant::now());
        self.settle_agent_later(udid, cx);
    }

    /// End the agent's turn once it has been idle for [`AGENT_BADGE`] — on a
    /// one-shot timer rather than a later render, which may never come (and a
    /// notify from inside a render is dropped).
    fn settle_agent_later(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AGENT_BADGE + Duration::from_millis(50)).await;
            let _ = this.update(cx, |hub, cx| {
                let now = Instant::now();
                if hub.agent.settle(&udid, now) {
                    let effects = hub.registry.set_agent_active(&udid, false, now);
                    hub.run(effects, cx);
                    cx.emit(HubEvent::AgentActivity(udid));
                }
            });
        })
        .detach();
    }

    /// Whether the badge shows for `udid`.
    pub fn agent_active(&self, udid: &DeviceId) -> bool {
        self.agent.is_active(udid, Instant::now())
    }

    /// `udid`'s pixels per point, once known.
    pub fn scale(&self, udid: &DeviceId) -> Option<f64> {
        self.agent.scales.get(udid).copied()
    }

    pub fn set_scale(&mut self, udid: &DeviceId, scale: f64) {
        self.agent.scales.insert(udid.clone(), scale);
    }

    /// Settings' "Revoke all".
    pub fn revoke_all_agents(&mut self, cx: &mut Context<Self>) {
        let all: Vec<DeviceId> = self.agent.granted.iter().map(|a| DeviceId(a.udid.clone())).collect();
        for udid in &all {
            self.revoke_agents(udid, cx);
        }
    }

    /// The user shut `udid` down from the panel (the confirmed power button).
    /// Agents are refused a wake of it from now on (see [`Wake::StoppedByUser`]).
    /// A real device is never latched: OxiMux never shuts one down, so
    /// nothing would ever lift it.
    pub fn note_stopped_by_user(&mut self, udid: &DeviceId) {
        if !udid.is_physical() && self.agent.stopped.insert(udid.clone()) {
            sim_state_keys::save_stopped(&self.repo, &self.agent.stopped);
        }
    }

    /// The user reconnected or picked `udid`, an attach chose it, or the
    /// watcher saw it boot again: agents may wake it again. Never cleared by
    /// an agent's own wake — its verb may land before the shutdown does.
    pub fn clear_stopped_by_user(&mut self, udid: &DeviceId) {
        if self.agent.stopped.remove(udid) {
            sim_state_keys::save_stopped(&self.repo, &self.agent.stopped);
        }
    }

    /// Bring up the helper for a device an agent needs: a parked, never
    /// started (restored) or disconnected device is restarted, as the user's
    /// Reconnect would. The panel does not have to be open. A device the user
    /// shut down from the panel is not: the power button means "stop".
    pub fn wake_for_agent(&mut self, udid: &DeviceId, cx: &mut Context<Self>) -> Wake {
        // Checked before the phase: in the moments between the confirmed
        // Shut Down and the helper's exit the device still reads Live, and a
        // verb must not slip through that window.
        if self.agent.stopped.contains(udid) {
            return Wake::StoppedByUser;
        }
        match self.registry.phase(udid) {
            Phase::Live { .. } => Wake::Live,
            Phase::Booting { .. } | Phase::Starting { .. } => Wake::Starting,
            Phase::Failed { error } => Wake::Failed(error),
            Phase::Parked | Phase::Idle | Phase::Disconnected { .. } => {
                self.reconnect(udid, cx);
                match self.registry.phase(udid) {
                    Phase::Live { .. } => Wake::Live,
                    Phase::Failed { error } => Wake::Failed(error),
                    _ => Wake::Starting,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::simulator_settings::SimulatorSettings;
    use oximux_simulator::consent::Verdict;

    /// Allow then Revoke, their writes landing in the other order: the
    /// Revoke writes and the stale Allow is skipped, so the database ends
    /// revoked, as the user left it.
    #[test]
    fn a_stale_approval_write_is_skipped() {
        let writes = LatestWrites::default();
        let (allow, revoke) = (writes.begin("U"), writes.begin("U"));
        let saved = std::cell::RefCell::new(Vec::new());
        assert!(writes.run_if_latest("U", revoke, || saved.borrow_mut().push("revoke")));
        assert!(!writes.run_if_latest("U", allow, || saved.borrow_mut().push("allow")));
        assert_eq!(*saved.borrow(), ["revoke"]);
        // Another device's decisions are its own.
        let other = writes.begin("V");
        assert!(writes.run_if_latest("V", other, || saved.borrow_mut().push("V")));
    }

    /// The first verb makes the agent a viewer — decided before the verb is
    /// counted, or it would look active already and never resume the device.
    /// It stays one while any verb runs, however long, and goes idle only
    /// [`AGENT_BADGE`] after the last one ends.
    #[test]
    fn the_first_verb_starts_the_turn_and_the_last_ends_it() {
        let (mut state, u, t) = (AgentState::load(None), DeviceId("U".into()), Instant::now());
        assert!(state.begin(&u, t), "the first verb starts the agent's turn");
        assert!(!state.begin(&u, t), "a second overlapping one does not");
        state.end(&u, t);
        assert!(!state.settle(&u, t + AGENT_BADGE * 5), "a verb is still running");
        state.end(&u, t + AGENT_BADGE * 5);
        assert!(!state.settle(&u, t + AGENT_BADGE * 5 + Duration::from_millis(10)), "not idle long enough");
        assert!(state.settle(&u, t + AGENT_BADGE * 6));
        assert!(state.begin(&u, t + AGENT_BADGE * 7), "and the next verb starts a new turn");
    }

    /// The power button means "stop": once the user shut a device down from
    /// the panel, an agent's verbs are refused instead of booting it again,
    /// however often they ask, until the user (or an attach) lifts it.
    #[gpui::test]
    fn a_device_the_user_shut_down_stays_down_for_agents(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let hub = cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db))
        });
        let udid = DeviceId("U-1".into());
        hub.update(cx, |hub, cx| {
            let key = WorktreeKey::from_path(Path::new("/nonexistent/w"));
            drop(hub.registry.attach(key, udid.clone(), true, Instant::now()));
            drop(hub.registry.device_shutdown(&udid));
            assert!(matches!(hub.registry.phase(&udid), Phase::Disconnected { .. }), "{:?}", hub.registry.phase(&udid));
            hub.note_stopped_by_user(&udid);
            assert_eq!(hub.wake_for_agent(&udid, cx), Wake::StoppedByUser);
            assert_eq!(hub.wake_for_agent(&udid, cx), Wake::StoppedByUser, "asking again does not lift it");
            hub.clear_stopped_by_user(&udid);
            assert!(!hub.agent.stopped.contains(&udid));
        });
    }

    /// The latch is checked before the phase — between the confirmed Shut
    /// Down and the helper's exit the device still reads Live, and here it
    /// reads Starting: refused either way. It is saved, so a relaunch
    /// (terminal agents survive it through the relay) keeps it. A phone is
    /// never latched: nothing would ever lift it.
    #[gpui::test]
    fn the_latch_wins_over_the_phase_and_survives_a_relaunch(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let settings = oximux_storage::SettingsRepo::new(db.clone());
        let hub = cx.update(|cx| super::super::install_for_test(cx, settings.clone(), SimApprovalRepo::new(db.clone())));
        let udid = DeviceId("U-1".into());
        hub.update(cx, |hub, cx| {
            let key = WorktreeKey::from_path(Path::new("/nonexistent/w"));
            drop(hub.registry.attach(key, udid.clone(), true, Instant::now()));
            hub.note_stopped_by_user(&udid);
            assert_eq!(hub.wake_for_agent(&udid, cx), Wake::StoppedByUser, "{:?}", hub.registry.phase(&udid));
            let phone = DeviceId("adb:R58M123".into());
            hub.note_stopped_by_user(&phone);
            assert!(!hub.agent.stopped.contains(&phone), "a phone is never latched");
        });
        assert!(sim_state_keys::load_stopped(&settings).contains(&udid), "saved");
        let relaunched = cx.update(|cx| super::super::install_for_test(cx, settings.clone(), SimApprovalRepo::new(db)));
        relaunched.update(cx, |hub, cx| {
            assert_eq!(hub.wake_for_agent(&udid, cx), Wake::StoppedByUser, "restored");
            hub.clear_stopped_by_user(&udid);
        });
        assert!(sim_state_keys::load_stopped(&settings).is_empty(), "lifting it is saved too");
    }

    /// The latch means "the user stopped it". A shutdown that fails (here:
    /// no Xcode, so no `simctl` at all) stopped nothing: the latch lifts and
    /// the user is told, instead of agents being refused a running device.
    #[gpui::test]
    fn a_failed_shutdown_lifts_the_latch(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let hub = cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db))
        });
        let udid = DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into());
        let notices = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = notices.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(&hub, move |_, ev: &HubEvent, _| {
                if matches!(ev, HubEvent::Notice(..)) {
                    seen.set(seen.get() + 1);
                }
            })
        });
        hub.update(cx, |hub, cx| {
            hub.note_stopped_by_user(&udid);
            hub.shutdown_device(&udid, cx);
        });
        cx.run_until_parked();
        hub.read_with(cx, |hub, _| assert!(!hub.agent.stopped.contains(&udid), "lifted"));
        assert_eq!(notices.get(), 1, "and said so");
    }

    /// Editing `simulator.toml` cannot grant an approval. The file sits where
    /// an agent's shell can write it, so it has no field for approvals, and a
    /// hand-added one is ignored; they are read only from the database, whose
    /// only writer is the banner's Allow.
    #[test]
    fn the_settings_file_cannot_grant_an_approval() {
        let forged = "agent_control = true\napproved = [\"U-1\"]\n\n[approvals]\n\"U-1\" = true\n";
        let settings = SimulatorSettings::from_toml_str(forged).expect("unknown keys are ignored, not fatal");
        assert!(settings.agent_control);

        let db = oximux_storage::open_memory().expect("db");
        // (See the consent module: like any guard against a same-user
        // process this is advisory — what it rules out is the settings file,
        // the one place agents' tools routinely edit.)
        let (udid, worktree) = (DeviceId("U-1".into()), WorktreeKey::from_path(Path::new("/w")));
        let mut state = AgentState::load(Some(SimApprovalRepo::new(db.clone())));
        assert_eq!(state.consent.check(&udid, &worktree, "iPhone", Instant::now()).0, Verdict::Pending);

        SimApprovalRepo::new(db.clone()).grant("U-1", "iPhone").expect("grant");
        let mut state = AgentState::load(Some(SimApprovalRepo::new(db)));
        assert_eq!(state.consent.check(&udid, &worktree, "iPhone", Instant::now()).0, Verdict::Allowed);
    }

    /// Agent access to a real device lasts until OxiMux quits: the Allow
    /// works now but is never saved, and a saved one (an older build wrote
    /// it) grants nothing at the next launch.
    #[gpui::test]
    fn a_real_devices_approval_lasts_until_quit(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let (settings, approvals) = (oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db));
        let hub = cx.update(|cx| super::super::install_for_test(cx, settings.clone(), approvals.clone()));
        let (phone, sim, w) = (DeviceId("adb:R58M123".into()), DeviceId("U-1".into()), Path::new("/w"));
        hub.update(cx, |hub, cx| {
            hub.allow_agents(&phone, "Galaxy".into(), cx);
            hub.allow_agents(&sim, "iPhone".into(), cx);
            assert_eq!(hub.consent_check(&phone, w, "Galaxy", cx).0, Verdict::Allowed, "allowed this run");
        });
        cx.run_until_parked();
        let saved: Vec<String> = approvals.list().expect("list").into_iter().map(|a| a.udid).collect();
        assert_eq!(saved, ["U-1"], "the phone's approval is not saved");
        // An older build saved one; this build reads none of it.
        approvals.grant("adb:R58M123", "Galaxy").expect("grant");
        let mut state = AgentState::load(Some(approvals));
        let key = WorktreeKey::from_path(w);
        assert_eq!(state.consent.check(&phone, &key, "Galaxy", Instant::now()).0, Verdict::Pending);
        assert_eq!(state.consent.check(&sim, &key, "iPhone", Instant::now()).0, Verdict::Allowed);
        assert!(state.granted.iter().all(|a| a.udid != "adb:R58M123"), "nor lists it in Settings");
    }

    /// An agent never attaches someone's phone on its own (an old CLI may
    /// pick one by id): only once the user allowed agents on it this run.
    #[gpui::test]
    fn an_agent_attaches_a_real_device_only_once_allowed(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let hub = cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db))
        });
        hub.update(cx, |hub, cx| {
            let (phone, iphone) = (DeviceId("adb:R58M123".into()), DeviceId("iosdev:00008110-001A2C3E0A88401E".into()));
            assert!(hub.agent_may_attach(&DeviceId("U-1".into())).is_ok() && hub.agent_may_attach(&DeviceId("avd:Pixel".into())).is_ok());
            assert!(hub.agent_may_attach(&phone).is_err());
            assert!(hub.agent_may_attach(&iphone).is_err());
            hub.allow_agents(&phone, "Galaxy".into(), cx);
            assert!(hub.agent_may_attach(&phone).is_ok());
            hub.revoke_agents(&phone, cx);
            assert!(hub.agent_may_attach(&phone).is_err());
        });
    }

    /// An install on a real device is asked every time and shown only where
    /// that worktree's agent asked; the answer is read once, an unanswered
    /// question expires, and a detach drops it.
    #[gpui::test]
    fn an_install_on_a_real_device_waits_for_the_user(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let hub = cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db))
        });
        let (phone, w, other) = (DeviceId("adb:R58M123".into()), Path::new("/nonexistent/w"), Path::new("/nonexistent/o"));
        hub.update(cx, |hub, cx| {
            drop(hub.registry.attach(WorktreeKey::from_path(w), phone.clone(), true, Instant::now()));
            let id = hub.ask_install(&phone, w, "Galaxy", "app-debug.apk", cx);
            assert_eq!(hub.install_answer(id), InstallAnswer::Pending);
            assert_eq!(hub.install_request(w), Some((id, "Galaxy".into(), "app-debug.apk".into())));
            assert_eq!(hub.install_request(other), None, "never over another worktree's panel");
            hub.answer_install(id, true, cx);
            assert_eq!(hub.install_answer(id), InstallAnswer::Allowed);
            assert_eq!(hub.install_answer(id), InstallAnswer::Gone, "read once");

            let id = hub.ask_install(&phone, w, "Galaxy", "app-debug.apk", cx);
            hub.answer_install(id, false, cx);
            assert_eq!(hub.install_answer(id), InstallAnswer::Refused);

            let id = hub.ask_install(&phone, w, "Galaxy", "app-debug.apk", cx);
            hub.agent.installs[0].asked -= INSTALL_ASK_TTL;
            assert_eq!(hub.install_answer(id), InstallAnswer::Gone, "expired: not allowed");

            let id = hub.ask_install(&phone, w, "Galaxy", "app-debug.apk", cx);
            hub.detach(w, cx);
            assert_eq!(hub.install_answer(id), InstallAnswer::Gone, "the worktree moved on");
        });
    }

    /// A simulator deleted in Xcode drops out of `simctl`'s listing: its
    /// approval and latch go with it, in memory and on disk. An emulator (an
    /// AVD `-list-avds` missed) and an attached device are kept. (A phone's
    /// saved approval is not loaded at all: real devices' last until quit.)
    #[gpui::test]
    fn a_deleted_simulator_loses_its_approval_and_latch(cx: &mut gpui::TestAppContext) {
        let db = oximux_storage::open_memory().expect("db");
        let (settings, approvals) = (oximux_storage::SettingsRepo::new(db.clone()), SimApprovalRepo::new(db));
        for udid in ["U-gone", "U-kept", "U-attached", "avd:Pixel", "adb:R58M123"] {
            approvals.grant(udid, "device").expect("grant");
        }
        let hub = cx.update(|cx| super::super::install_for_test(cx, settings.clone(), approvals.clone()));
        let (gone, kept) = (DeviceId("U-gone".into()), DeviceId("U-kept".into()));
        hub.update(cx, |hub, cx| {
            let key = WorktreeKey::from_path(Path::new("/nonexistent/w"));
            drop(hub.registry.attach(key, DeviceId("U-attached".into()), true, Instant::now()));
            hub.note_stopped_by_user(&gone);
            hub.note_stopped_by_user(&kept);
            let stamp = hub.begin_listing(true);
            let listed = oximux_simulator::DeviceInfo {
                udid: kept.clone(),
                name: "iPhone".into(),
                runtime: String::new(),
                os_version: String::new(),
                state: oximux_simulator::DeviceState::Shutdown,
                kind: oximux_simulator::DeviceKind::Phone,
                is_available: true,
                note: None,
            };
            assert!(hub.land_listing(stamp, vec![listed], cx));
            hub.forget_deleted_simulators(cx);
            let left: Vec<&str> = hub.approvals().iter().map(|a| a.udid.as_str()).collect();
            assert_eq!(left.len(), 3, "{left:?}");
            assert!(!left.contains(&"U-gone"));
            assert!(hub.agent.stopped.contains(&kept) && !hub.agent.stopped.contains(&gone));
        });
        cx.run_until_parked();
        assert!(approvals.list().expect("list").iter().all(|a| a.udid != "U-gone"), "revoked on disk");
        assert!(!sim_state_keys::load_stopped(&settings).contains(&gone), "unlatched on disk");
    }
}
