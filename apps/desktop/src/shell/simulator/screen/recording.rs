//! The user's input while the panel records a flow: each tap, swipe and
//! typed character as a step the panel keeps (see `hub::flow_recorder`).
//!
//! A tap names its element from a read of the screen as it stood: while
//! recording, the screen is read once it has shown the user's last input
//! (a simulator takes about half a second to read), and again when it moves
//! on its own; a tap names its element at once from a read begun after its
//! last input had shown. Else a read starts as the finger goes down and
//! gets [`READ_BUDGET`] from there; past it the tap is kept as a point (one
//! read late still says the screen's size, when nothing else has) — the tap
//! may have changed the screen by then. A real iPhone is not read for it
//! (its runner does one job at a time): the inspector's last tree serves,
//! when there is one.
//!
//! Motion alone cannot say a read is out of date: a simulator's stream
//! sends refresh pictures for seconds after a change, and a key frame every
//! few seconds on a still screen. It says when to read again.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{Context, Pixels, Point};
use oximux_remote_proto::simulator::SimSelectorWire;
use oximux_simulator::ax;
use oximux_simulator::keyframe::FrameClock;

use super::{ScreenEvent, ScreenView};
use crate::shell::simulator::hub::{PanelStep, SharedRead, Tree};

/// How long a tap waits, from the press, for the element under it.
const READ_BUDGET: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Less movement than this (window pixels) is a tap.
const TAP_SLOP: f32 = 6.0;
/// How long the screen takes to show what an input did (a page sliding in).
const AFTER_INPUT: Duration = Duration::from_millis(600);
/// How long the screen is still before it is read.
const STILL: Duration = Duration::from_millis(250);
/// The longest a read waits for stillness: past it, the motion is likely
/// the stream's own refresh.
const STILL_CAP: Duration = Duration::from_millis(1500);
/// How long after a failed read the next one waits.
const RETRY: Duration = Duration::from_secs(2);
/// How often a recording looks whether the screen needs a read.
const POLL: Duration = Duration::from_millis(150);

/// The screen as last read while recording, and the user's last input.
#[derive(Default)]
struct Standing {
    /// The latest finished read: when it began, in which run of pictures,
    /// and what it read.
    tree: Option<(Instant, u64, Arc<Vec<ax::AxNode>>)>,
    /// A read under way: its id and start.
    reading: Option<(u64, Instant)>,
    failed_at: Option<Instant>,
    input_at: Option<Instant>,
}

impl Standing {
    /// The read to name a tap's element from: begun after the last input
    /// had shown, in this run of pictures (a gap may be another screen).
    fn usable(&self, epoch: u64) -> Option<&Arc<Vec<ax::AxNode>>> {
        let (started, read_epoch, nodes) = self.tree.as_ref()?;
        (*read_epoch == epoch && self.input_at.is_none_or(|input| *started >= input + AFTER_INPUT)).then_some(nodes)
    }

    /// Whether to read the screen now: once the last input has shown and
    /// the screen is still (or has moved too long to wait for), when there
    /// is no usable read or the screen moved since it began.
    fn wants_read(&self, clock: &FrameClock, now: Instant) -> bool {
        let ago = |t: Instant| now.saturating_duration_since(t);
        if self.reading.is_some() || self.failed_at.is_some_and(|t| ago(t) < RETRY) || self.input_at.is_some_and(|t| ago(t) < AFTER_INPUT) {
            return false;
        }
        let still = clock.last_motion_at.is_none_or(|motion| ago(motion) >= STILL);
        match (self.usable(clock.epoch), &self.tree) {
            (Some(_), Some((started, _, _))) => {
                let moved = clock.last_motion_at.is_some_and(|motion| motion > *started);
                moved && (still || ago(*started) >= STILL_CAP)
            }
            _ => still || self.input_at.is_none_or(|t| ago(t) >= STILL_CAP),
        }
    }
}

/// A press being recorded.
struct Press {
    pos: Point<Pixels>,
    /// Display-normalized.
    at: (f64, f64),
    started: Instant,
    /// The screen as it stood at the press, read before it.
    standing: Option<Arc<Vec<ax::AxNode>>>,
    tree: Option<SharedRead<Tree>>,
}

/// Recording state, for the life of the screen view.
#[derive(Default)]
pub(super) struct Recorder {
    on: bool,
    press: Option<Press>,
    /// Steps in the order they happened; a tap still reading its element
    /// holds back what came after it.
    queue: VecDeque<(u64, Slot)>,
    next: u64,
    standing: Standing,
    /// Whether the screen is being watched for rest.
    watching: bool,
}

/// A place in [`Recorder::queue`].
enum Slot {
    /// Still being worked out; what to keep if recording stops first (a
    /// tap as a point, when the screen's size is known).
    Waiting(Option<PanelStep>),
    /// Known (`None`: nothing to record after all).
    Done(Option<PanelStep>),
}

impl Recorder {
    /// Drop a press in progress (a forced release is no step).
    pub(super) fn cancel(&mut self) {
        self.press = None;
    }

    /// A place in line for a step still being worked out (`fallback`: what
    /// to keep if recording stops first).
    fn wait_turn(&mut self, fallback: Option<PanelStep>) -> u64 {
        self.next += 1;
        self.queue.push_back((self.next, Slot::Waiting(fallback)));
        self.next
    }

    /// The step for place `seq` (`None`: nothing to record after all).
    fn resolve(&mut self, seq: u64, step: Option<PanelStep>, cx: &mut Context<ScreenView>) {
        if let Some(slot) = self.queue.iter_mut().find(|(s, _)| *s == seq) {
            slot.1 = Slot::Done(step);
        }
        self.flush(cx);
    }

    /// A step known now, after any still being worked out.
    fn ready(&mut self, step: PanelStep, cx: &mut Context<ScreenView>) {
        self.next += 1;
        self.queue.push_back((self.next, Slot::Done(Some(step))));
        self.flush(cx);
    }

    fn flush(&mut self, cx: &mut Context<ScreenView>) {
        while let Some((_, Slot::Done(_))) = self.queue.front() {
            if let Some((_, Slot::Done(Some(step)))) = self.queue.pop_front()
                && self.on
            {
                cx.emit(ScreenEvent::Recorded(step));
            }
        }
    }

    /// How long until every tap still reading has its step: within the
    /// budget when it has a point to fall back on, else as long as the read
    /// itself may take (the read is what says the screen's size).
    fn settles_in(&self) -> Option<Duration> {
        let waiting = |pointless: bool| self.queue.iter().any(|(_, slot)| matches!(slot, Slot::Waiting(f) if f.is_none() || !pointless));
        let wait = if waiting(true) {
            READ_TIMEOUT
        } else if waiting(false) {
            READ_BUDGET
        } else {
            return None;
        };
        Some(wait + Duration::from_millis(100))
    }

    /// Everything still in line, in order, a tap still reading its element
    /// as its fallback: what a stop keeps.
    fn drain(&mut self) -> Vec<PanelStep> {
        self.queue
            .drain(..)
            .filter_map(|(_, slot)| match slot {
                Slot::Waiting(step) | Slot::Done(step) => step,
            })
            .collect()
    }
}

impl ScreenView {
    /// The panel started or stopped recording this device's input.
    pub(crate) fn set_recording(&mut self, on: bool, cx: &mut Context<Self>) {
        self.recorder.on = on;
        if !on {
            self.recorder.press = None;
            self.recorder.queue.clear();
            self.recorder.standing = Standing::default();
        } else if !self.recorder.watching {
            self.recorder.watching = true;
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(POLL).await;
                    if !matches!(this.update(cx, |view, cx| view.watch_screen(cx)), Ok(true)) {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// Keep a read of the screen as it stands: once it is still, unless the
    /// one in hand (or reading) began after it last moved. `false` once
    /// recording has stopped.
    fn watch_screen(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.recorder.on {
            self.recorder.watching = false;
            return false;
        }
        let (Some(device), Some(session)) = (self.binding.device.clone(), self.session(cx)) else { return true };
        let (clock, now) = (session.frame_clock(), Instant::now());
        if session.ios_device().is_some() || !self.recorder.standing.wants_read(&clock, now) {
            return true;
        }
        let read = self.hub.update(cx, |hub, cx| hub.read_tree(&device, &session, Some(now), READ_TIMEOUT, cx));
        self.recorder.next += 1;
        let (id, epoch) = (self.recorder.next, clock.epoch);
        self.recorder.standing.reading = Some((id, now));
        cx.spawn(async move |this, cx| {
            let nodes = read.await.ok();
            let _ = this.update(cx, |view, _| {
                let standing = &mut view.recorder.standing;
                if standing.reading.is_some_and(|(r, _)| r == id) {
                    standing.reading = None;
                    match nodes {
                        Some(nodes) => standing.tree = Some((now, epoch, nodes)),
                        None => standing.failed_at = Some(Instant::now()),
                    }
                }
            });
        })
        .detach();
        true
    }

    /// A finger went down at `at` (display-normalized).
    pub(super) fn rec_press(&mut self, pos: Point<Pixels>, at: (f64, f64), cx: &mut Context<Self>) {
        if !self.recorder.on {
            return;
        }
        let session = self.session(cx);
        let standing = session.as_ref().and_then(|s| self.recorder.standing.usable(s.frame_clock().epoch).cloned());
        self.rec_input();
        let tree = match (self.binding.device.clone(), session) {
            _ if standing.is_some() => None,
            // A read of this screen, not one already in flight for the last.
            (Some(device), Some(session)) if session.ios_device().is_none() => {
                let now = Instant::now();
                Some(self.hub.update(cx, |hub, cx| hub.read_tree(&device, &session, Some(now), READ_TIMEOUT, cx)))
            }
            _ => None,
        };
        self.recorder.press = Some(Press { pos, at, started: Instant::now(), standing, tree });
    }

    /// The finger lifted at `pos`: a tap or a swipe, recorded. A tap waits
    /// (within [`READ_BUDGET`] of the press) for the element under it; what
    /// is recorded meanwhile waits behind it, so steps keep their order.
    pub(super) fn rec_release(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(press) = self.recorder.press.take() else { return };
        let Some(device) = self.binding.device.clone() else { return };
        let moved = f32::from(pos.x - press.pos.x).hypot(f32::from(pos.y - press.pos.y));
        let swipe = if moved >= TAP_SLOP {
            let Some(end) = self.display_point(pos, true) else { return };
            let duration_ms = u32::try_from(press.started.elapsed().as_millis().clamp(50, 5000)).unwrap_or(300);
            Some((end, duration_ms))
        } else {
            None
        };
        // The screen in points: known once a scale is (an agent verb, an
        // earlier read), else from this press's read.
        let known = self.hub.read_with(cx, |hub, _| crate::shell::simulator::agent_ops::screen_points(hub, &device));
        let fallback = known.map(|screen @ (w, h)| {
            let to_pt = |(x, y): (f64, f64)| (x * w, y * h);
            match swipe {
                Some((end, duration_ms)) => PanelStep::Swipe { from: to_pt(press.at), to: to_pt(end), screen, duration_ms },
                None => PanelStep::Tap { at: to_pt(press.at), screen, target: None },
            }
        });
        let seq = self.recorder.wait_turn(fallback);
        let (left, read, standing) = (READ_BUDGET.saturating_sub(press.started.elapsed()), press.tree.clone(), press.standing.clone());
        let (hub, session) = (self.hub.clone(), self.session(cx));
        cx.spawn(async move |this, cx| {
            // A swipe on a known screen needs nothing read. A read past the
            // budget still gives a screen not known yet its size, never the
            // tap its element (the tap may have changed the screen by then).
            let (tree, late) = match read.filter(|_| swipe.is_none() || known.is_none()) {
                _ if standing.is_some() => (standing, false),
                Some(read) => {
                    let timer = Box::pin(cx.background_executor().timer(left));
                    match futures::future::select(read, timer).await {
                        futures::future::Either::Left((tree, _)) => (tree.ok(), false),
                        futures::future::Either::Right((_, read)) if known.is_none() => (read.await.ok(), true),
                        futures::future::Either::Right(_) => (None, false),
                    }
                }
                None => (None, false),
            };
            let _ = this.update(cx, |view, cx| {
                let read = match (tree, session) {
                    (Some(nodes), Some(session)) => {
                        super::inspector::mapping(&hub, &device, &session, session.orientation(), &nodes, cx).map(|map| (nodes, map))
                    }
                    _ => None,
                };
                let points = known.or_else(|| read.as_ref().map(|(_, map)| map.points())).or_else(|| view.cached_points(cx));
                let step = points.map(|screen @ (w, h)| {
                    let to_pt = |(x, y): (f64, f64)| (x * w, y * h);
                    match swipe {
                        Some((end, duration_ms)) => PanelStep::Swipe { from: to_pt(press.at), to: to_pt(end), screen, duration_ms },
                        None => {
                            let target = match &read {
                                _ if late => None,
                                Some((nodes, map)) => selector_at(nodes, press.at, &|n| map.to_screen(n.frame)),
                                // A real iPhone: the inspector's tree, when it has one.
                                None => view.target_in_cached_tree(press.at, cx),
                            };
                            PanelStep::Tap { at: to_pt(press.at), screen, target }
                        }
                    }
                });
                view.recorder.resolve(seq, step, cx);
            });
        })
        .detach();
    }

    /// A key typed while recording: a character, or a backspace.
    pub(super) fn rec_key(&mut self, key: &str, key_char: Option<&str>, cx: &mut Context<Self>) {
        if !self.recorder.on {
            return;
        }
        self.rec_input();
        let step = match key {
            "backspace" => PanelStep::Backspace,
            "enter" => PanelStep::Return,
            "tab" | "delete" | "up" | "down" | "left" | "right" | "home" | "end" => {
                PanelStep::Skipped("arrow, tab, delete, home and end keys".into())
            }
            _ => match key_char.filter(|c| !c.is_empty() && c.is_ascii() && !c.chars().any(char::is_control)) {
                Some(c) => PanelStep::Text(c.to_owned()),
                None => return,
            },
        };
        self.recorder.ready(step, cx);
    }

    /// Input that may change the screen, recorded or not (a wheel scroll, a
    /// pinch): the screen as read before it no longer names taps.
    pub(super) fn rec_input(&mut self) {
        self.recorder.standing.input_at = Some(Instant::now());
    }

    /// How long until every tap still reading its element has its step
    /// (`None`: none is reading).
    pub(crate) fn recording_settles_in(&self) -> Option<Duration> {
        self.recorder.settles_in()
    }

    /// Recording stops: the steps still in line, in order (a tap still
    /// reading its element as a point), handed over rather than dropped.
    pub(crate) fn drain_recording(&mut self) -> Vec<PanelStep> {
        self.recorder.drain()
    }

    /// A step from outside the screen (a toolbar button), in line with the
    /// screen's own.
    pub(crate) fn record_step(&mut self, step: PanelStep, cx: &mut Context<Self>) {
        if self.recorder.on {
            self.rec_input();
            self.recorder.ready(step, cx);
        }
    }

    /// Text pasted into the device while recording.
    pub(super) fn rec_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.recorder.on && !text.is_empty() {
            self.rec_input();
            self.recorder.ready(PanelStep::Text(text.to_owned()), cx);
        }
    }
}

/// The exact selector of the element under `pt`, and what typing there goes
/// into (`ax::field_kind`) — a password field when any password field on
/// screen holds the point (a tap on its hint is a tap on it).
pub(super) fn selector_at(
    nodes: &[ax::AxNode],
    pt: (f64, f64),
    rect_of: &dyn Fn(&ax::AxNode) -> oximux_simulator::geometry::Rect,
) -> Option<(SimSelectorWire, Option<bool>)> {
    let node = ax::hit_test(nodes, pt, rect_of)?;
    let sel = ax::exact_selector(nodes, node)?;
    let nth = u32::try_from(sel.nth).unwrap_or(u32::MAX);
    let inside = |n: &ax::AxNode| {
        let r = rect_of(n);
        (r.x..=r.x + r.w).contains(&pt.0) && (r.y..=r.y + r.h).contains(&pt.1)
    };
    let in_password = ax::flatten(nodes, usize::MAX).iter().any(|f| f.node.secure && ax::is_on_screen(f.node) && inside(f.node));
    let kind = if in_password { Some(true) } else { ax::field_kind(node) };
    Some((SimSelectorWire { label: sel.label, id: sel.id, role: sel.role, nth }, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_names_taps_until_the_next_input() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let clock = |motion: Option<u64>, epoch| FrameClock { epoch, last_motion_at: motion.map(ms), ..FrameClock::default() };
        let mut st = Standing { input_at: Some(t0), ..Standing::default() };
        assert!(!st.wants_read(&clock(None, 1), ms(300)), "the input is still showing");
        assert!(!st.wants_read(&clock(Some(500), 1), ms(700)), "the screen still moves");
        assert!(st.wants_read(&clock(Some(500), 1), ms(800)));
        assert!(st.wants_read(&clock(Some(1590), 1), ms(1600)), "moving too long to wait for");
        st.tree = Some((ms(800), 1, Arc::new(Vec::new())));
        assert!(st.usable(1).is_some());
        assert!(st.usable(2).is_none(), "a gap in the pictures");
        assert!(!st.wants_read(&clock(Some(700), 1), ms(1200)), "nothing moved since it began");
        assert!(!st.wants_read(&clock(Some(1100), 1), ms(1200)), "a refresh picture: wait for stillness");
        assert!(st.wants_read(&clock(Some(1100), 1), ms(1400)), "moved since, and still again");
        assert!(st.usable(1).is_some(), "motion alone keeps it usable");
        st.input_at = Some(ms(1500));
        assert!(st.usable(1).is_none(), "an input since it began");
        st.reading = Some((7, ms(2200)));
        assert!(!st.wants_read(&clock(None, 1), ms(2300)), "one read at a time");
    }

    #[test]
    fn a_stop_waits_for_a_tap_that_has_no_point_to_keep() {
        let mut rec = Recorder::default();
        assert_eq!(rec.settles_in(), None);
        let point = PanelStep::Tap { at: (1.0, 2.0), screen: (402.0, 874.0), target: None };
        let first = rec.wait_turn(Some(point));
        assert!(rec.settles_in().is_some_and(|d| d < READ_TIMEOUT), "a point to keep: the budget");
        let second = rec.wait_turn(None);
        assert!(rec.settles_in().is_some_and(|d| d > READ_TIMEOUT), "no point: the whole read");
        for seq in [first, second] {
            rec.queue.iter_mut().find(|(s, _)| *s == seq).unwrap().1 = Slot::Done(Some(PanelStep::Backspace));
        }
        assert_eq!(rec.settles_in(), None);
        assert_eq!(rec.drain().len(), 2);
    }
}
