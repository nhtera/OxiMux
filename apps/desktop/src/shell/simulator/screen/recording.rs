//! The user's input while the panel records a flow: each tap, swipe and
//! typed character as a step the panel keeps (see `hub::flow_recorder`).
//!
//! A tap names its element when the tree can say which one in time: a read
//! starts as the finger goes down (a simulator answers in milliseconds) and
//! gets [`READ_BUDGET`] from there, else the tap is kept as a point. A real
//! iPhone is not read for it (its runner does one job at a time): the
//! inspector's last tree serves, when there is one.

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
}

impl Recorder {
    /// Drop a press in progress (a forced release is no step).
    pub(super) fn cancel(&mut self) {
        self.press = None;
    }
}

impl ScreenView {
    /// The panel started or stopped recording this device's input.
    pub(crate) fn set_recording(&mut self, on: bool) {
        self.recorder.on = on;
        if !on {
            self.recorder.press = None;
        }
    }

    /// A finger went down at `at` (display-normalized).
    pub(super) fn rec_press(&mut self, pos: Point<Pixels>, at: (f64, f64), cx: &mut Context<Self>) {
        if !self.recorder.on {
            return;
        }
        let tree = match (self.binding.device.clone(), self.session(cx)) {
            (Some(device), Some(session)) if session.ios_device().is_none() => {
                Some(self.hub.update(cx, |hub, cx| hub.read_tree(&device, &session, None, READ_TIMEOUT, cx)))
            }
            _ => None,
        };
        self.recorder.press = Some(Press { pos, at, started: Instant::now(), tree });
    }

    /// The finger lifted at `pos`: a tap or a swipe, recorded.
    pub(super) fn rec_release(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(press) = self.recorder.press.take() else { return };
        let Some(device) = self.binding.device.clone() else { return };
        let Some(points) = self.hub.read_with(cx, |hub, _| crate::shell::simulator::agent_ops::screen_points(hub, &device)) else { return };
        let to_pt = |(x, y): (f64, f64)| (x * points.0, y * points.1);
        let moved = f32::from(pos.x - press.pos.x).hypot(f32::from(pos.y - press.pos.y));
        if moved >= TAP_SLOP {
            let Some(end) = self.display_point(pos, true) else { return };
            let duration_ms = u32::try_from(press.started.elapsed().as_millis().clamp(50, 5000)).unwrap_or(300);
            cx.emit(ScreenEvent::Recorded(PanelStep::Swipe { from: to_pt(press.at), to: to_pt(end), duration_ms }));
            return;
        }
        let at = to_pt(press.at);
        let Some(read) = press.tree else {
            // A real iPhone: the inspector's tree, when it has one.
            let target = self.target_in_cached_tree(press.at, cx);
            cx.emit(ScreenEvent::Recorded(PanelStep::Tap { at, target }));
            return;
        };
        let left = READ_BUDGET.saturating_sub(press.started.elapsed());
        let (hub, session) = (self.hub.clone(), self.session(cx));
        cx.spawn(async move |this, cx| {
            let timer = Box::pin(cx.background_executor().timer(left));
            let tree = match futures::future::select(read, timer).await {
                futures::future::Either::Left((tree, _)) => tree.ok(),
                futures::future::Either::Right(_) => None,
            };
            let _ = this.update(cx, |_, cx| {
                let target = match (tree, session) {
                    (Some(nodes), Some(session)) => {
                        let map = super::inspector::mapping(&hub, &device, &session, session.orientation(), &nodes, cx);
                        map.and_then(|map| selector_at(&nodes, press.at, &|n| map.to_screen(n.frame)))
                    }
                    _ => None,
                };
                cx.emit(ScreenEvent::Recorded(PanelStep::Tap { at, target }));
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
            "enter" => PanelStep::Text("\n".into()),
            _ => match key_char.filter(|c| !c.is_empty() && c.is_ascii() && !c.chars().any(char::is_control)) {
                Some(c) => PanelStep::Text(c.to_owned()),
                None => return,
            },
        };
        cx.emit(ScreenEvent::Recorded(step));
    }

    /// Text pasted into the device while recording.
    pub(super) fn rec_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.recorder.on && !text.is_empty() {
            cx.emit(ScreenEvent::Recorded(PanelStep::Text(text.to_owned())));
        }
    }
}

/// The exact selector of the element under `pt`, and whether it is a
/// password field.
pub(super) fn selector_at(nodes: &[ax::AxNode], pt: (f64, f64), rect_of: &dyn Fn(&ax::AxNode) -> oximux_simulator::geometry::Rect) -> Option<(SimSelectorWire, bool)> {
    let node = ax::hit_test(nodes, pt, rect_of)?;
    let sel = ax::exact_selector(nodes, node)?;
    let nth = u32::try_from(sel.nth).unwrap_or(u32::MAX);
    Some((SimSelectorWire { label: sel.label, id: sel.id, role: sel.role, nth }, node.secure))
}
