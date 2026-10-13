//! The user's input while the panel records a flow: each tap, swipe and
//! typed character as a step the panel keeps (see `hub::flow_recorder`).
//!
//! A tap names its element when the tree can say which one in time: a read
//! starts as the finger goes down (a simulator answers in milliseconds) and
//! gets [`READ_BUDGET`] from there, else the tap is kept as a point. A real
//! iPhone is not read for it (its runner does one job at a time): the
//! inspector's last tree serves, when there is one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui::{Context, Pixels, Point};
use oximux_remote_proto::simulator::SimSelectorWire;
use oximux_simulator::ax;

use super::{ScreenEvent, ScreenView};
use crate::shell::simulator::hub::{PanelStep, SharedRead, Tree};

/// How long a tap waits, from the press, for the element under it.
const READ_BUDGET: Duration = Duration::from_millis(300);
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Less movement than this (window pixels) is a tap.
const TAP_SLOP: f32 = 6.0;

/// A press being recorded.
struct Press {
    pos: Point<Pixels>,
    /// Display-normalized.
    at: (f64, f64),
    started: Instant,
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

    /// Whether a tap is still reading its element.
    fn reading(&self) -> bool {
        self.queue.iter().any(|(_, slot)| matches!(slot, Slot::Waiting(_)))
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
    pub(crate) fn set_recording(&mut self, on: bool) {
        self.recorder.on = on;
        if !on {
            self.recorder.press = None;
            self.recorder.queue.clear();
        }
    }

    /// A finger went down at `at` (display-normalized).
    pub(super) fn rec_press(&mut self, pos: Point<Pixels>, at: (f64, f64), cx: &mut Context<Self>) {
        if !self.recorder.on {
            return;
        }
        let tree = match (self.binding.device.clone(), self.session(cx)) {
            // A read of this screen, not one already in flight for the last.
            (Some(device), Some(session)) if session.ios_device().is_none() => {
                let now = Instant::now();
                Some(self.hub.update(cx, |hub, cx| hub.read_tree(&device, &session, Some(now), READ_TIMEOUT, cx)))
            }
            _ => None,
        };
        self.recorder.press = Some(Press { pos, at, started: Instant::now(), tree });
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
        let (left, read) = (READ_BUDGET.saturating_sub(press.started.elapsed()), press.tree.clone());
        let (hub, session) = (self.hub.clone(), self.session(cx));
        cx.spawn(async move |this, cx| {
            // A swipe on a known screen needs nothing read.
            let tree = match read.filter(|_| swipe.is_none() || known.is_none()) {
                Some(read) => {
                    let timer = Box::pin(cx.background_executor().timer(left));
                    match futures::future::select(read, timer).await {
                        futures::future::Either::Left((tree, _)) => tree.ok(),
                        futures::future::Either::Right(_) => None,
                    }
                }
                None => None,
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

    /// How long until every tap still reading its element has its step
    /// (`None`: none is reading).
    pub(crate) fn recording_settles_in(&self) -> Option<Duration> {
        self.recorder.reading().then(|| READ_BUDGET + Duration::from_millis(100))
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
            self.recorder.ready(step, cx);
        }
    }

    /// Text pasted into the device while recording.
    pub(super) fn rec_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.recorder.on && !text.is_empty() {
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
    fn a_stop_waits_for_a_tap_that_has_no_point_to_keep() {
        let mut rec = Recorder::default();
        assert!(!rec.reading());
        let seq = rec.wait_turn(None);
        assert!(rec.reading(), "a tap with no fallback is still reading");
        rec.queue.iter_mut().find(|(s, _)| *s == seq).unwrap().1 = Slot::Done(Some(PanelStep::Backspace));
        assert!(!rec.reading());
        assert_eq!(rec.drain().len(), 1);
    }
}
