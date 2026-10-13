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
//! - **Visibility is counted per device** ([`crate::viewers`]): each panel,
//!   tile or remote view of a worktree's device is a viewer. The helper is
//!   paused only when no viewer shows it, and a device hidden for
//!   [`PARK_AFTER`] is **parked** — its helper stopped, its attachments kept
//!   — until a viewer shows it again. Its stream quality is the best any
//!   shown viewer asks for.
//!
//! The session type is generic (`S`) so tests use a plain token; the app uses
//! `StreamSession`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::viewers::{StreamProfile, Viewer, ViewerId, arbitrate};
use crate::{DeviceId, DeviceInfo, DeviceKind, DeviceState, Source};

/// An owned device is shut down this long after its last detach, unless
/// Settings chose another delay (or never: [`Registry::set_idle_shutdown`]).
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

/// Why a real device's stream ended when the device itself went away.
const UNPLUGGED: &str = "The phone was unplugged. Plug it back in, then Reconnect.";
/// Why a real device's stream ended while the device is still there (its
/// USB connection reset, the stream server died).
const DROPPED: &str = "The connection to the phone dropped. Reconnect to stream again.";

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
    /// Stream this session at `profile` (see [`crate::viewers`]).
    SetProfile { udid: DeviceId, session: S, profile: StreamProfile },
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
    /// The viewers of its attached worktrees (a copy of the registry's).
    viewers: BTreeMap<(WorktreeKey, ViewerId), Viewer>,
    /// The profile its session streams at (a fresh session: the user's).
    profile: StreamProfile,
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
            viewers: BTreeMap::new(),
            profile: StreamProfile::Full,
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
    /// Every worktree's viewers, attached or not: a panel shown before its
    /// attach lands must not start its helper paused.
    viewers: BTreeMap<(WorktreeKey, ViewerId), Viewer>,
    /// How long an owned, unattached device idles before it is shut down;
    /// `None` keeps it running.
    idle_shutdown: Option<Duration>,
}

impl<S> Default for Registry<S> {
    fn default() -> Self {
        Self {
            attachments: HashMap::new(),
            devices: HashMap::new(),
            next_generation: 1,
            viewers: BTreeMap::new(),
            idle_shutdown: Some(IDLE_SHUTDOWN),
        }
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
        // A real device is never ours: an owned one in an old (or tampered)
        // snapshot is dropped, never shut down.
        for udid in snapshot.owned_boots.into_iter().filter(|u| !u.is_physical()) {
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
                        let mut effects: Vec<Effect<S>> = start_or_boot(device, &udid, booted, generation).into_iter().collect();
                        if !booted && device.owned {
                            // Now an owned boot: saved at once, so a crash
                            // before quit still leaves it ours to shut down.
                            effects.push(Effect::Persist);
                        }
                        effects
                    }
                };
            }
            effects.extend(self.detach(&worktree, now));
        }
        self.attachments.insert(worktree.clone(), udid.clone());
        let generation = self.bump();
        let viewers: Vec<_> = self.viewers_of(&worktree).collect();
        let device = self.device_mut(&udid);
        device.viewers.extend(viewers);
        device.attached.insert(worktree);
        device.idle_since = None;
        match device.phase {
            // Already coming up or up: join it — and resume it if this
            // viewer is the first to show it (a paused device would park).
            Phase::Booting { .. } | Phase::Starting { .. } | Phase::Live { .. } => {
                effects.extend(follow_viewers(&udid, device, Some(now)));
            }
            Phase::Idle | Phase::Parked | Phase::Disconnected { .. } | Phase::Failed { .. } => {
                device.restarts = 0;
                device.paused = false;
                effects.extend(start_or_boot(device, &udid, booted, generation));
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
        device.viewers.retain(|(w, _), _| w != worktree);
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
        } else {
            effects.extend(follow_viewers(&udid, device, Some(now)));
        }
        effects.push(Effect::Persist);
        effects
    }

    /// The panel for `worktree` became visible or hidden: one
    /// [`ViewerId::Panel`] viewer, at the user's stream settings.
    pub fn set_visible(&mut self, worktree: &WorktreeKey, visible: bool, now: Instant) -> Vec<Effect<S>> {
        let viewer = Viewer { shown: true, profile: StreamProfile::Full };
        self.put_viewer(worktree, ViewerId::Panel(0), visible.then_some(viewer), now)
    }

    /// Add or update a viewer of `worktree`'s device (attached now or
    /// later). A shown viewer of a parked device starts a fresh helper.
    pub fn set_viewer(&mut self, worktree: &WorktreeKey, id: ViewerId, viewer: Viewer, now: Instant) -> Vec<Effect<S>> {
        self.put_viewer(worktree, id, Some(viewer), now)
    }

    /// The viewer went away (its tile closed, its window too).
    pub fn release_viewer(&mut self, worktree: &WorktreeKey, id: ViewerId, now: Instant) -> Vec<Effect<S>> {
        self.put_viewer(worktree, id, None, now)
    }

    fn put_viewer(&mut self, worktree: &WorktreeKey, id: ViewerId, viewer: Option<Viewer>, now: Instant) -> Vec<Effect<S>> {
        let key = (worktree.clone(), id);
        match viewer {
            Some(v) => self.viewers.insert(key.clone(), v),
            None => self.viewers.remove(&key),
        };
        let Some(udid) = self.attachments.get(worktree).cloned() else { return Vec::new() };
        let shown = viewer.is_some_and(|v| v.shown);
        let unpark = shown && self.devices.get(&udid).is_some_and(|d| d.phase == Phase::Parked);
        // Only a restart moves the generation (the watcher keys on it).
        let generation = if unpark { self.bump() } else { self.next_generation };
        let device = self.device_mut(&udid);
        match viewer {
            Some(v) => device.viewers.insert(key, v),
            None => device.viewers.remove(&key),
        };
        if unpark {
            device.restarts = 0;
            device.phase = Phase::Starting { generation };
            return vec![Effect::StartSession { udid, generation }];
        }
        follow_viewers(&udid, device, Some(now))
    }

    /// The profile `udid`'s session streams at.
    pub fn profile(&self, udid: &DeviceId) -> StreamProfile {
        self.devices.get(udid).map_or(StreamProfile::Full, |d| d.profile)
    }

    /// Every attachment, by worktree.
    pub fn attachments(&self) -> Vec<(WorktreeKey, DeviceId)> {
        let mut out: Vec<_> = self.attachments.iter().map(|(w, d)| (w.clone(), d.clone())).collect();
        out.sort();
        out
    }

    fn viewers_of<'a>(&'a self, worktree: &'a WorktreeKey) -> impl Iterator<Item = ((WorktreeKey, ViewerId), Viewer)> + 'a {
        self.viewers.iter().filter(move |((w, _), _)| w == worktree).map(|(k, v)| (k.clone(), *v))
    }

    /// An agent started or stopped driving `udid`. While it drives, the
    /// device counts as seen: a paused helper resumes and nothing parks it.
    /// Once it stops, a device no panel shows pauses again and parks
    /// [`PARK_AFTER`] later, like any hidden device.
    pub fn set_agent_active(&mut self, udid: &DeviceId, active: bool, now: Instant) -> Vec<Effect<S>> {
        let Some(device) = self.devices.get_mut(udid) else { return Vec::new() };
        device.agent = active;
        follow_viewers(udid, device, Some(now))
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
        // cleared it in between. Never a real device, whatever was reported.
        if result == BootResult::Booted && !device.owned && !udid.is_physical() {
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
                // A fresh helper streams at the user's settings.
                device.profile = StreamProfile::Full;
                device
            }
        };
        // No clock here: a hidden start's parking clock starts at the next tick.
        follow_viewers(udid, device, None)
    }

    /// The session for `generation` ended (helper exited). One automatic
    /// restart while the device is still booted and attached; after that the
    /// panel shows Disconnected and waits for the user. A real device is
    /// not restarted behind the user's back (an unplug ends a session too):
    /// it shows Disconnected at once, and Reconnect brings it back — except
    /// an iPhone whose capture lost it while it is still listed: a phone
    /// briefly re-enumerating on USB (measured, plugged in throughout) gets
    /// the one restart, and a real unplug is Disconnected once the watcher
    /// sees it gone. One per attachment, as for a simulator: attach,
    /// unpark and Reconnect allow the next one.
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
        let iphone_blip = udid.source() == Source::Devicectl && reason == crate::ios_device::PHONE_DROPPED;
        if still_booted && !device.attached.is_empty() && device.restarts == 0 && (!udid.is_physical() || iphone_blip) {
            device.restarts += 1;
            device.phase = Phase::Starting { generation: next };
            effects.push(Effect::StartSession { udid: udid.clone(), generation: next });
        } else if udid.is_physical() {
            // A connection that just ended says so in words for a person; any
            // other failure (a decoder, an OEM encoder) keeps its own.
            let reason = if !still_booted {
                UNPLUGGED.into()
            } else if reason == crate::android::session::STREAM_DROPPED {
                DROPPED.into()
            } else {
                reason
            };
            device.phase = Phase::Disconnected { reason };
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
        effects.extend(start_or_boot(device, udid, booted, generation));
        if !booted && device.owned {
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

    /// A visible panel shows `udid`, streaming or not.
    pub fn is_shown(&self, udid: &DeviceId) -> bool {
        self.devices.get(udid).is_some_and(has_shown_viewer)
    }

    /// Every device a visible panel shows while it streams.
    pub fn shown_live(&self) -> Vec<DeviceId> {
        let mut out: Vec<_> = self
            .devices
            .iter()
            .filter(|(_, d)| has_shown_viewer(d) && matches!(d.phase, Phase::Live { .. }))
            .map(|(u, _)| u.clone())
            .collect();
        out.sort();
        out
    }

    /// The devices OxiMux booted and still owns (the idle rule shuts them down).
    pub fn owned_devices(&self) -> Vec<DeviceId> {
        let mut out: Vec<_> = self.devices.iter().filter(|(_, d)| d.owned).map(|(u, _)| u.clone()).collect();
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
            // A real device is never shut down from here: it went away.
            let reason = if udid.is_physical() { UNPLUGGED } else { "The device shut down." };
            device.phase = Phase::Disconnected { reason: reason.into() };
        } else if udid.is_physical() && device.phase == (Phase::Disconnected { reason: DROPPED.into() }) {
            // The stream's end beat the watcher to it: the phone is gone, not
            // just its connection.
            device.phase = Phase::Disconnected { reason: UNPLUGGED.into() };
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
    /// And only for the sources the set covers (`listed`): a round that
    /// skipped `adb` (or `simctl`, or `devicectl`) says nothing about those
    /// devices, and reading it as "shut down" would drop the ownership the
    /// idle rule needs — or disconnect a live phone.
    pub fn reconcile_booted(&mut self, booted: &BTreeSet<DeviceId>, listed: &[Source]) -> Vec<Effect<S>> {
        let gone: Vec<DeviceId> = self
            .devices
            .iter()
            .filter(|(udid, d)| {
                listed.contains(&udid.source())
                    && !booted.contains(*udid)
                    && !matches!(d.phase, Phase::Booting { .. })
                    && (d.owned
                        || matches!(d.phase, Phase::Live { .. } | Phase::Starting { .. } | Phase::Parked)
                        // A dropped phone the watcher now sees gone: unplugged.
                        || d.phase == (Phase::Disconnected { reason: DROPPED.into() }))
            })
            .map(|(u, _)| u.clone())
            .collect();
        gone.iter().flat_map(|udid| self.device_shutdown(udid)).collect()
    }

    /// The idle-shutdown delay from Settings (`None`: never).
    pub fn set_idle_shutdown(&mut self, after: Option<Duration>) {
        self.idle_shutdown = after;
    }

    /// Periodic housekeeping: park devices hidden for [`PARK_AFTER`], and
    /// shut down owned devices idle for the idle-shutdown delay.
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
        let idle = self.idle_shutdown;
        let due: Vec<DeviceId> = self
            .devices
            .iter()
            .filter(|(_, d)| {
                d.owned
                    && d.attached.is_empty()
                    && idle.zip(d.idle_since).is_some_and(|(after, since)| now.duration_since(since) >= after)
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

/// Boot the device (we then own it) or, already booted, start a session. A
/// real device is never booted or owned: one that is not connected fails.
fn start_or_boot<S>(device: &mut Device<S>, udid: &DeviceId, booted: bool, generation: Generation) -> Option<Effect<S>> {
    if booted {
        device.phase = Phase::Starting { generation };
        Some(Effect::StartSession { udid: udid.clone(), generation })
    } else if udid.is_physical() {
        device.phase = Phase::Failed { error: not_connected(udid) };
        None
    } else {
        let cancel = Arc::new(AtomicBool::new(false));
        device.boot_cancel = Some(cancel.clone());
        // Owned from the moment we ask: a boot that completes after a cancel
        // still leaves a device we started, which the idle rule must stop.
        device.owned = true;
        device.phase = Phase::Booting { generation };
        Some(Effect::Boot { udid: udid.clone(), generation, cancel })
    }
}

/// Why a real device that is not listed as connected cannot start.
fn not_connected(udid: &DeviceId) -> String {
    match udid.source() {
        Source::Devicectl => "The iPhone is not connected.".into(),
        _ => "The phone is not connected (or USB debugging is not allowed on it).".into(),
    }
}

/// Whether any viewer shows the device now.
fn has_shown_viewer<S>(device: &Device<S>) -> bool {
    device.viewers.values().any(|v| v.shown)
}

/// Bring the session in line with its viewers: paused or not, and the
/// profile they ask for (an agent driving it asks for the user's settings).
fn follow_viewers<S: Clone>(udid: &DeviceId, device: &mut Device<S>, now: Option<Instant>) -> Vec<Effect<S>> {
    let mut effects: Vec<Effect<S>> = pause_if_hidden(device, now).into_iter().collect();
    let wanted = if device.agent { Some(StreamProfile::Full) } else { arbitrate(device.viewers.values()) };
    if let (Some(profile), Some(session)) = (wanted, device.session.clone())
        && profile != device.profile
    {
        device.profile = profile;
        effects.push(Effect::SetProfile { udid: udid.clone(), session, profile });
    }
    effects
}

/// Pause when no viewer shows the device; resume when one does.
/// `now` starts the parking clock (`None`: the next tick starts it).
fn pause_if_hidden<S: Clone>(device: &mut Device<S>, now: Option<Instant>) -> Option<Effect<S>> {
    let session = device.session.clone()?;
    let hidden = !has_shown_viewer(device) && !device.agent;
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
    // A real device (a phone, an iPhone) is someone's own: only ever picked
    // by name, never automatically — not even as the Settings default.
    let automatic = |d: &&DeviceInfo| d.is_available && d.kind != DeviceKind::Other && !d.udid.is_physical();
    let booted = |d: &DeviceInfo| d.state == DeviceState::Booted;
    if let Some(d) = devices.iter().filter(automatic).find(|d| booted(d) && d.kind == DeviceKind::Phone) {
        return Some((d, true));
    }
    if let Some(d) = preferred.and_then(|p| devices.iter().filter(automatic).find(|d| &d.udid == p)) {
        return Some((d, booted(d)));
    }
    devices
        .iter()
        .filter(automatic)
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
