//! The hub's iPhone control (Phase 8): turning a real iPhone's control
//! runner on and off, and keeping it with the phone across video restarts.
//!
//! Control is the user's choice, per phone, with a signing team they pick
//! (both kept in the settings database). Turning it on builds the runner on
//! this Mac if needed — signed with that team, the progress shown in the
//! panel — launches it, and hands the phone's [`DeviceControl`] to its
//! session, so the panel's input and the agent verbs reach the phone. The
//! control outlives the video (a parked stream restarts without it), and
//! ends when the phone is detached everywhere, control is turned off, or
//! OxiMux quits. Off macOS, control stays off.

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_simulator::DeviceId;
#[cfg(target_os = "macos")]
use oximux_simulator::ios_device::{
    DeviceSession,
    control::{ControlEvent, DeviceControl},
    runner_build::{self, BuildRequest, RunnerHome},
    runner_supervisor::{ControlError, RunnerSpec, RunnerSupervisor, Transport},
    usbmux::Usbmux,
};
#[cfg(target_os = "macos")]
use oximux_simulator::runner::Runner;

use super::SimulatorHub;
#[cfg(target_os = "macos")]
use super::{HubEvent, NoticeKind, simulator_dir};
use crate::app_settings::sim_state_keys;

/// The home screen's bundle id (what "no target" means to the runner).
const SPRINGBOARD_ID: &str = "com.apple.springboard";

/// What the panel shows of a phone's control.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ControlState {
    #[default]
    Off,
    /// Building the runner: xcodebuild's latest line.
    Building(String),
    /// Launching the runner on the phone.
    Starting,
    /// Input reaches the phone; `busy` while commands are queued.
    On { busy: bool },
    Failed(String),
}

impl ControlState {
    /// Being turned on (a click now would only start it twice).
    pub fn in_progress(&self) -> bool {
        matches!(self, Self::Building(_) | Self::Starting)
    }
}

/// A signing team, as the team picker shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamChoice {
    pub id: String,
    pub name: String,
    pub personal: bool,
}

/// The hub's control bookkeeping.
#[derive(Default)]
pub(crate) struct Controls {
    states: HashMap<DeviceId, ControlState>,
    #[cfg(target_os = "macos")]
    running: HashMap<DeviceId, Arc<DeviceControl>>,
    /// Gives a start in progress up (turned off, detached, quit).
    #[cfg(target_os = "macos")]
    starts: HashMap<DeviceId, Arc<AtomicBool>>,
    /// Each controlled phone's current video session, for its control's
    /// view of the screen's shape (the session restarts; the control stays).
    #[cfg(target_os = "macos")]
    screens: HashMap<DeviceId, Screen>,
    /// Phones already rebuilt once for an expired profile this run (a second
    /// expiry is the user's to look at).
    #[cfg(target_os = "macos")]
    rebuilt: std::collections::HashSet<DeviceId>,
    /// This Mac's signing teams, once listed (`Err`: why not).
    teams: Option<Result<Vec<TeamChoice>, String>>,
    teams_loading: bool,
    /// Each phone's apps, for the target picker (listed when control comes
    /// on, and again after a failure or an install); `Err`: why not.
    apps: HashMap<DeviceId, Result<Vec<TargetApp>, String>>,
    apps_loading: std::collections::HashSet<DeviceId>,
    /// The app each phone's gestures and typing address (absent: the home
    /// screen), kept across a relaunch of its control.
    targets: HashMap<DeviceId, TargetApp>,
}

/// An app control can address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetApp {
    pub bundle_id: String,
    pub name: String,
}

/// Build progress reaches the panel at most this often.
#[cfg(target_os = "macos")]
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// A phone's current video session, shared with its control.
#[cfg(target_os = "macos")]
type Screen = Arc<std::sync::Mutex<Option<DeviceSession>>>;

impl SimulatorHub {
    /// What `udid` can do now: its session's view (a real iPhone takes
    /// input while its control is on), else its class default.
    /// A controlled iPhone keeps its caps while its video is parked: the
    /// runner is up, and an agent's verb wakes the video.
    pub fn caps(&self, udid: &DeviceId) -> oximux_simulator::caps::DeviceCaps {
        if self.controlled(udid) {
            return oximux_simulator::caps::DeviceCaps::for_session(udid, true);
        }
        self.session(udid).map_or_else(|| oximux_simulator::caps::DeviceCaps::for_id(udid), |s| s.caps(udid))
    }

    /// Whether `udid`'s control is on (its runner up or relaunchable).
    fn controlled(&self, udid: &DeviceId) -> bool {
        #[cfg(target_os = "macos")]
        return self.controls.running.contains_key(udid);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = udid;
            false
        }
    }

    pub fn control_state(&self, udid: &DeviceId) -> ControlState {
        self.controls.states.get(udid).cloned().unwrap_or_default()
    }

    /// The team the user chose for the runner, if any.
    pub fn chosen_team(&self) -> Option<String> {
        sim_state_keys::load_iphone_team(&self.repo)
    }

    /// The app `udid`'s control addresses (`None`: the home screen).
    pub fn control_target(&self, udid: &DeviceId) -> Option<TargetApp> {
        self.controls.targets.get(udid).cloned()
    }

    /// Address `app` (`None`: the home screen and system dialogs).
    pub fn set_control_target(&mut self, udid: &DeviceId, app: Option<TargetApp>, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(control) = self.controls.running.get(udid) {
            let bundle = app.as_ref().map_or(SPRINGBOARD_ID, |a| a.bundle_id.as_str());
            control.set_target(bundle);
        }
        match app {
            Some(app) => self.controls.targets.insert(udid.clone(), app),
            None => self.controls.targets.remove(udid),
        };
        cx.emit(super::HubEvent::Changed(udid.clone()));
    }

    /// An agent launched `bundle_id` on the phone: it is what typing and the
    /// accessibility tree should address now.
    pub(crate) fn note_launched(&mut self, udid: &DeviceId, bundle_id: &str, cx: &mut Context<Self>) {
        if bundle_id == SPRINGBOARD_ID {
            return self.set_control_target(udid, None, cx);
        }
        let name = self
            .controls
            .apps
            .get(udid)
            .and_then(|apps| apps.as_ref().ok()?.iter().find(|a| a.bundle_id == bundle_id))
            .map_or_else(|| bundle_id.to_owned(), |a| a.name.clone());
        self.set_control_target(udid, Some(TargetApp { bundle_id: bundle_id.to_owned(), name }), cx);
    }

    /// The phone's apps, for the picker (`None` while they are listed; a
    /// failed listing is said, and tried again on the next ask).
    pub fn phone_apps(&mut self, udid: &DeviceId, cx: &mut Context<Self>) -> Option<Result<Vec<TargetApp>, String>> {
        let known = self.controls.apps.get(udid).cloned();
        if !matches!(known, Some(Ok(_))) && self.controls.apps_loading.insert(udid.clone()) {
            let (runner, target) = (self.runner.clone(), udid.clone());
            cx.spawn(async move |this, cx| {
                let listed = cx
                    .background_executor()
                    .spawn({
                        let target = target.clone();
                        async move { oximux_simulator::devicectl::apps(runner.as_ref(), &target, std::time::Duration::from_secs(20)) }
                    })
                    .await;
                let _ = this.update(cx, |hub, cx| {
                    // Forgotten meanwhile (an install): this list is stale.
                    if !hub.controls.apps_loading.remove(&target) {
                        return;
                    }
                    let apps = listed
                        .map(|apps| apps.into_iter().map(|a| TargetApp { bundle_id: a.bundle_id, name: a.name }).collect())
                        .map_err(|e| e.to_string());
                    hub.controls.apps.insert(target.clone(), apps);
                    cx.emit(super::HubEvent::Changed(target));
                });
            })
            .detach();
        }
        known
    }

    /// List the phone's apps again (one was installed meanwhile).
    pub fn forget_phone_apps(&mut self, udid: &DeviceId) {
        self.controls.apps.remove(udid);
        self.controls.apps_loading.remove(udid);
    }

    /// This Mac's signing teams (listed on first ask; `None` meanwhile).
    pub fn signing_teams(&mut self, cx: &mut Context<Self>) -> Option<Result<Vec<TeamChoice>, String>> {
        if self.controls.teams.is_none() && !self.controls.teams_loading {
            self.load_signing_teams(cx);
        }
        self.controls.teams.clone()
    }

    /// List the signing teams again (after the user added an account).
    pub fn load_signing_teams(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            self.controls.teams_loading = true;
            let runner = self.runner.clone();
            cx.spawn(async move |this, cx| {
                let teams = cx
                    .background_executor()
                    .spawn(async move {
                        oximux_simulator::ios_device::team::teams(runner.as_ref(), Duration::from_secs(20))
                            .map(|teams| teams.into_iter().map(|t| TeamChoice { id: t.id, name: t.name, personal: t.personal }).collect())
                            .map_err(|e| e.to_string())
                    })
                    .await;
                let _ = this.update(cx, |hub, cx| {
                    hub.controls.teams_loading = false;
                    hub.controls.teams = Some(teams);
                    cx.notify();
                });
            })
            .detach();
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.controls.teams = Some(Err("iPhone control needs macOS".into()));
            cx.notify();
        }
    }

    /// Turn control of `udid` on, signed with `team` (the user's choice,
    /// remembered for the next phone).
    pub fn enable_control(&mut self, udid: &DeviceId, team: &str, cx: &mut Context<Self>) {
        if !oximux_simulator::ios_device::is_team_id(team) {
            return;
        }
        #[cfg(target_os = "macos")]
        self.controls.rebuilt.remove(udid);
        // A failed attempt is let go: this is a fresh one.
        if matches!(self.control_state(udid), ControlState::Failed(_)) {
            self.stop_control(udid, cx);
        }
        sim_state_keys::save_iphone_team(&self.repo, team);
        let mut controlled = sim_state_keys::load_iphone_controlled(&self.repo);
        if controlled.insert(udid.clone()) {
            sim_state_keys::save_iphone_controlled(&self.repo, &controlled);
        }
        self.start_control(udid, cx);
    }

    /// Turn control of `udid` off (and keep it off next time).
    pub fn disable_control(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let mut controlled = sim_state_keys::load_iphone_controlled(&self.repo);
        if controlled.remove(udid) {
            sim_state_keys::save_iphone_controlled(&self.repo, &controlled);
        }
        self.stop_control(udid, cx);
    }

    /// A phone's video session (re)started: hand it the phone's control, or
    /// start control if the user left it on.
    pub(super) fn control_session_started(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            let Some(session) = self.session(udid).and_then(|s| s.ios_device().cloned()) else { return };
            if let Some(control) = self.controls.running.get(udid) {
                session.set_control(Some(control.clone()));
                if let Some(screen) = self.controls.screens.get(udid) {
                    *screen.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
                }
                return;
            }
            let wanted = sim_state_keys::load_iphone_controlled(&self.repo).contains(udid);
            // Only from Off: a failure waits for the user's Retry, not every
            // unpark.
            if wanted && self.chosen_team().is_some() && self.control_state(udid) == ControlState::Off {
                self.start_control(udid, cx);
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (udid, cx);
    }

    /// Control ends with a phone's last attachment.
    pub(super) fn stop_unattached_controls(&mut self, cx: &mut Context<Self>) {
        let attached = self.registry.attached_devices();
        let orphans: Vec<DeviceId> = self.controls.states.keys().filter(|u| !attached.contains(u)).cloned().collect();
        for udid in orphans {
            self.stop_control(&udid, cx);
        }
    }

    /// Every phone's control, on quit: each runner's group is sent `SIGTERM`
    /// at once (the child ledger catches whatever outlives the quit).
    pub(super) fn stop_controls_blocking(&mut self) {
        #[cfg(target_os = "macos")]
        {
            for (_, cancel) in self.controls.starts.drain() {
                cancel.store(true, Ordering::Release);
            }
            for (_, control) in self.controls.running.drain() {
                control.abort();
            }
        }
        self.controls.states.clear();
    }

    fn set_control_state(&mut self, udid: &DeviceId, state: ControlState, cx: &mut Context<Self>) {
        if state == ControlState::Off {
            self.controls.states.remove(udid);
        } else {
            self.controls.states.insert(udid.clone(), state);
        }
        cx.emit(super::HubEvent::Changed(udid.clone()));
    }

    fn stop_control(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        {
            if let Some(cancel) = self.controls.starts.remove(udid) {
                cancel.store(true, Ordering::Release);
            }
            if let Some(session) = self.session(udid).and_then(|s| s.ios_device().cloned()) {
                session.set_control(None);
            }
            self.controls.screens.remove(udid);
            if let Some(control) = self.controls.running.remove(udid) {
                cx.background_executor().spawn(async move { control.stop() }).detach();
            }
        }
        if self.controls.states.contains_key(udid) {
            self.set_control_state(udid, ControlState::Off, cx);
        }
    }

    #[cfg(target_os = "macos")]
    fn start_control(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if self.controls.running.contains_key(udid) || self.control_state(udid).in_progress() {
            return;
        }
        let Some(team) = self.chosen_team() else { return };
        let Some(hardware) = oximux_simulator::devicectl::hardware_udid(udid).map(str::to_owned) else { return };
        let cancel = Arc::new(AtomicBool::new(false));
        self.controls.starts.insert(udid.clone(), cancel.clone());
        self.set_control_state(udid, ControlState::Building("Preparing the control runner…".into()), cx);
        let (progress_tx, mut progress_rx) = futures::channel::mpsc::unbounded::<ControlState>();
        let (runner, ledger) = (self.runner.clone(), self.ledger.clone());
        let target = udid.clone();
        // Progress lines, a few a second.
        cx.spawn({
            let udid = udid.clone();
            async move |this, cx| {
                use futures::StreamExt as _;
                while let Some(state) = progress_rx.next().await {
                    let still = this
                        .update(cx, |hub, cx| {
                            if hub.control_state(&udid).in_progress() {
                                hub.set_control_state(&udid, state, cx);
                            }
                        })
                        .is_ok();
                    if !still {
                        return;
                    }
                }
            }
        })
        .detach();
        let stop = cancel.clone();
        cx.spawn(async move |this, cx| {
            let started = cx
                .background_executor()
                .spawn(async move { launch(runner.as_ref(), ledger, &hardware, &team, &stop, &progress_tx) })
                .await;
            let _ = this.update(cx, |hub, cx| {
                let current = hub.controls.starts.get(&target).is_some_and(|c| Arc::ptr_eq(c, &cancel));
                if current {
                    hub.controls.starts.remove(&target);
                }
                match started {
                    Ok(supervisor) if current => hub.control_ready(&target, supervisor, cx),
                    // Turned off (or superseded) meanwhile.
                    Ok(supervisor) => {
                        cx.background_executor().spawn(async move { supervisor.stop() }).detach();
                    }
                    Err(ControlError::Cancelled) => {}
                    Err(e) if current => hub.set_control_state(&target, ControlState::Failed(e.to_string()), cx),
                    Err(_) => {}
                }
            });
        })
        .detach();
    }

    #[cfg(not(target_os = "macos"))]
    fn start_control(&mut self, _udid: &DeviceId, _cx: &mut Context<Self>) {}

    /// The runner is up: the phone's control, handed to its session.
    #[cfg(target_os = "macos")]
    fn control_ready(&mut self, udid: &DeviceId, supervisor: Arc<RunnerSupervisor>, cx: &mut Context<Self>) {
        // The captured screen's shape, from whichever session is current.
        let session = self.session(udid).and_then(|s| s.ios_device().cloned());
        let screen: Screen = Arc::new(std::sync::Mutex::new(session.clone()));
        let shape = screen.clone();
        let frame_of = move || shape.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|s| s.video().framebuffer_size());
        let (events_tx, events_rx) = std::sync::mpsc::channel::<ControlEvent>();
        let control = Arc::new(DeviceControl::new(supervisor, frame_of, events_tx));
        if let Some(app) = self.controls.targets.get(udid) {
            control.set_target(&app.bundle_id);
        }
        if let Some(session) = session {
            session.set_control(Some(control.clone()));
        }
        self.controls.screens.insert(udid.clone(), screen);
        self.controls.running.insert(udid.clone(), control);
        self.set_control_state(udid, ControlState::On { busy: false }, cx);
        self.forward_control_events(udid.clone(), events_rx, cx);
        // Listed now, so the target picker has them when it opens.
        let _ = self.phone_apps(udid, cx);
    }

    /// The control's events, to the panel: failures and hints as notices,
    /// the busy flag into the state.
    #[cfg(target_os = "macos")]
    fn forward_control_events(&mut self, udid: DeviceId, events: std::sync::mpsc::Receiver<ControlEvent>, cx: &mut Context<Self>) {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<ControlEvent>();
        // Ends when the control (its sender) is gone.
        std::thread::Builder::new()
            .name("iphone-control-events".into())
            .spawn(move || {
                for event in events {
                    if tx.unbounded_send(event).is_err() {
                        return;
                    }
                }
            })
            .ok();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(event) = rx.next().await {
                let alive = this
                    .update(cx, |hub, cx| match event {
                        ControlEvent::Busy(busy) => {
                            if matches!(hub.control_state(&udid), ControlState::On { .. }) {
                                hub.set_control_state(&udid, ControlState::On { busy }, cx);
                            }
                        }
                        ControlEvent::Error(message) => cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Error, message)),
                        ControlEvent::Failed { message, expired } => hub.control_failed(&udid, message, expired, cx),
                        ControlEvent::Hint(hint) => cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Info, hint.to_owned())),
                        ControlEvent::Reactivated(app) if app != SPRINGBOARD_ID => {
                            cx.emit(HubEvent::Notice(udid.clone(), NoticeKind::Success, format!("Brought {app} to the front")));
                        }
                        ControlEvent::Reactivated(_) => {}
                        ControlEvent::TargetReset => {
                            if hub.controls.targets.remove(&udid).is_some() {
                                cx.emit(HubEvent::Changed(udid.clone()));
                            }
                        }
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
        })
        .detach();
    }
}

impl SimulatorHub {
    /// The runner is unusable: control stops and the row shows why, until
    /// the user retries — except an expired profile, rebuilt once by itself.
    #[cfg(target_os = "macos")]
    fn control_failed(&mut self, udid: &DeviceId, message: String, expired: bool, cx: &mut Context<Self>) {
        if !self.controls.running.contains_key(udid) {
            return;
        }
        self.stop_control(udid, cx);
        if expired && self.controls.rebuilt.insert(udid.clone()) {
            let home = RunnerHome::new(simulator_dir().join("ios-runner"));
            if let Some(hardware) = oximux_simulator::devicectl::hardware_udid(udid) {
                runner_build::invalidate(&home, hardware);
            }
            self.start_control(udid, cx);
            return;
        }
        self.set_control_state(udid, ControlState::Failed(message), cx);
    }
}

/// Builds the runner if needed and launches it (background executor).
#[cfg(target_os = "macos")]
fn launch(
    runner: &dyn Runner,
    ledger: Option<Arc<oximux_simulator::child_ledger::Ledger>>,
    udid: &str,
    team: &str,
    cancel: &AtomicBool,
    progress: &futures::channel::mpsc::UnboundedSender<ControlState>,
) -> Result<Arc<RunnerSupervisor>, ControlError> {
    use oximux_simulator::availability::{self, HelperStatus};
    let failed = |what: &str, e: &dyn std::fmt::Display| ControlError::LaunchFailed(format!("{what}: {e}"));
    let tarball = match availability::default_runner_tarball(&runner_build::tarball_name()) {
        HelperStatus::Found(path) => path,
        HelperStatus::Missing(why) => return Err(ControlError::LaunchFailed(why)),
    };
    let found = runner.run("xcrun", &["--find", "xcodebuild"], None, Duration::from_secs(20)).map_err(|e| failed("Xcode", &e))?;
    let xcodebuild = std::path::PathBuf::from(found.into_success("xcrun").map_err(|e| failed("Xcode", &e))?.stdout_str().trim());
    let version = runner
        .run(&xcodebuild.display().to_string(), &["-version"], None, Duration::from_secs(30))
        .map_err(|e| failed("Xcode", &e))?
        .stdout_str();
    let home = RunnerHome::new(simulator_dir().join("ios-runner"));
    let request = BuildRequest {
        home: &home,
        tarball: &tarball,
        team,
        udid,
        xcodebuild: &xcodebuild,
        xcode_version: &version,
        ledger: ledger.as_deref(),
        cancel,
    };
    let mut last = Instant::now() - PROGRESS_EVERY;
    let mut report = |line: &str| {
        let line = line.trim();
        if !line.is_empty() && last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            let _ = progress.unbounded_send(ControlState::Building(line.chars().take(160).collect()));
        }
    };
    let build = |report: &mut dyn FnMut(&str)| {
        runner_build::ensure(runner, &request, report).map_err(|e| match e {
            oximux_simulator::SimError::Cancelled => ControlError::Cancelled,
            other => ControlError::LaunchFailed(other.to_string()),
        })
    };
    let built = build(&mut report)?;
    let spec = |built: &runner_build::Built| RunnerSpec {
        udid: udid.to_owned(),
        xcodebuild: xcodebuild.clone(),
        xctestrun: built.xctestrun.clone(),
        derived: home.derived(udid),
        ledger: ledger.clone(),
        transport: Transport::Usbmux(Usbmux::system()),
    };
    if cancel.load(Ordering::Acquire) {
        return Err(ControlError::Cancelled);
    }
    let _ = progress.unbounded_send(ControlState::Starting);
    let supervisor = Arc::new(RunnerSupervisor::new(spec(&built)));
    match supervisor.start_unless(cancel) {
        Ok(()) => Ok(supervisor),
        // A profile that ran out early (revoked, or the clock moved): one
        // rebuild.
        Err(ControlError::ProfileExpired) => {
            runner_build::invalidate(&home, udid);
            let built = build(&mut report)?;
            let supervisor = Arc::new(RunnerSupervisor::new(spec(&built)));
            supervisor.start_unless(cancel).map(|()| supervisor)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(cx: &mut gpui::TestAppContext) -> gpui::Entity<SimulatorHub> {
        let db = oximux_storage::open_memory().expect("db");
        cx.update(|cx| {
            super::super::install_for_test(cx, oximux_storage::SettingsRepo::new(db.clone()), oximux_storage::SimApprovalRepo::new(db))
        })
    }

    /// What an agent launches becomes the target, named as the phone lists
    /// it; the home screen is the target again once picked.
    #[gpui::test]
    fn a_launched_app_becomes_the_target(cx: &mut gpui::TestAppContext) {
        let hub = hub(cx);
        let phone = DeviceId("iosdev:00008130-0001".into());
        hub.update(cx, |hub, cx| {
            assert_eq!(hub.control_target(&phone), None, "the home screen by default");
            hub.controls.apps.insert(phone.clone(), Ok(vec![TargetApp { bundle_id: "com.example.notes".into(), name: "Notes+".into() }]));
            hub.note_launched(&phone, "com.example.notes", cx);
            assert_eq!(hub.control_target(&phone).map(|a| a.name), Some("Notes+".into()));
            // Not listed (yet): its id stands in for its name.
            hub.note_launched(&phone, "com.example.fresh", cx);
            assert_eq!(hub.control_target(&phone).map(|a| a.name), Some("com.example.fresh".into()));
            hub.set_control_target(&phone, None, cx);
            assert_eq!(hub.control_target(&phone), None);
            // The home screen launched by id is the home screen.
            hub.note_launched(&phone, "com.example.notes", cx);
            hub.note_launched(&phone, "com.apple.springboard", cx);
            assert_eq!(hub.control_target(&phone), None);
        });
    }

    /// Control is the user's choice per phone, with a team id checked first;
    /// turning it off is remembered.
    #[gpui::test]
    fn turning_control_on_and_off_is_remembered(cx: &mut gpui::TestAppContext) {
        let hub = hub(cx);
        // Not a hardware UDID: nothing is built or launched by the test.
        let phone = DeviceId("iosdev:not-a-udid".into());
        hub.update(cx, |hub, cx| {
            hub.enable_control(&phone, "not a team", cx);
            assert_eq!(hub.chosen_team(), None, "a malformed team id is never saved");
            assert!(!sim_state_keys::load_iphone_controlled(&hub.repo).contains(&phone));
            hub.enable_control(&phone, "TEAM000001", cx);
            assert_eq!(hub.chosen_team().as_deref(), Some("TEAM000001"));
            assert!(sim_state_keys::load_iphone_controlled(&hub.repo).contains(&phone));
            hub.disable_control(&phone, cx);
            assert!(!sim_state_keys::load_iphone_controlled(&hub.repo).contains(&phone));
            assert_eq!(hub.control_state(&phone), ControlState::Off);
        });
    }
}
