//! Reading the accessibility tree for the control verbs, and moving between
//! its space and the agent's.
//!
//! Every read goes through the hub's per-device coalescer (one read in flight;
//! callers asking meanwhile share it). A tree's frames are in the tree's own
//! space — points that follow the app on iOS, display pixels on Android — and
//! an agent works in points in the screenshot's space: [`Shown`] maps a node
//! out to the wire, [`touch_point`] maps an element's centre back to a touch.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{SimAxNodeWire, SimErrorWire, SimPointWire};
use oximux_simulator::agent;
use oximux_simulator::ax::{self, AxNode, Query};
use oximux_simulator::geometry::{self, Size};
use oximux_simulator::session::StreamSession;
use oximux_simulator::{DeviceId, Orientation};

use crate::shell::simulator::hub::SimulatorHub;

/// Helper requests (screenshot, AX tree, rotation).
pub(super) const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a just-booted device may take to have an accessibility tree.
const AX_SETTLE: Duration = Duration::from_secs(8);

/// One read of the tree, joining a read in flight that started no earlier
/// than `not_before`. An empty tree is a tree (a screen in transition).
pub(super) async fn read_once(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    not_before: Option<Instant>,
    cx: &mut AsyncApp,
) -> Result<Arc<Vec<AxNode>>, SimErrorWire> {
    let read = hub.update(cx, |hub, cx| hub.read_tree(udid, session, not_before, HELPER_TIMEOUT, cx));
    read.await.map_err(|e| SimErrorWire::Failed(format!("could not read the accessibility tree: {e}")))
}

/// The tree, retried for a few seconds while it is not there yet: right after
/// a boot SpringBoard has no frontmost app to describe. Joins any read in
/// flight (a retry always starts a new one: the last has finished).
pub(super) async fn read(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, cx: &mut AsyncApp) -> Result<Arc<Vec<AxNode>>, SimErrorWire> {
    let deadline = Instant::now() + AX_SETTLE;
    loop {
        match read_once(hub, udid, session, None, cx).await {
            Ok(nodes) if !nodes.is_empty() => return Ok(nodes),
            _ if Instant::now() < deadline => cx.background_executor().timer(Duration::from_millis(500)).await,
            Ok(_) => return Err(SimErrorWire::Failed("the accessibility tree is empty".into())),
            Err(e) => return Err(e),
        }
    }
}

/// Orientation and portrait framebuffer size, in pixels.
pub(super) fn screen(session: &StreamSession) -> Result<(Orientation, Size), SimErrorWire> {
    let (w, h) = session.framebuffer_size().ok_or(SimErrorWire::NotStreaming)?;
    Ok((session.orientation(), Size::new(f64::from(w), f64::from(h))))
}

/// The AX root frame's size: the whole screen, in the app's orientation.
pub(super) fn root_size(nodes: &[AxNode]) -> Size {
    nodes.first().map_or(Size::new(0.0, 0.0), |n| Size::new(n.frame.w, n.frame.h))
}

/// Android's uiautomator reports frames in the display's space (the video's);
/// iOS's AX tree follows the app.
pub(super) fn ax_space(session: &StreamSession) -> agent::AxSpace {
    if session.android().is_some() { agent::AxSpace::Display } else { agent::AxSpace::App }
}

/// The tree's units per point: Android's are display pixels (its density
/// per dp), iOS's are points.
pub(super) fn units_per_point(session: &StreamSession) -> f64 {
    session.android().and_then(|a| a.density()).unwrap_or(1.0)
}

/// Pixels per point, read once per device from the AX root frame.
pub(super) async fn scale(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, cx: &mut AsyncApp) -> Result<f64, SimErrorWire> {
    if let Some(scale) = hub.read_with(cx, |hub, _| hub.scale(udid)).or_else(|| session.point_scale()) {
        return Ok(scale);
    }
    let nodes = read(hub, udid, session, cx).await?;
    scale_from(hub, udid, session, &nodes, cx)
}

/// [`scale`], from a tree already read.
pub(super) fn scale_from(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, nodes: &[AxNode], cx: &mut AsyncApp) -> Result<f64, SimErrorWire> {
    if let Some(scale) = hub.read_with(cx, |hub, _| hub.scale(udid)).or_else(|| session.point_scale()) {
        return Ok(scale);
    }
    let (o, portrait) = screen(session)?;
    let scale = agent::device_scale(geometry::display_size(o, portrait), root_size(nodes))
        .ok_or_else(|| SimErrorWire::Failed("could not work out the screen's scale from the accessibility tree".into()))?;
    hub.update(cx, |hub, _| hub.set_scale(udid, scale));
    Ok(scale)
}

pub(super) async fn point_to_portrait(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    at: (f64, f64),
    cx: &mut AsyncApp,
) -> Result<(f64, f64), SimErrorWire> {
    let scale = scale(hub, udid, session, cx).await?;
    let (o, portrait) = screen(session)?;
    agent::points_to_portrait(o, at, portrait, scale).ok_or_else(|| off_screen(at, o, portrait, scale))
}

pub(super) fn off_screen(at: (f64, f64), o: Orientation, portrait: Size, scale: f64) -> SimErrorWire {
    let pts = agent::display_points(o, portrait, scale);
    SimErrorWire::BadInput(format!(
        "({}, {}) is off the screen, which is {}×{} points (x right, y down, from the top-left)",
        at.0, at.1, pts.w, pts.h
    ))
}

/// The element `query` finds in a fresh read, and that read.
pub(super) async fn element(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    lookup: Lookup<'_>,
    what: &str,
    cx: &mut AsyncApp,
) -> Result<(AxNode, Arc<Vec<AxNode>>), SimErrorWire> {
    let nodes = read(hub, udid, session, cx).await?;
    let node = lookup.find(&nodes).cloned().ok_or_else(|| {
        SimErrorWire::NotFound(format!("no on-screen element matches “{what}” (list them with `oximux sim ax`)"))
    })?;
    Ok((node, nodes))
}

/// How a verb names its element: `--label`/`--id` (a label falls back to
/// a substring), or exactly (`--exact`, `--role`, `--nth`).
#[derive(Clone, Copy, Debug)]
pub(super) enum Lookup<'a> {
    Query(Query<'a>),
    Exact(&'a ax::Exact),
}

impl Lookup<'_> {
    /// The element it names, preferring one on screen (see [`ax::find`]).
    pub(super) fn find(self, nodes: &[AxNode]) -> Option<&AxNode> {
        match self {
            Self::Query(q) => ax::find(nodes, q),
            Self::Exact(e) => ax::find_exact(nodes, e),
        }
    }

}

/// The wire's exact selector as the tree's.
pub(super) fn exact_of(sel: &oximux_remote_proto::simulator::SimSelectorWire) -> ax::Exact {
    ax::Exact { label: sel.label.clone(), id: sel.id.clone(), role: sel.role.clone(), nth: sel.nth as usize }
}

/// What an exact selector names, in words.
pub(super) fn exact_name(sel: &oximux_remote_proto::simulator::SimSelectorWire) -> String {
    let what = match (&sel.label, &sel.id, &sel.role) {
        (Some(label), _, _) => label.clone(),
        (_, Some(id), _) => format!("#{id}"),
        (_, _, Some(role)) => format!("a {role}"),
        _ => "nothing".into(),
    };
    if sel.nth > 0 { format!("{what} (match {})", sel.nth) } else { what }
}

/// The centre of `node` (from the read `nodes`) as a touch coordinate.
pub(super) fn touch_point(session: &StreamSession, nodes: &[AxNode], node: &AxNode) -> (f64, f64) {
    agent::ax_point_to_portrait(ax_space(session), session.orientation(), root_size(nodes), ax::center(node))
}

/// How a read's frames map into the screenshot's space, in points.
pub(super) struct Shown {
    space: agent::AxSpace,
    o: Orientation,
    root: Size,
    pts: Size,
}

impl Shown {
    pub(super) fn new(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, nodes: &[AxNode], cx: &mut AsyncApp) -> Result<Self, SimErrorWire> {
        let scale = scale_from(hub, udid, session, nodes, cx)?;
        let (o, portrait) = screen(session)?;
        Ok(Self { space: ax_space(session), o, root: root_size(nodes), pts: agent::display_points(o, portrait, scale) })
    }

    pub(super) fn rect(&self, r: geometry::Rect) -> geometry::Rect {
        agent::ax_rect_to_display(self.space, self.o, self.root, r, self.pts)
    }

    pub(super) fn wire(&self, node: &AxNode, depth: usize) -> SimAxNodeWire {
        let frame = self.rect(node.frame);
        SimAxNodeWire {
            depth: depth as u32,
            role: node.role.clone(),
            label: node.label.clone(),
            identifier: node.identifier.clone(),
            value: node.value.clone(),
            enabled: node.enabled,
            frame: [frame.x, frame.y, frame.w, frame.h],
        }
    }

    /// `node`'s centre, where a tap on it lands.
    pub(super) fn center(&self, node: &AxNode) -> SimPointWire {
        let (x, y) = self.rect(node.frame).center();
        SimPointWire { x, y }
    }
}
