//! The hub's side of agent control (`oximux sim …`): per-device consent, the
//! "Agent is using this device" badge, the device's pixel scale, and waking a
//! device an agent needs.
//!
//! Ownership, stated once because P7's recording bugs came from mixing them:
//! **consent and the badge belong to the device** (an approval covers every
//! worktree; the screen that moves is the device's), while **a consent request
//! belongs to the worktree that asked** (it shows where that worktree's panel
//! is, and nowhere else).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_simulator::DeviceId;
use oximux_simulator::consent::{Consent, State, Verdict};
use oximux_simulator::registry::{Phase, WorktreeKey};
use oximux_storage::SimApprovalRepo;

use super::{HubEvent, SimulatorHub};

/// How long the badge stays after an agent's last verb.
pub(crate) const AGENT_BADGE: Duration = Duration::from_secs(2);

/// Agent-control state the hub keeps.
pub(crate) struct AgentState {
    consent: Consent,
    /// Where approvals persist. `None` in tests; grants then live in memory.
    approvals: Option<SimApprovalRepo>,
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
}

impl AgentState {
    /// Start from the persisted approvals. A database that cannot be read
    /// grants nothing: every device asks again.
    pub(crate) fn load(approvals: Option<SimApprovalRepo>) -> Self {
        let approved = approvals
            .as_ref()
            .and_then(|repo| repo.list().inspect_err(|e| tracing::warn!("simulator approvals unreadable: {e}")).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|a| DeviceId(a.udid));
        Self {
            consent: Consent::new(approved),
            approvals,
            active_until: HashMap::new(),
            in_flight: HashMap::new(),
            scales: HashMap::new(),
            input: HashMap::new(),
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
        if self.agent.consent.forget_worktree(&WorktreeKey::from_path(worktree)) {
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
        if let Some(repo) = self.agent.approvals.clone() {
            let udid = udid.clone();
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = repo.grant(udid.as_str(), &device_name) {
                        tracing::warn!(%udid, "could not save the simulator approval: {e}");
                    }
                })
                .detach();
        }
        cx.emit(HubEvent::Consent);
    }

    /// The user refused `udid` (for `consent::DENY_COOLDOWN`).
    pub fn deny_agents(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.agent.consent.deny(udid, Instant::now());
        cx.emit(HubEvent::Consent);
    }

    /// Settings withdrew an approval.
    pub fn revoke_agents(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        self.agent.consent.revoke(udid);
        if let Some(repo) = self.agent.approvals.clone() {
            let udid = udid.clone();
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = repo.revoke(udid.as_str()) {
                        tracing::warn!(%udid, "could not revoke the simulator approval: {e}");
                    }
                })
                .detach();
        }
        cx.emit(HubEvent::Consent);
    }

    /// Drop consent requests nobody polls any more (from the tick).
    pub(super) fn expire_consent(&mut self, cx: &mut Context<Self>) {
        if self.agent.consent.expire(Instant::now()) {
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

    /// Bring up the helper for a device an agent needs: a parked, never
    /// started (restored) or disconnected device is restarted, as the user's
    /// Reconnect would. The panel does not have to be open.
    pub fn wake_for_agent(&mut self, udid: &DeviceId, cx: &mut Context<Self>) -> Wake {
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
}
