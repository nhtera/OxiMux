//! The element inspector: hold ⌥ over the screen to outline the element
//! under the pointer; an ⌥-click that does not move picks it (the panel then
//! offers Send to agent and Copy selector). An ⌥-drag still pinches.
//!
//! The tree is read only while ⌥ is held, through the device's one-at-a-time
//! read (`hub::snapshot`), and kept a moment ([`FRESH`]) so moving the
//! pointer re-tests the same tree without reading again. Frames are tested in
//! the screen's own space (display-normalized, the video's), whichever way the
//! app or the device is turned.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyElement, Context, IntoElement, ParentElement as _, Pixels, Point, Styled as _, Task, Window, div, px};
use oximux_simulator::agent::{self, AxSpace};
use oximux_simulator::ax::{self, AxNode, Selector};
use oximux_simulator::geometry::{self, Rect, Size};
use oximux_simulator::{DeviceId, Orientation};

use super::ScreenView;

/// A tree this young is tested again rather than read again (a simulator
/// reads in milliseconds; a phone's or an emulator's takes a second or two).
const FRESH: Duration = Duration::from_secs(1);
const FRESH_SLOW: Duration = Duration::from_secs(2);
/// Less movement than this between press and release is a click (a pick).
pub(super) const CLICK_SLOP: f32 = 4.0;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// While ⌥ is held, how often its release is looked for: a release with the
/// pointer still reaches no listener unless the screen has focus.
const RELEASE_POLL: Duration = Duration::from_millis(100);

/// An element the user picked (or points at).
#[derive(Clone, Debug)]
pub(crate) struct Picked {
    pub device: DeviceId,
    /// The node, without its children.
    pub node: AxNode,
    /// Its frame as fractions of the screen (the video's space).
    pub rect: Rect,
    /// Its frame in points, as `oximux sim` verbs take them.
    pub points: Rect,
    /// `--id` or `--label` when either names it alone.
    pub selector: Option<Selector>,
}

impl Picked {
    /// The `oximux sim tap` that reaches it.
    pub(crate) fn tap_command(&self) -> String {
        match &self.selector {
            Some(Selector::Id(id)) => format!("oximux sim tap --id {}", shell_word(id)),
            Some(Selector::Label(label)) => format!("oximux sim tap --label {}", shell_word(label)),
            None => format!("oximux sim tap {:.0} {:.0}", self.points.x + self.points.w / 2.0, self.points.y + self.points.h / 2.0),
        }
    }

    /// "Button “Sign in”".
    pub(crate) fn title(&self) -> String {
        match self.node.label.as_deref().filter(|l| !l.is_empty()) {
            Some(label) => format!("{} “{label}”", self.node.role),
            None => self.node.role.clone(),
        }
    }
}

/// `s` as one shell word.
fn shell_word(s: &str) -> String {
    let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'));
    if plain { s.to_owned() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// How the tree's frames map to the screen.
#[derive(Clone, Copy, Debug)]
struct Mapping {
    space: AxSpace,
    orientation: Orientation,
    root: Size,
    /// The screen in points (current orientation).
    points: Size,
}

impl Mapping {
    fn to_screen(self, r: Rect) -> Rect {
        agent::ax_rect_to_display(self.space, self.orientation, self.root, r, Size::new(1.0, 1.0))
    }

    fn to_points(self, r: Rect) -> Rect {
        agent::ax_rect_to_display(self.space, self.orientation, self.root, r, self.points)
    }
}

struct Tree {
    device: DeviceId,
    read_at: Instant,
    nodes: Arc<Vec<AxNode>>,
    map: Mapping,
}

/// The inspector's state, for the life of the screen view.
#[derive(Default)]
pub(super) struct Inspector {
    /// ⌥ is held over the screen.
    armed: bool,
    tree: Option<Tree>,
    reading: bool,
    /// Where the pointer is (display-normalized), for a tree that lands.
    pointer: Option<(f64, f64)>,
    hover: Option<Picked>,
    /// The device has no accessibility tree (or the read failed): why.
    unavailable: Option<String>,
    /// An ⌥-press not yet a click or a drag: where it went down.
    pub(super) press: Option<Point<Pixels>>,
    /// Looks for ⌥'s release while armed.
    _release: Option<Task<()>>,
}

impl Inspector {
    pub(super) fn armed(&self) -> bool {
        self.armed
    }
}

impl ScreenView {
    /// The pointer moved, or ⌥ changed, at `pos`.
    pub(super) fn inspect(&mut self, pos: Point<Pixels>, alt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let point = self.display_point(pos, false);
        if !alt || point.is_none() {
            self.disarm(cx);
            return;
        }
        let Some(device) = self.binding.device.clone() else { return };
        if !self.inspector.armed {
            self.inspector._release = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor().timer(RELEASE_POLL).await;
                    let held = this.update_in(cx, |view, window, cx| {
                        let held = window.modifiers().alt;
                        if !held {
                            view.disarm(cx);
                        }
                        held
                    });
                    if !matches!(held, Ok(true)) {
                        return;
                    }
                }
            }));
        }
        self.inspector.armed = true;
        self.inspector.pointer = point;
        if !self.hub.read(cx).caps(&device).ax {
            self.inspector.unavailable = Some("No accessibility tree on this device".into());
            self.inspector.hover = None;
            return cx.notify();
        }
        self.refresh_tree(&device, cx);
        self.retest(cx);
    }

    /// ⌥ released (or the pointer left the screen): no outline.
    fn disarm(&mut self, cx: &mut Context<Self>) {
        if self.inspector.armed {
            self.inspector.armed = false;
            self.inspector.hover = None;
            self.inspector.press = None;
            self.inspector._release = None;
            cx.notify();
        }
    }

    /// Read the tree when the one in hand is stale (or another device's, or
    /// from another orientation).
    fn refresh_tree(&mut self, device: &DeviceId, cx: &mut Context<Self>) {
        let Some(session) = self.session(cx) else { return };
        let fresh = if session.android().is_some() || session.ios_device().is_some() { FRESH_SLOW } else { FRESH };
        let current = self.inspector.tree.as_ref().is_some_and(|t| {
            &t.device == device && t.read_at.elapsed() < fresh && t.map.orientation == session.orientation()
        });
        if current || self.inspector.reading {
            return;
        }
        self.inspector.reading = true;
        let read = self.hub.update(cx, |hub, cx| hub.read_tree(device, &session, None, READ_TIMEOUT, cx));
        let (device, hub) = (device.clone(), self.hub.clone());
        cx.spawn(async move |this, cx| {
            let tree = read.await;
            let _ = this.update(cx, |view, cx| {
                view.inspector.reading = false;
                // A tree for a device switched away from is dropped.
                if view.binding.device.as_ref() != Some(&device) {
                    return;
                }
                match tree {
                    Ok(nodes) => match view.session(cx).and_then(|s| mapping(&hub, &device, &s, &nodes, cx)) {
                        Some(map) => {
                            view.inspector.unavailable = None;
                            view.inspector.tree = Some(Tree { device, read_at: Instant::now(), nodes, map });
                        }
                        None => view.inspector.unavailable = Some("Could not place the elements on the screen".into()),
                    },
                    Err(e) => view.inspector.unavailable = Some(format!("Could not read the screen's elements: {e}")),
                }
                view.retest(cx);
            });
        })
        .detach();
    }

    /// Hit-test the tree in hand at the pointer.
    fn retest(&mut self, cx: &mut Context<Self>) {
        let hover = match (&self.inspector.tree, self.inspector.pointer, self.inspector.armed) {
            (Some(tree), Some(pt), true) => picked_at(tree, pt),
            _ => None,
        };
        let changed = hover.as_ref().map(|h| h.rect) != self.inspector.hover.as_ref().map(|h| h.rect);
        self.inspector.hover = hover;
        if changed {
            cx.notify();
        }
    }

    /// The element under `pos` now (an ⌥-click's pick).
    pub(super) fn pick_at(&mut self, pos: Point<Pixels>) -> Option<Picked> {
        let pt = self.display_point(pos, false)?;
        picked_at(self.inspector.tree.as_ref()?, pt)
    }

    /// The outline and its chip, over the screen while ⌥ is held.
    pub(super) fn render_inspector(&self) -> Option<AnyElement> {
        if !self.inspector.armed {
            return None;
        }
        let (theme, ty) = (self.theme, &self.typography);
        let chip = |text: String| {
            div()
                .px(px(6.))
                .py(px(2.))
                .rounded(px(self.density.r_chip))
                .bg(theme.bg_overlay)
                .text_size(px(ty.t_body_sm))
                .text_color(theme.fg_base)
                .whitespace_nowrap()
                .child(text)
        };
        let Some(hover) = &self.inspector.hover else {
            let text = match (&self.inspector.unavailable, self.inspector.reading) {
                (Some(why), _) => why.clone(),
                (None, true) => "Reading the screen…".into(),
                (None, false) => "No element here".into(),
            };
            return Some(div().absolute().top(px(8.)).left_0().right_0().flex().justify_center().child(chip(text)).into_any_element());
        };
        let fit = self.frame_fit()?;
        let r = Rect::new(fit.x + hover.rect.x * fit.w, fit.y + hover.rect.y * fit.h, hover.rect.w * fit.w, hover.rect.h * fit.h);
        let mut label = hover.title();
        if let Some(Selector::Id(id)) = &hover.selector {
            label.push_str(&format!(" · {id}"));
        }
        // Above the outline, or below it near the top.
        let above = r.y > 28.0;
        let outline = div()
            .absolute()
            .left(px(r.x as f32))
            .top(px(r.y as f32))
            .w(px(r.w as f32))
            .h(px(r.h as f32))
            .border_2()
            .border_color(theme.status_info)
            .rounded(px(2.));
        let chip_top = if above { r.y - 24.0 } else { r.y + r.h + 4.0 };
        let chip_row = div()
            .absolute()
            .left(px(fit.x as f32 + 4.0))
            .right(px(4.0))
            .top(px(chip_top as f32))
            .flex()
            .justify_center()
            .child(chip(label));
        Some(div().absolute().inset_0().child(outline).child(chip_row).into_any_element())
    }
}

/// The element under `pt` (display-normalized), with its selector.
fn picked_at(tree: &Tree, pt: (f64, f64)) -> Option<Picked> {
    let node = ax::hit_test(&tree.nodes, pt, &|n| tree.map.to_screen(n.frame))?;
    Some(Picked {
        device: tree.device.clone(),
        node: AxNode { children: Vec::new(), ..node.clone() },
        rect: tree.map.to_screen(node.frame),
        points: tree.map.to_points(node.frame),
        selector: ax::unique_selector(&tree.nodes, node),
    })
}

/// How `nodes`' frames map onto `session`'s screen; `None` before its size
/// or scale is known.
fn mapping(
    hub: &gpui::Entity<crate::shell::simulator::hub::SimulatorHub>,
    device: &DeviceId,
    session: &oximux_simulator::session::StreamSession,
    nodes: &[AxNode],
    cx: &gpui::App,
) -> Option<Mapping> {
    let root = nodes.first().map(|n| Size::new(n.frame.w, n.frame.h)).filter(|s| s.w > 0.0 && s.h > 0.0)?;
    let (w, h) = session.framebuffer_size()?;
    let (orientation, portrait) = (session.orientation(), Size::new(f64::from(w), f64::from(h)));
    let scale = hub
        .read(cx)
        .scale(device)
        .or_else(|| session.point_scale())
        .or_else(|| agent::device_scale(geometry::display_size(orientation, portrait), root))?;
    let space = if session.android().is_some() { AxSpace::Display } else { AxSpace::App };
    Some(Mapping { space, orientation, root, points: agent::display_points(orientation, portrait, scale) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(label: Option<&str>, selector: Option<Selector>) -> Picked {
        Picked {
            device: DeviceId("U".into()),
            node: AxNode {
                label: label.map(Into::into),
                identifier: None,
                value: None,
                role: "Button".into(),
                role_description: String::new(),
                enabled: true,
                frame: Rect::new(0.0, 0.0, 1.0, 1.0),
                children: vec![],
            },
            rect: Rect::new(0.1, 0.2, 0.3, 0.1),
            points: Rect::new(120.0, 600.0, 162.0, 44.0),
            selector,
        }
    }

    #[test]
    fn a_pick_reads_as_a_command_that_reaches_it() {
        assert_eq!(picked(Some("Sign in"), Some(Selector::Id("login_btn".into()))).tap_command(), "oximux sim tap --id login_btn");
        assert_eq!(picked(Some("Sign in"), Some(Selector::Label("Sign in".into()))).tap_command(), "oximux sim tap --label 'Sign in'");
        assert_eq!(picked(None, None).tap_command(), "oximux sim tap 201 622", "the centre, in points");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(picked(Some("Sign in"), None).title(), "Button “Sign in”");
        assert_eq!(picked(None, None).title(), "Button");
    }
}
