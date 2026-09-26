//! Which worktree uses which simulator, and what that obliges us to do.
//!
//! [`Registry`] is pure transition logic. Every method updates state and
//! returns the [`Effect`]s the caller must carry out — boot a device, start or
//! stop a helper session, pause, shut a device down — on a background
//! executor, feeding each result back through the matching method. Nothing
//! here spawns, blocks or reads a clock, so every rule below is unit-tested
//! without a simulator.
//!
//! Rules (Claude Desktop's, plus our review's):
//! - **One device per worktree**; several worktrees may share a device and
//!   then share its one helper session.
//! - **A device the user booted is never shut down.** Only devices *we*
//!   booted (`owned`) are, 10 minutes after their last detach, or at quit.
//! - **Stale completions lose.** Every boot and session start carries a
//!   generation; a result for any other generation is discarded, and a late
//!   session is stopped rather than installed.
//! - **Visibility is refcounted per device**: the helper is paused only when
//!   no attached worktree shows it, and a device hidden for [`PARK_AFTER`] is
//!   **parked** — its helper stopped, its attachments kept — until a viewer
//!   shows it again.
//!
//! The session type is generic (`S`) so tests use a plain token; the app uses
//! `StreamSession`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{DeviceId, DeviceInfo, DeviceKind, DeviceState};

/// An owned device is shut down this long after its last detach.
pub const IDLE_SHUTDOWN: Duration = Duration::from_secs(10 * 60);

/// A device no viewer has shown for this long has its helper stopped (a
/// paused helper still holds the capture session and its memory).
pub const PARK_AFTER: Duration = Duration::from_secs(60);

/// A worktree, by canonical path, so `/tmp/x` and `/private/tmp/x` (or a
/// symlinked checkout) name the same key. The CLI and the desktop both build
/// it through [`WorktreeKey::from_path`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorktreeKey(PathBuf);

impl WorktreeKey {
    /// Canonicalize `path`; a path that no longer exists (a deleted
    /// worktree) falls back to its lexical absolute form, so its attachment
    /// can still be looked up and expired.
    pub fn from_path(path: &Path) -> Self {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| {
            std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
        });
        Self(canonical)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Monotonic id of one boot or session attempt.
pub type Generation = u64;

/// A device's lifecycle, as the panel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing running for it.
    Idle,
    /// `simctl boot` in flight (cancellable).
    Booting { generation: Generation },
    /// Helper spawning / handshaking.
    Starting { generation: Generation },
    /// Streaming.
    Live { generation: Generation },
    /// Booted and attached, but hidden for [`PARK_AFTER`]: the helper was
    /// stopped. Showing it again starts a fresh one.
    Parked,
    /// Was live; the helper exited or the device shut down. Not frozen on
    /// "Streaming": the panel offers Reconnect.
    Disconnected { reason: String },
    /// A boot or start failed.
    Failed { error: String },
}

/// Work for the caller, run off the UI thread.
#[derive(Debug)]
pub enum Effect<S> {
    /// `simctl boot` + wait; report via [`Registry::boot_finished`]. Check
    /// `cancel` while waiting.
    Boot { udid: DeviceId, generation: Generation, cancel: Arc<AtomicBool> },
    /// Spawn the helper; report via [`Registry::session_started`].
    StartSession { udid: DeviceId, generation: Generation },
    /// Shut this session down (explicitly — clones may still be held).
    StopSession { udid: DeviceId, session: S },
    Pause(S),
    Resume(S),
    /// `simctl shutdown` a device we booted.
    ShutdownDevice { udid: DeviceId },
    /// Attachments or owned boots changed: persist [`Registry::snapshot`].
    Persist,
}

/// How a [`Effect::Boot`] ended. Ownership follows it even when the result
/// is stale: a device that was already booted, or whose boot failed, was
/// never ours to shut down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootResult {
    /// We booted it.
    Booted,
    /// Someone else had already booted it (a stale "not booted" reading, or a
    /// race with Xcode): not ours.
    AlreadyBooted,
    /// Abandoned by our own cancel; the boot may still complete, so it stays
    /// ours and the idle rule covers it.
    Cancelled,
    Failed(String),
}

/// What survives a restart: intent, never processes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub attachments: Vec<(WorktreeKey, DeviceId)>,
    /// Devices we booted, so the 10-minute rule and quit shutdown still
    /// apply after a crash and relaunch.
    pub owned_boots: Vec<DeviceId>,
}

#[derive(Debug)]
struct Device<S> {
    phase: Phase,
    session: Option<S>,
    /// Cancel flag of the boot in flight, if any.
    boot_cancel: Option<Arc<AtomicBool>>,
    /// We booted it, so we shut it down (never a user-booted device).
    owned: bool,
    attached: BTreeSet<WorktreeKey>,
    visible: BTreeSet<WorktreeKey>,
    /// Last detach of an owned device, for the idle shutdown.
    idle_since: Option<Instant>,
    /// Automatic restarts since the session last went live.
    restarts: u32,
    /// Whether the helper is currently paused (all viewers hidden).
    paused: bool,
    /// When the helper was paused, for parking; filled by the next
    /// [`Registry::tick`] when the pause had no clock at hand.
    hidden_since: Option<Instant>,
    /// An agent is driving it: it counts as a viewer, so the helper runs
    /// (a paused helper captures nothing, not even the screen's size).
    agent: bool,
}

impl<S> Device<S> {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            session: None,
            boot_cancel: None,
            owned: false,
            attached: BTreeSet::new(),
            visible: BTreeSet::new(),
            idle_since: None,
            restarts: 0,
            paused: false,
            hidden_since: None,
            agent: false,
        }
    }
}

/// The app-wide registry. See the module docs.
#[derive(Debug)]
pub struct Registry<S> {
    attachments: HashMap<WorktreeKey, DeviceId>,
    devices: HashMap<DeviceId, Device<S>>,
    next_generation: Generation,
    /// Worktrees whose panel is visible, attached or not: a panel shown
    /// before its attach lands must not start its helper paused.
    visible: BTreeSet<WorktreeKey>,
}

impl<S> Default for Registry<S> {
    fn default() -> Self {
        Self { attachments: HashMap::new(), devices: HashMap::new(), next_generation: 1, visible: BTreeSet::new() }
    }
}

impl<S: Clone> Registry<S> {
    /// Rebuild from a persisted snapshot. Only intent comes back: attachments
    /// re-connect when the panel first shows them, and owned devices resume
    /// their idle clock from `now`.
    pub fn restore(snapshot: Snapshot, now: Instant) -> Self {
        let mut reg = Self::default();
        for (worktree, udid) in snapshot.attachments {
            reg.device_mut(&udid).attached.insert(worktree.clone());
            reg.attachments.insert(worktree, udid);
        }
        for udid in snapshot.owned_boots {
            let device = reg.device_mut(&udid);
            device.owned = true;
            if device.attached.is_empty() {
                device.idle_since = Some(now);
            }
        }
        reg
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut attachments: Vec<_> = self.attachments.iter().map(|(w, d)| (w.clone(), d.clone())).collect();
        attachments.sort();
        let mut owned_boots: Vec<_> = self.devices.iter().filter(|(_, d)| d.owned).map(|(u, _)| u.clone()).collect();
        owned_boots.sort();
        Snapshot { attachments, owned_boots }
    }

    pub fn device_for(&self, worktree: &WorktreeKey) -> Option<&DeviceId> {
        self.attachments.get(worktree)
    }

    pub fn phase(&self, udid: &DeviceId) -> Phase {
        self.devices.get(udid).map(|d| d.phase.clone()).unwrap_or(Phase::Idle)
    }

    pub fn session(&self, udid: &DeviceId) -> Option<&S> {
        self.devices.get(udid).and_then(|d| d.session.as_ref())
    }

    /// Changes whenever a boot or session is started or finishes. A caller
    /// holding a device list read before and after a change must not apply
    /// it (see [`Registry::reconcile_booted`]).
    pub fn generation(&self) -> Generation {
        self.next_generation
    }

    pub fn is_owned(&self, udid: &DeviceId) -> bool {
        self.devices.get(udid).is_some_and(|d| d.owned)
    }

    /// Attach `worktree` to `udid`. `booted` is the device's current state
    /// from `simctl` (the caller just listed devices to pick it). Detaches the
    /// worktree's previous device first.
    pub fn attach(&mut self, worktree: WorktreeKey, udid: DeviceId, booted: bool, now: Instant) -> Vec<Effect<S>> {
        let mut effects = Vec::new();
        if let Some(previous) = self.attachments.get(&worktree).cloned() {
            if previous == udid {
                // Same device: a no-op while it is coming up or up; from
                // Idle/Failed/Disconnected (e.g. a restored attachment) this
                // is the user's "Attach" and starts it.
                let generation = self.bump();
                let device = self.device_mut(&udid);
                return match device.phase {
                    Phase::Booting { .. } | Phase::Starting { .. } | Phase::Live { .. } => effects,
                    _ => {
                        device.restarts = 0;
                        device.idle_since = None;
                        vec![start_or_boot(device, &udid, booted, generation)]
                    }
                };
            }
            effects.extend(self.detach(&worktree, now));
        }
        self.attachments.insert(worktree.clone(), udid.clone());
        let generation = self.bump();
        let shown = self.visible.contains(&worktree);
        let device = self.device_mut(&udid);
        if shown {
            device.visible.insert(worktree.clone());
        }
        device.attached.insert(worktree);
        device.idle_since = None;
        match device.phase {
            // Already coming up or up: join it — and resume it if this
            // viewer is the first to show it (a paused device would park).
            Phase::Booting { .. } | Phase::Starting { .. } | Phase::Live { .. } => {
                effects.extend(pause_if_hidden(device, Some(now)));
            }
            Phase::Idle | Phase::Parked | Phase::Disconnected { .. } | Phase::Failed { .. } => {
                device.restarts = 0;
                device.paused = false;
                effects.push(start_or_boot(device, &udid, booted, generation));
            }
        }
        effects.push(Effect::Persist);
        effects
    }

    /// Detach `worktree`. The last detach stops the session (and cancels a
    /// boot in flight); an owned device starts its idle clock.
    pub fn detach(&mut self, worktree: &WorktreeKey, now: Instant) -> Vec<Effect<S>> {
        let Some(udid) = self.attachments.remove(worktree) else { return Vec::new() };
        let mut effects = Vec::new();
        let device = self.device_mut(&udid);
        device.attached.remove(worktree);
        let was_visible = device.visible.remove(worktree);
        if device.attached.is_empty() {
            if let Some(cancel) = device.boot_cancel.take() {
                cancel.store(true, Ordering::SeqCst);
            }
            if let Some(session) = device.session.take() {
                effects.push(Effect::StopSession { udid: udid.clone(), session });
            }
            device.phase = Phase::Idle;
            device.paused = false;
            device.hidden_since = None;
            if device.owned {
                device.idle_since = Some(now);
            }
        } else if was_visible {
            effects.extend(pause_if_hidden(device, Some(now)));
        }
        effects.push(Effect::Persist);
        effects
    }

    /// The panel for `worktree` became visible or hidden. Showing a parked
    /// device starts a fresh helper for it.
    pub fn set_visible(&mut self, worktree: &WorktreeKey, visible: bool, now: Instant) -> Vec<Effect<S>> {
        if visible {
            self.visible.insert(worktree.clone());
        } else {
            self.visible.remove(worktree);
        }
        let Some(udid) = self.attachments.get(worktree).cloned() else { return Vec::new() };
        let unpark = visible && self.devices.get(&udid).is_some_and(|d| d.phase == Phase::Parked);
        // Only a restart moves the generation (the watcher keys on it).
        let generation = if unpark { self.bump() } else { self.next_generation };
        let device = self.device_mut(&udid);
        if visible {
            device.visible.insert(worktree.clone());
        } else {
            device.visible.remove(worktree);
        }
        if unpark {
            device.restarts = 0;
            device.phase = Phase::Starting { generation };
            return vec![Effect::StartSession { udid, generation }];
        }
        pause_if_hidden(device, Some(now)).into_iter().collect()
    }

    /// An agent started or stopped driving `udid`. While it drives, the
    /// device counts as seen: a paused helper resumes and nothing parks it.
    /// Once it stops, a device no panel shows pauses again and parks
    /// [`PARK_AFTER`] later, like any hidden device.
    pub fn set_agent_active(&mut self, udid: &DeviceId, active: bool, now: Instant) -> Vec<Effect<S>> {
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        device.agent = active;
        pause_if_hidden(device, Some(now)).into_iter().collect()
    }

    /// A [`Effect::Boot`] finished.
    pub fn boot_finished(&mut self, udid: &DeviceId, generation: Generation, result: BootResult) -> Vec<Effect<S>> {
        let next = self.bump();
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        let mut effects = Vec::new();
        // Ownership first, stale or not: only a boot we performed makes the
        // device ours.
        if matches!(result, BootResult::AlreadyBooted | BootResult::Failed(_)) && device.owned {
            device.owned = false;
            device.idle_since = None;
            effects.push(Effect::Persist);
        }
        // Ours for certain now, even if a watcher poll racing the boot
        // cleared it in between.
        if result == BootResult::Booted && !device.owned {
            device.owned = true;
            effects.push(Effect::Persist);
        }
        if device.phase != (Phase::Booting { generation }) {
            // Stale: detached or switched meanwhile. If we did boot it, it is
            // owned and the idle rule shuts it down later.
            return effects;
        }
        device.boot_cancel = None;
        match result {
            BootResult::Booted | BootResult::AlreadyBooted => {
                device.phase = Phase::Starting { generation: next };
                effects.push(Effect::StartSession { udid: udid.clone(), generation: next });
            }
            BootResult::Failed(error) => device.phase = Phase::Failed { error },
            BootResult::Cancelled => device.phase = Phase::Idle,
        }
        effects
    }

    /// A [`Effect::StartSession`] finished. A result for a stale generation
    /// (or a device nobody is attached to any more) is stopped, never
    /// installed.
    pub fn session_started(&mut self, udid: &DeviceId, generation: Generation, result: Result<S, String>) -> Vec<Effect<S>> {
        let current = self.devices.get(udid).is_some_and(|d| {
            d.phase == (Phase::Starting { generation }) && !d.attached.is_empty()
        });
        let device = match (current, result) {
            (false, Ok(session)) => return vec![Effect::StopSession { udid: udid.clone(), session }],
            (false, Err(_)) => return Vec::new(),
            (true, Err(error)) => {
                self.device_mut(udid).phase = Phase::Failed { error };
                return Vec::new();
            }
            (true, Ok(session)) => {
                let device = self.device_mut(udid);
                device.session = Some(session);
                device.phase = Phase::Live { generation };
                device.paused = false;
                device
            }
        };
        // No clock here: a hidden start's parking clock starts at the next tick.
        pause_if_hidden(device, None).into_iter().collect()
    }

    /// The session for `generation` ended (helper exited). One automatic
    /// restart while the device is still booted and attached; after that the
    /// panel shows Disconnected and waits for the user.
    pub fn session_exited(&mut self, udid: &DeviceId, generation: Generation, still_booted: bool, reason: String) -> Vec<Effect<S>> {
        let next = self.bump();
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        if device.phase != (Phase::Live { generation }) {
            return Vec::new();
        }
        let mut effects: Vec<Effect<S>> = device
            .session
            .take()
            .map(|session| Effect::StopSession { udid: udid.clone(), session })
            .into_iter()
            .collect();
        if still_booted && !device.attached.is_empty() && device.restarts == 0 {
            device.restarts += 1;
            device.phase = Phase::Starting { generation: next };
            effects.push(Effect::StartSession { udid: udid.clone(), generation: next });
        } else {
            device.phase = Phase::Disconnected { reason };
        }
        effects
    }

    /// Reconnect a Disconnected/Failed device (the user's Reconnect), or
    /// restart a live one (the selected Xcode changed under it). A boot in
    /// flight is left to finish.
    pub fn reconnect(&mut self, udid: &DeviceId, booted: bool) -> Vec<Effect<S>> {
        let generation = self.bump();
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        if device.attached.is_empty() || matches!(device.phase, Phase::Booting { .. }) {
            return Vec::new();
        }
        let mut effects: Vec<Effect<S>> = device
            .session
            .take()
            .map(|session| Effect::StopSession { udid: udid.clone(), session })
            .into_iter()
            .collect();
        device.restarts = 0;
        device.paused = false;
        device.hidden_since = None;
        effects.push(start_or_boot(device, udid, booted, generation));
        if !booted {
            effects.push(Effect::Persist); // now an owned boot
        }
        effects
    }

    /// Every device that currently has attachments.
    pub fn attached_devices(&self) -> Vec<DeviceId> {
        let mut out: Vec<_> = self.devices.iter().filter(|(_, d)| !d.attached.is_empty()).map(|(u, _)| u.clone()).collect();
        out.sort();
        out
    }

    /// The device watcher saw `udid` shut down (by anyone). It is not ours to
    /// shut down any more; a live session is now Disconnected.
    pub fn device_shutdown(&mut self, udid: &DeviceId) -> Vec<Effect<S>> {
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        let mut effects = Vec::new();
        let was_owned = std::mem::replace(&mut device.owned, false);
        device.idle_since = None;
        if let Some(session) = device.session.take() {
            effects.push(Effect::StopSession { udid: udid.clone(), session });
        }
        if matches!(device.phase, Phase::Live { .. } | Phase::Starting { .. } | Phase::Parked) {
            device.phase = Phase::Disconnected { reason: "The device shut down.".into() };
        }
        if was_owned {
            effects.push(Effect::Persist);
        }
        effects
    }

    /// The device watcher's full booted set (every poll, the first included).
    /// A device we think we own that is not booted (and not mid-boot) stops
    /// being ours — it was shut down by someone, or by our own quit-time
    /// script — so a later user boot of it can never be taken for ours. A
    /// session on a device that is no longer booted is Disconnected.
    ///
    /// Only apply a set listed while [`Registry::generation`] stood still: a
    /// boot that finished during the listing would be misread as a shutdown.
    pub fn reconcile_booted(&mut self, booted: &BTreeSet<DeviceId>) -> Vec<Effect<S>> {
        let gone: Vec<DeviceId> = self
            .devices
            .iter()
            .filter(|(udid, d)| {
                !booted.contains(*udid)
                    && !matches!(d.phase, Phase::Booting { .. })
                    && (d.owned || matches!(d.phase, Phase::Live { .. } | Phase::Starting { .. } | Phase::Parked))
            })
            .map(|(u, _)| u.clone())
            .collect();
        gone.iter().flat_map(|udid| self.device_shutdown(udid)).collect()
    }

    /// Periodic housekeeping: park devices hidden for [`PARK_AFTER`], and
    /// shut down owned devices idle for [`IDLE_SHUTDOWN`].
    pub fn tick(&mut self, now: Instant) -> Vec<Effect<S>> {
        let mut effects = Vec::new();
        for (udid, device) in &mut self.devices {
            if !device.paused || !matches!(device.phase, Phase::Live { .. }) {
                continue;
            }
            let since = *device.hidden_since.get_or_insert(now);
            if now.duration_since(since) >= PARK_AFTER
                && let Some(session) = device.session.take()
            {
                device.phase = Phase::Parked;
                device.paused = false;
                device.hidden_since = None;
                effects.push(Effect::StopSession { udid: udid.clone(), session });
            }
        }
        let due: Vec<DeviceId> = self
            .devices
            .iter()
            .filter(|(_, d)| {
                d.owned && d.attached.is_empty() && d.idle_since.is_some_and(|t| now.duration_since(t) >= IDLE_SHUTDOWN)
            })
            .map(|(u, _)| u.clone())
            .collect();
        for udid in &due {
            self.devices.remove(udid);
            effects.push(Effect::ShutdownDevice { udid: udid.clone() });
        }
        if !due.is_empty() {
            effects.push(Effect::Persist);
        }
        effects
    }

    /// App quit: every session to stop, and the owned devices to shut down
    /// (after we are gone — see the desktop quit hook). Ownership of those is
    /// handed to that shutdown and cleared here, so a snapshot saved after
    /// quit can never make a later, user-booted instance of them ours.
    pub fn quit(&mut self) -> (Vec<S>, Vec<DeviceId>) {
        let mut sessions = Vec::new();
        for device in self.devices.values_mut() {
            if let Some(cancel) = device.boot_cancel.take() {
                cancel.store(true, Ordering::SeqCst);
            }
            sessions.extend(device.session.take());
        }
        let mut owned: Vec<DeviceId> =
            self.devices.iter().filter(|(_, d)| d.owned).map(|(u, _)| u.clone()).collect();
        owned.sort();
        for device in self.devices.values_mut() {
            device.owned = false;
        }
        (sessions, owned)
    }

    fn bump(&mut self) -> Generation {
        let g = self.next_generation;
        self.next_generation += 1;
        g
    }

    fn device_mut(&mut self, udid: &DeviceId) -> &mut Device<S> {
        self.devices.entry(udid.clone()).or_insert_with(Device::new)
    }
}

/// Boot the device (we then own it) or, already booted, start a session.
fn start_or_boot<S>(device: &mut Device<S>, udid: &DeviceId, booted: bool, generation: Generation) -> Effect<S> {
    if booted {
        device.phase = Phase::Starting { generation };
        Effect::StartSession { udid: udid.clone(), generation }
    } else {
        let cancel = Arc::new(AtomicBool::new(false));
        device.boot_cancel = Some(cancel.clone());
        // Owned from the moment we ask: a boot that completes after a cancel
        // still leaves a device we started, which the idle rule must stop.
        device.owned = true;
        device.phase = Phase::Booting { generation };
        Effect::Boot { udid: udid.clone(), generation, cancel }
    }
}

/// Pause when no attached worktree shows the device; resume when one does.
/// `now` starts the parking clock (`None`: the next tick starts it).
fn pause_if_hidden<S: Clone>(device: &mut Device<S>, now: Option<Instant>) -> Option<Effect<S>> {
    let session = device.session.clone()?;
    let hidden = device.visible.is_empty() && !device.agent;
    if hidden == device.paused {
        return None;
    }
    device.paused = hidden;
    device.hidden_since = if hidden { now } else { None };
    Some(if hidden { Effect::Pause(session) } else { Effect::Resume(session) })
}

/// Which device to attach when the user did not name one: a booted iPhone,
/// else the preferred device (Settings), else the iPhone on the newest
/// runtime. Returns the device and whether it is already booted.
pub fn auto_pick<'a>(devices: &'a [DeviceInfo], preferred: Option<&DeviceId>) -> Option<(&'a DeviceInfo, bool)> {
    let usable = |d: &&DeviceInfo| d.is_available && d.kind != DeviceKind::Other;
    let booted = |d: &DeviceInfo| d.state == DeviceState::Booted;
    if let Some(d) = devices.iter().filter(usable).find(|d| booted(d) && d.kind == DeviceKind::Phone) {
        return Some((d, true));
    }
    if let Some(d) = preferred.and_then(|p| devices.iter().filter(usable).find(|d| &d.udid == p)) {
        return Some((d, booted(d)));
    }
    devices
        .iter()
        .filter(usable)
        .filter(|d| d.kind == DeviceKind::Phone)
        .max_by(|a, b| version_key(&a.os_version).cmp(&version_key(&b.os_version)))
        .map(|d| (d, booted(d)))
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
