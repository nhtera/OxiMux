//! A blank screen that recovers on its own: a simulator a panel shows (so
//! streaming, not paused) that has sent no picture at all for
//! [`NO_PICTURE`] climbs a short ladder, one step at a time:
//!
//! 1. wait for its boot to finish (`simctl bootstatus`, ≤ 60 s), then
//!    restart the stream — a helper started before the device finished
//!    booting can sit on an empty framebuffer;
//! 2. once per device per run, and only for a device OxiMux booted: shut it
//!    down and boot it again;
//! 3. a hint in the panel: "Erase device…" — never done on its own.
//!
//! What never starts it: a hidden (paused) device — a paused helper captures
//! nothing — time spent paused, a stream that already sent a picture, and a
//! decoder in trouble (an H.264 fallback): those keep today's error path.
//! [`Recovery`] is the pure decision; the hub runs what it asks for.

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

/// One device's ladder.
#[derive(Debug, Default)]
pub struct Recovery {
    step: Step,
    /// The stream and pause epoch being timed, and since when.
    clock: Option<(u64, u64, Instant)>,
    /// A step is running: wait for it to finish.
    busy: bool,
    /// The one reboot this run is spent.
    rebooted: bool,
    /// The stream's decoder gave up (an H.264 fallback): not a blank screen.
    decoder_trouble: Option<u64>,
}

impl Recovery {
    pub fn step(&self) -> Step {
        self.step
    }

    /// The stream `generation` fell back from H.264: its blank screen is the
    /// decoder's, and never reboots anything.
    pub fn decoder_trouble(&mut self, generation: u64) {
        self.decoder_trouble = Some(generation);
        self.clock = None;
    }

    /// The step asked for has finished: time the next stretch afresh.
    pub fn finished(&mut self) {
        self.busy = false;
        self.clock = None;
    }

    /// One look at the device; what to do now.
    pub fn observe(&mut self, now: Instant, seen: Seen) -> Action {
        if seen.seq > 0 {
            // Pictures: whatever was wrong is not (or no longer).
            self.step = Step::Watching;
            self.clock = None;
            return Action::None;
        }
        let troubled = self.decoder_trouble == Some(seen.generation);
        if !seen.shown_live || troubled || self.busy || self.step == Step::Hint {
            if !self.busy {
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
        self.busy = action != Action::None;
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

    /// Look at every shown simulator; start the steps the ladder asks for.
    fn look_for_blank_screens(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let shown = self.registry.shown_live();
        for udid in shown.iter().filter(|u| u.source() == Source::Simctl) {
            let (Phase::Live { generation }, Some(session)) = (self.registry.phase(udid), self.session(udid)) else { continue };
            let clock = session.frame_clock();
            let seen = Seen { shown_live: true, generation, seq: clock.seq, epoch: clock.epoch, owned: self.is_owned(udid) };
            let recovery = self.recovery.entry(udid.clone()).or_default();
            let before = recovery.step();
            let action = recovery.observe(now, seen);
            if recovery.step() != before {
                cx.emit(HubEvent::Changed(udid.clone()));
            }
            match action {
                Action::None => {}
                Action::WaitForBoot => self.wait_for_boot(udid, cx),
                Action::Reboot => self.reboot_blank(udid, cx),
            }
        }
        // A device no panel shows: its clock stops (the ladder's step and
        // spent reboot stay).
        for (udid, recovery) in &mut self.recovery {
            if !shown.contains(udid) {
                recovery.observe(now, Seen { shown_live: false, generation: 0, seq: 0, epoch: 0, owned: false });
            }
        }
    }

    /// Step 1: wait for the boot, then a fresh stream.
    fn wait_for_boot(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let (Some(sim), runner) = (udid.sim_udid(), self.runner.clone()) else { return };
        tracing::info!(%udid, "no picture: waiting for the boot to finish, then restarting the stream");
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            let booted = cx.background_executor().spawn(async move { simctl::bootstatus(runner.as_ref(), &sim, BOOT_WAIT) }).await;
            if let Err(e) = booted {
                tracing::info!(%udid, "bootstatus: {e}");
            }
            let _ = this.update(cx, |hub, cx| {
                hub.reconnect(&udid, cx);
                if let Some(recovery) = hub.recovery.get_mut(&udid) {
                    recovery.finished();
                }
            });
        })
        .detach();
    }

    /// Step 2: shut down and boot again (the registry boots a device it
    /// owns when it reconnects as off).
    fn reboot_blank(&mut self, udid: &DeviceId, cx: &mut Context<Self>) {
        let (Some(sim), runner) = (udid.sim_udid(), self.runner.clone()) else { return };
        tracing::info!(%udid, "still no picture: rebooting the simulator once");
        self.shutdown_after_recording(udid, cx);
        let udid = udid.clone();
        cx.spawn(async move |this, cx| {
            // Already shutting down from above: an error here only says so.
            let _ = cx.background_executor().spawn(async move { simctl::shutdown(runner.as_ref(), &sim, BOOT_WAIT) }).await;
            let _ = this.update(cx, |hub, cx| {
                hub.reconnect_as(&udid, false, cx);
                if let Some(recovery) = hub.recovery.get_mut(&udid) {
                    recovery.finished();
                }
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
        assert_eq!(r.step(), Step::WaitingForBoot);
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
    fn a_picture_or_a_pause_resets_the_clock() {
        let (mut r, t) = (Recovery::default(), Instant::now());
        // 8 s blank, then paused and resumed (a new epoch): 8 more is not 10.
        assert!(run(&mut r, t, 8, seen(0, 0, true)).is_empty());
        assert!(run(&mut r, t + Duration::from_secs(10), 8, seen(0, 1, true)).is_empty());
        // A picture: back to watching, whatever the step was.
        assert_eq!(run(&mut r, t + Duration::from_secs(20), 12, seen(0, 1, true)), [Action::WaitForBoot]);
        r.finished();
        r.observe(t + Duration::from_secs(40), seen(1, 1, true));
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
