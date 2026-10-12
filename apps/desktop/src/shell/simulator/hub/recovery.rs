//! A blank screen that recovers on its own: a simulator a panel shows (so
//! streaming, not paused) whose stream has sent no picture at all since it
//! started — timed only while shown, [`NO_PICTURE`] of it — climbs a short
//! ladder, one step at a time:
//!
//! 1. wait for its boot to finish (`simctl bootstatus`, ≤ 60 s), then
//!    restart the stream — a helper started before the device finished
//!    booting can sit on an empty framebuffer;
//! 2. once per device per run, and only for a device OxiMux booted: shut it
//!    down and boot it again;
//! 3. a hint in the panel: "Erase device…" — never done on its own.
//!
//! Only a stream that never showed anything counts: a still screen sends no
//! pictures (measured: none for 20 s and more on an idle home screen), so
//! "no picture lately" would raise false alarms. What never starts it: a
//! hidden (paused) device — a paused helper captures nothing — time spent
//! paused, a stream that showed a picture, and a decoder in
//! trouble (an H.264 fallback): those keep today's error path. A step that
//! ends re-checks before it acts: the same stream, still shown, still blank,
//! not being erased, not stopped by the user. A boot that does not finish in
//! time ends at the hint (a reboot would only interrupt it). [`Recovery`] is
//! the pure decision; the hub runs what it asks for.

use std::time::{Duration, Instant};

use gpui::{App, Context, WeakEntity};
use oximux_simulator::registry::Phase;
use oximux_simulator::{DeviceId, Source, simctl};

use super::{HubEvent, SimulatorHub};

/// Shown and streaming this long without a single picture: something is
/// wrong.
const NO_PICTURE: Duration = Duration::from_secs(10);
/// How often shown devices are looked at.
const POLL: Duration = Duration::from_secs(2);
/// `simctl bootstatus` waits at most this long.
const BOOT_WAIT: Duration = Duration::from_secs(60);

/// Where a device is on the ladder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    /// Pictures arrive (or have not been missed for long).
    #[default]
    Watching,
    /// Waiting for the boot to finish, then restarting the stream.
    WaitingForBoot,
    /// Shutting down and booting again.
    Rebooting,
    /// Nothing helped: the panel suggests erasing.
    Hint,
}

/// What the hub should do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    WaitForBoot,
    Reboot,
}

/// What the hub sees of a device at one look.
#[derive(Clone, Copy, Debug)]
pub struct Seen {
    /// A panel shows it and its stream is live (so not paused).
    pub shown_live: bool,
    /// The stream it is ([`Phase::Live`]'s generation).
    pub generation: u64,
    /// Pictures since the stream started.
    pub seq: u64,
    /// Bumped by every pause and resume.
    pub epoch: u64,
    /// OxiMux booted it (only such a device is ever rebooted).
    pub owned: bool,
}

impl Seen {
    /// The stream has shown a picture.
    fn shown_a_picture(&self) -> bool {
        self.seq > 0
    }
}

/// One device's ladder.
#[derive(Debug, Default)]
pub struct Recovery {
    step: Step,
    /// The stream and pause epoch being timed, and since when.
    clock: Option<(u64, u64, Instant)>,
    /// The stream a running step was started for: wait for it to finish.
    busy: Option<u64>,
    /// The one reboot this run is spent.
    rebooted: bool,
    /// The stream's decoder gave up (an H.264 fallback): not a blank screen.
    decoder_trouble: Option<u64>,
}

impl Recovery {
    pub fn step(&self) -> Step {
        self.step
    }

    /// The stream a running step was started for.
    #[cfg(test)]
    pub fn busy_for(&self) -> Option<u64> {
        self.busy
    }

    /// The stream `generation` fell back from H.264: its blank screen is the
    /// decoder's, and never reboots anything.
    pub fn decoder_trouble(&mut self, generation: u64) {
        self.decoder_trouble = Some(generation);
        self.clock = None;
    }

    /// The step asked for has finished: time the next stretch afresh.
    pub fn finished(&mut self) {
        self.busy = None;
        self.clock = None;
    }

    /// The boot did not finish in time (a slow first boot, say): rebooting
    /// it now would only interrupt it, so the ladder ends at the hint.
    pub fn boot_wait_failed(&mut self) {
        self.finished();
        self.step = Step::Hint;
    }

    /// One look at the device; what to do now.
    pub fn observe(&mut self, now: Instant, seen: Seen) -> Action {
        if seen.shown_a_picture() {
            // Pictures: whatever was wrong is not (or no longer).
            if self.busy.is_none() {
                self.step = Step::Watching;
            }
            self.clock = None;
            return Action::None;
        }
        if !seen.shown_live {
            // Not shown (hidden, detached, stopped, failed): no clock, and
            // no stale "waiting…" line; a running step finishes on its own.
            self.clock = None;
            if self.busy.is_none() && matches!(self.step, Step::WaitingForBoot | Step::Rebooting) {
                self.step = Step::Watching;
            }
            return Action::None;
        }
        let troubled = self.decoder_trouble == Some(seen.generation);
        if troubled || self.busy.is_some() || self.step == Step::Hint {
            if self.busy.is_none() {
                self.clock = None;
            }
            return Action::None;
        }
        // A new stream, or a pause and resume, starts the clock again.
        let since = match self.clock {
            Some((generation, epoch, since)) if generation == seen.generation && epoch == seen.epoch => since,
            _ => {
                self.clock = Some((seen.generation, seen.epoch, now));
                return Action::None;
            }
        };
        if now.duration_since(since) < NO_PICTURE {
            return Action::None;
        }
        self.clock = None;
        let (step, action) = match self.step {
            Step::Watching => (Step::WaitingForBoot, Action::WaitForBoot),
            Step::WaitingForBoot if seen.owned && !self.rebooted => {
                self.rebooted = true;
                (Step::Rebooting, Action::Reboot)
            }
            Step::WaitingForBoot | Step::Rebooting | Step::Hint => (Step::Hint, Action::None),
        };
        self.step = step;
        self.busy = (action != Action::None).then_some(seen.generation);
        action
    }
}

impl SimulatorHub {
    /// Where `udid` is on the ladder (the panel's line and hint).
    pub fn recovery_step(&self, udid: &DeviceId) -> Step {
        self.recovery.get(udid).map(Recovery::step).unwrap_or_default()
    }

    /// The stream `generation` of `udid` fell back from H.264.
    pub(super) fn note_decoder_trouble(&mut self, udid: &DeviceId, generation: u64) {
        self.recovery.entry(udid.clone()).or_default().decoder_trouble(generation);
    }

    /// What the hub sees of `udid` now (`None`: no live stream).
    fn seen(&self, udid: &DeviceId, shown: &[DeviceId]) -> Option<Seen> {
        let (Phase::Live { generation }, Some(session)) = (self.registry.phase(udid), self.session(udid)) else { return None };
        let clock = session.frame_clock();
        Some(Seen { shown_live: shown.contains(udid), generation, seq: clock.seq, epoch: clock.epoch, owned: self.is_owned(udid) })
    }

    /// A step's ending may act on `udid` only if nothing changed meanwhile:
    /// the same stream, still shown, still without a picture, not being
    /// erased, not stopped by the user.
    fn still_blank(&self, udid: &DeviceId, generation: u64) -> bool {
        let shown = self.registry.shown_live();
        let unchanged = self.seen(udid, &shown).is_some_and(|s| s.shown_live && s.generation == generation && !s.shown_a_picture());
        unchanged && !self.erasing(udid) && !self.is_stopped_by_user(udid)
    }

    /// Look at every shown simulator; start the steps the ladder asks for.
    fn look_for_blank_screens(&mut self, cx: &mut Context<Self>) {
        // Never `xcrun` without a resolvable Xcode (the CLT dialog).
        if !self.watch_gate().xcode_ok {
            return;
        }
        let now = Instant::now();
        let shown = self.registry.shown_live();
        let mut udids: Vec<DeviceId> = shown.iter().filter(|u| u.source() == Source::Simctl).cloned().collect();
        udids.extend(self.recovery.keys().filter(|u| !shown.contains(u)).cloned());
        for udid in udids {
            let seen = self.seen(&udid, &shown).unwrap_or(Seen { shown_live: false, generation: 0, seq: 0, epoch: 0, owned: false });
            tracing::trace!(%udid, shown = seen.shown_live, generation = seen.generation, epoch = seen.epoch, seq = seen.seq, "recovery look");
            let recovery = self.recovery.entry(udid.clone()).or_default();
            let before = recovery.step();
            let action = recovery.observe(now, seen);
            if recovery.step() != before {
                cx.emit(HubEvent::Changed(udid.clone()));
            }
            match action {
                Action::None => {}
                Action::WaitForBoot => self.wait_for_boot(&udid, seen.generation, cx),
                Action::Reboot => self.reboot_blank(&udid, seen.generation, cx),
            }
        }
    }

    fn finish_step(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        if let Some(recovery) = self.recovery.get_mut(udid) {
            recovery.finished();
        }
        cx.emit(HubEvent::Changed(udid.clone()));
    }

    /// Step 1: wait for the boot, then a fresh stream — unless something
    /// changed meanwhile.
    fn wait_for_boot(&mut self, udid: &DeviceId, generation: u64, cx: &mut Context<Self>) {
        let (Some(sim), runner) = (udid.sim_udid(), self.runner.clone()) else { return self.finish_step(udid, cx) };
        tracing::info!(%udid, "no picture: waiting for the boot to finish, then restarting the stream");
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let booted = cx.background_executor().spawn(async move { simctl::bootstatus(runner.as_ref(), &sim, BOOT_WAIT) }).await;
            let _ = this.update(cx, |hub, cx| {
                if let Err(e) = booted {
                    tracing::info!(%udid, "bootstatus: {e}");
                    if let Some(recovery) = hub.recovery.get_mut(&udid) {
                        recovery.boot_wait_failed();
                    }
                    return cx.emit(HubEvent::Changed(udid.clone()));
                }
                if hub.still_blank(&udid, generation) {
                    hub.reconnect(&udid, cx);
                }
                hub.finish_step(&udid, cx);
            });
        })
        .detach();
    }

    /// Step 2: shut down (once, after a recording is finalized), wait until
    /// the device is off, then boot it again — the registry boots a device
    /// it owns when it reconnects as off. Not booted again if the user
    /// stopped it, an erase began or nothing shows it any more.
    fn reboot_blank(&mut self, udid: &DeviceId, generation: u64, cx: &mut Context<Self>) {
        if !self.still_blank(udid, generation) {
            return self.finish_step(udid, cx);
        }
        tracing::info!(%udid, "still no picture: rebooting the simulator once");
        self.shutdown_after_recording(udid, cx);
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let deadline = Instant::now() + BOOT_WAIT;
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let Ok(off) = this.update(cx, |hub, _| hub.watch.lock().unwrap().is_booted(&udid) == Some(false)) else { return };
                if off || Instant::now() >= deadline {
                    break;
                }
            }
            let _ = this.update(cx, |hub, cx| {
                let wanted = hub.registry.attached_devices().contains(&udid);
                if wanted && !hub.is_stopped_by_user(&udid) && !hub.erasing(&udid) {
                    hub.reconnect_as(&udid, false, cx);
                }
                hub.finish_step(&udid, cx);
            });
        })
        .detach();
    }
}

pub(super) fn spawn_recovery(cx: &mut App, hub: WeakEntity<SimulatorHub>) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL).await;
            if hub.update(cx, |hub, cx| hub.look_for_blank_screens(cx)).is_err() {
                return;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(seq: u64, epoch: u64, owned: bool) -> Seen {
        Seen { shown_live: true, generation: 1, seq, epoch, owned }
    }

    /// Looks every 2 s for `secs`, as the hub does; the actions asked for.
    fn run(r: &mut Recovery, start: Instant, secs: u64, at: Seen) -> Vec<Action> {
        (0..=secs / 2).map(|i| r.observe(start + Duration::from_secs(i * 2), at)).filter(|a| *a != Action::None).collect()
    }

    #[test]
    fn a_hidden_device_never_climbs_the_ladder() {
        let mut r = Recovery::default();
        let hidden = Seen { shown_live: false, ..seen(0, 0, true) };
        assert!(run(&mut r, Instant::now(), 120, hidden).is_empty(), "two minutes hidden");
        assert_eq!(r.step(), Step::Watching);
    }

    #[test]
    fn a_shown_blank_owned_simulator_waits_then_reboots_once_then_hints() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        assert_eq!(run(&mut r, t, 12, seen(0, 0, true)), [Action::WaitForBoot]);
        assert_eq!((r.step(), r.busy_for()), (Step::WaitingForBoot, Some(1)));
        // Nothing more while the step runs.
        assert!(run(&mut r, t + Duration::from_secs(14), 30, seen(0, 0, true)).is_empty());
        r.finished();
        let t = t + Duration::from_secs(60);
        assert_eq!(run(&mut r, t, 12, seen(0, 0, true)), [Action::Reboot]);
        r.finished();
        let t = t + Duration::from_secs(60);
        assert!(run(&mut r, t, 12, seen(0, 0, true)).is_empty(), "one reboot per run");
        assert_eq!(r.step(), Step::Hint);
        assert!(run(&mut r, t + Duration::from_secs(20), 60, seen(0, 0, true)).is_empty(), "the hint stays a hint");
    }

    #[test]
    fn a_device_the_user_booted_is_never_rebooted() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        assert_eq!(run(&mut r, t, 12, seen(0, 0, false)), [Action::WaitForBoot]);
        r.finished();
        assert!(run(&mut r, t + Duration::from_secs(60), 12, seen(0, 0, false)).is_empty());
        assert_eq!(r.step(), Step::Hint);
    }

    #[test]
    fn a_boot_that_does_not_finish_is_never_rebooted() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        assert_eq!(run(&mut r, t, 12, seen(0, 0, true)), [Action::WaitForBoot]);
        r.boot_wait_failed();
        assert!(run(&mut r, t + Duration::from_secs(70), 30, seen(0, 0, true)).is_empty());
        assert_eq!(r.step(), Step::Hint);
    }

    #[test]
    fn a_pause_resets_the_clock_and_a_picture_ends_the_ladder() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        // 8 s blank, then paused and resumed (a new epoch): 8 more is not 10.
        assert!(run(&mut r, t, 8, seen(0, 0, true)).is_empty());
        assert!(run(&mut r, t + Duration::from_secs(10), 8, seen(0, 1, true)).is_empty());
        assert_eq!(run(&mut r, t + Duration::from_secs(20), 12, seen(0, 1, true)), [Action::WaitForBoot]);
        r.finished();
        r.observe(t + Duration::from_secs(40), seen(1, 1, true));
        assert_eq!(r.step(), Step::Watching);
    }

    #[test]
    fn a_still_screen_is_never_taken_for_a_blank_one() {
        // One picture, then none for minutes (an idle screen sends none).
        let mut r = Recovery::default();
        assert!(run(&mut r, Instant::now(), 300, seen(1, 0, true)).is_empty());
        assert_eq!(r.step(), Step::Watching);
    }

    #[test]
    fn a_step_line_clears_once_nothing_shows_the_device() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        assert_eq!(run(&mut r, t, 12, seen(0, 0, true)), [Action::WaitForBoot]);
        r.finished();
        r.observe(t + Duration::from_secs(20), Seen { shown_live: false, ..seen(0, 0, true) });
        assert_eq!(r.step(), Step::Watching);
    }

    #[test]
    fn a_decoder_in_trouble_is_never_rebooted() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        r.decoder_trouble(1);
        assert!(run(&mut r, t, 60, seen(0, 0, true)).is_empty());
        // A new stream (after the user picks H.264 again) is timed again.
        let next = Seen { generation: 2, ..seen(0, 0, true) };
        assert_eq!(run(&mut r, t + Duration::from_secs(70), 12, next), [Action::WaitForBoot]);
    }
}
