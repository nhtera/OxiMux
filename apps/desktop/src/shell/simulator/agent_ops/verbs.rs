//! The control verbs, once consent is settled: screenshots, the AX tree,
//! touches, keys, buttons, rotation, and the `simctl` app verbs.
//!
//! Touch, keys, the AX tree and screenshots go through the device's helper,
//! so they need its stream; an agent does not need the panel open for that —
//! [`live_session`] wakes a parked or never-started device the way the
//! user's Reconnect would, and waits a moment for it. The app verbs
//! (launch, open-url, install) are `simctl` — `adb` on Android (see
//! [`super::android`]) — but wake the device the same way: an attached device
//! may be shut down, and `simctl` cannot boot it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{
    SimAxNodeWire, SimButtonWire, SimCmdWire, SimErrorWire, SimOrientationWire, SimReplyWire, SimTargetWire,
};
use oximux_simulator::agent::{self, InstallPathError};
use oximux_simulator::ax::{self, AxNode, Query};
use oximux_simulator::geometry::{self, Size};
use oximux_simulator::protocol::{Command, KeyPhase, TouchPhase};
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::StreamSession;
use oximux_simulator::{Button, DeviceId, Orientation, Platform, keyboard, simctl};

use super::Target;
use crate::shell::simulator::hub::{SimulatorHub, Wake, home_button, paste_now};

/// How long a woken device may take to start streaming: a cold boot of a
/// shut-down device measured 40-odd seconds on an M-series Mac.
const START_WAIT: Duration = Duration::from_secs(60);
/// Helper requests (screenshot, AX tree, rotation).
const HELPER_TIMEOUT: Duration = Duration::from_secs(10);
/// `simctl launch` / `openurl`.
const APP_TIMEOUT: Duration = Duration::from_secs(60);
/// `simctl install` (a large app copies for a while).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);
/// Finger down to finger up, for a tap.
const TAP_HOLD: Duration = Duration::from_millis(60);
/// How long a just-booted device may take to have an accessibility tree.
const AX_SETTLE: Duration = Duration::from_secs(8);
/// The longest text one `type` takes (it goes key by key).
const MAX_TYPED: usize = 10_000;

/// Run a blocking call on a thread of its own: an install copies for minutes,
/// and the background executor's few threads are shared by the whole app.
async fn on_own_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, SimErrorWire> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.map_err(|_| SimErrorWire::Failed("the operation stopped unexpectedly".into()))
}

type Out = Result<SimReplyWire, SimErrorWire>;

pub(super) async fn run(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    name: &str,
    cmd: SimCmdWire,
    target: &Target,
    cx: &mut AsyncApp,
) -> Out {
    // Input takes turns per device: interleaved touch streams from parallel
    // calls would make gestures nobody asked for.
    let input = matches!(cmd, SimCmdWire::Tap(_) | SimCmdWire::Swipe { .. } | SimCmdWire::Type { .. } | SimCmdWire::Button(_));
    let lock = input.then(|| hub.update(cx, |hub, _| hub.input_lock(udid)));
    let _turn = match &lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    match cmd {
        // Answered before consent in `agent_ops::run`.
        SimCmdWire::Status | SimCmdWire::Devices | SimCmdWire::Attach { .. } | SimCmdWire::Detach => Ok(SimReplyWire::Done),
        SimCmdWire::Screenshot { full } => screenshot(hub, udid, full, cx).await,
        SimCmdWire::Ax { max } => {
            let session = live_session(hub, udid, cx).await?;
            let nodes = describe(&session, cx).await?;
            let scale = scale_from(hub, udid, &session, &nodes, cx)?;
            // Frames in the screenshot's space, whichever way the app is turned.
            let (o, portrait) = screen(&session)?;
            let (root, pts) = (root_size(&nodes), agent::display_points(o, portrait, scale));
            let flat = ax::flatten(&nodes, max.clamp(1, 5000) as usize);
            let shown = |r| agent::ax_rect_to_display(o, root, r, pts);
            Ok(SimReplyWire::Ax(flat.iter().map(|f| ax_wire(f.node, f.depth, shown(f.node.frame))).collect()))
        }
        SimCmdWire::Tap(aim) => {
            let session = live_session(hub, udid, cx).await?;
            let at = match aim {
                SimTargetWire::Point(p) => point_to_portrait(hub, udid, &session, (p.x, p.y), cx).await?,
                SimTargetWire::Label(label) => element(&session, Query::Label(&label), &label, cx).await?,
                SimTargetWire::Id(id) => element(&session, Query::Id(&id), &id, cx).await?,
            };
            send(&session, Command::Touch { phase: TouchPhase::Begin, x: at.0, y: at.1, edge: 0 })?;
            cx.background_executor().timer(TAP_HOLD).await;
            send(&session, Command::Touch { phase: TouchPhase::End, x: at.0, y: at.1, edge: 0 })?;
            Ok(SimReplyWire::Done)
        }
        SimCmdWire::Swipe { from, to, duration_ms } => {
            let session = live_session(hub, udid, cx).await?;
            let scale = scale(hub, udid, &session, cx).await?;
            let (o, portrait) = screen(&session)?;
            let map = |(x, y): (f64, f64)| {
                agent::points_to_portrait(o, (x, y), portrait, scale).ok_or_else(|| off_screen((x, y), o, portrait, scale))
            };
            let start = map((from.x, from.y))?;
            map((to.x, to.y))?;
            // A swipe that starts in the home-indicator band is the system's
            // edge gesture, and keeps that edge for its whole length.
            let pts = agent::display_points(o, portrait, scale);
            let edge = geometry::edge_for(o, (from.x / pts.w, from.y / pts.h)).unwrap_or(0);
            let duration = Duration::from_millis(u64::from(duration_ms.clamp(50, 5000)));
            let path = agent::swipe_path((from.x, from.y), (to.x, to.y), duration);
            let step = duration / path.len() as u32;
            send(&session, Command::Touch { phase: TouchPhase::Begin, x: start.0, y: start.1, edge })?;
            let mut last = start;
            for p in path {
                cx.background_executor().timer(step).await;
                last = map(p)?;
                send(&session, Command::Touch { phase: TouchPhase::Move, x: last.0, y: last.1, edge })?;
            }
            send(&session, Command::Touch { phase: TouchPhase::End, x: last.0, y: last.1, edge })?;
            Ok(SimReplyWire::Done)
        }
        SimCmdWire::Type { text, paste } => {
            if text.is_empty() {
                return Err(SimErrorWire::BadInput("nothing to type".into()));
            }
            if text.chars().count() > MAX_TYPED {
                return Err(SimErrorWire::BadInput(format!("at most {MAX_TYPED} characters at a time")));
            }
            let session = live_session(hub, udid, cx).await?;
            // Android takes text as text, any Unicode.
            if let Some(android) = session.android().cloned() {
                return on_own_thread(move || android.type_text(&text))
                    .await?
                    .map(|()| SimReplyWire::Done)
                    .map_err(|e| SimErrorWire::Failed(format!("typing failed: {e}")));
            }
            if paste || keyboard::needs_paste(&text) {
                let lock = hub.read_with(cx, |hub, _| hub.paste_lock());
                let udid = udid.clone();
                return on_own_thread(move || paste_now(&session, &udid, &text, &lock))
                    .await?
                    .map(|()| SimReplyWire::Done)
                    .map_err(|e| SimErrorWire::Failed(format!("paste failed: {e}")));
            }
            let keys = keyboard::text_to_key_events(&text).map_err(|e| SimErrorWire::BadInput(e.to_string()))?;
            for key in keys {
                let phase = if key.down { KeyPhase::Down } else { KeyPhase::Up };
                send(&session, Command::Key { phase, usage: key.usage })?;
            }
            Ok(SimReplyWire::Done)
        }
        SimCmdWire::Button(button @ (SimButtonWire::Back | SimButtonWire::VolumeUp | SimButtonWire::VolumeDown)) => {
            use oximux_simulator::android::input::AndroidButton;
            if udid.platform() != Platform::Android {
                return Err(SimErrorWire::BadInput("that button exists only on Android".into()));
            }
            let session = live_session(hub, udid, cx).await?;
            let button = match button {
                SimButtonWire::Back => AndroidButton::Back,
                SimButtonWire::VolumeUp => AndroidButton::VolumeUp,
                _ => AndroidButton::VolumeDown,
            };
            session.press_android(button).map(|()| SimReplyWire::Done).map_err(|e| SimErrorWire::Failed(format!("the device did not take the button: {e}")))
        }
        SimCmdWire::Button(button) => {
            let session = live_session(hub, udid, cx).await?;
            let name = match button {
                SimButtonWire::Home => home_button(name),
                SimButtonWire::Lock => Button::Lock,
                SimButtonWire::Siri => Button::Siri,
                SimButtonWire::SideButton => Button::SideButton,
                SimButtonWire::AppSwitcher => Button::AppSwitcher,
                SimButtonWire::Back | SimButtonWire::VolumeUp | SimButtonWire::VolumeDown => unreachable!("matched above"),
            };
            send(&session, Command::Button { name })?;
            Ok(SimReplyWire::Done)
        }
        SimCmdWire::Rotate(to) => {
            let session = live_session(hub, udid, cx).await?;
            let orientation = match to {
                SimOrientationWire::Portrait => Orientation::Portrait,
                SimOrientationWire::LandscapeLeft => Orientation::LandscapeLeft,
                SimOrientationWire::LandscapeRight => Orientation::LandscapeRight,
                SimOrientationWire::UpsideDown => Orientation::PortraitUpsideDown,
            };
            cx.background_executor()
                .spawn(async move { session.configure(None, None, Some(orientation), HELPER_TIMEOUT) })
                .await
                .map(|()| SimReplyWire::Done)
                .map_err(|e| SimErrorWire::Failed(format!("rotation failed: {e}")))
        }
        SimCmdWire::Launch { bundle_id, relaunch } if udid.platform() == Platform::Android => {
            let device = android_device(hub, udid, cx).await?;
            on_own_thread(move || device.launch(&bundle_id, relaunch)).await?.map(|()| SimReplyWire::Done)
        }
        SimCmdWire::Launch { bundle_id, relaunch } => {
            let well_formed = bundle_id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                && bundle_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            if !well_formed {
                return Err(SimErrorWire::BadInput(format!("`{bundle_id}` is not a bundle identifier")));
            }
            let udid = simctl_ready(hub, udid, cx).await?;
            on_own_thread(move || {
                if relaunch {
                    // Not running is fine: launching is what matters.
                    let _ = simctl::terminate(&SystemRunner, udid.as_str(), &bundle_id, APP_TIMEOUT);
                }
                simctl::launch(&SystemRunner, udid.as_str(), &bundle_id, APP_TIMEOUT)
            })
            .await?
                .map(|()| SimReplyWire::Done)
                .map_err(|e| SimErrorWire::Failed(format!("launch failed: {e}")))
        }
        SimCmdWire::OpenUrl { url } if udid.platform() == Platform::Android => {
            agent::check_url(&url).map_err(SimErrorWire::BadInput)?;
            let device = android_device(hub, udid, cx).await?;
            on_own_thread(move || device.open_url(url.trim())).await?.map(|()| SimReplyWire::Done)
        }
        SimCmdWire::OpenUrl { url } => {
            agent::check_url(&url).map_err(SimErrorWire::BadInput)?;
            let udid = simctl_ready(hub, udid, cx).await?;
            on_own_thread(move || simctl::open_url(&SystemRunner, udid.as_str(), url.trim(), APP_TIMEOUT))
                .await?
                .map(|()| SimReplyWire::Done)
                .map_err(|e| SimErrorWire::Failed(format!("could not open the URL: {e}")))
        }
        SimCmdWire::Install { path } if udid.platform() == Platform::Android => {
            let apk = super::android::apk_path(&path, &target.worktree)?;
            let device = android_device(hub, udid, cx).await?;
            on_own_thread(move || device.install(&apk)).await?.map(|()| SimReplyWire::Done)
        }
        SimCmdWire::Install { path } => {
            let udid = simctl_ready(hub, udid, cx).await?;
            let worktree = target.worktree.clone();
            on_own_thread(move || {
                let app = install_path(&path, &worktree)?;
                simctl::install(&SystemRunner, udid.as_str(), &app, INSTALL_TIMEOUT)
                    .map_err(|e| SimErrorWire::Failed(format!("install failed: {e}")))
            })
            .await?
            .map(|()| SimReplyWire::Done)
        }
        SimCmdWire::Shutdown { .. } if SimulatorHub::is_phone(udid) => {
            Err(SimErrorWire::Refused("this is a phone; OxiMux never shuts one down".into()))
        }
        SimCmdWire::Shutdown { force } => {
            let refused = hub
                .update(cx, |hub, cx| {
                    if !force && !hub.is_owned(udid) {
                        return true;
                    }
                    // A recording is finalized before the device goes.
                    hub.shutdown_after_recording(udid, cx);
                    false
                });
            if refused {
                return Err(SimErrorWire::Refused(
                    "this simulator was booted outside OxiMux; pass --force only if the user asked to shut it down".into(),
                ));
            }
            Ok(SimReplyWire::Done)
        }
    }
}

/// Why an agent may not wake a device the user shut down from the panel.
const STOPPED_BY_USER: &str =
    "the user shut this simulator down in OxiMux; ask them before booting it again (`oximux sim attach`)";

/// The device's helper session, waking the device if it is parked, was never
/// started this run, or dropped its stream.
async fn live_session(hub: &Entity<SimulatorHub>, udid: &DeviceId, cx: &mut AsyncApp) -> Result<StreamSession, SimErrorWire> {
    let deadline = Instant::now() + START_WAIT;
    let mut woke = false;
    loop {
        let (wake, session) = hub
            .update(cx, |hub, cx| {
                // Wake once; after that only watch it come up.
                let wake = if woke { phase_wake(hub, udid) } else { hub.wake_for_agent(udid, cx) };
                (wake, hub.session(udid))
            });
        woke = true;
        match (wake, session) {
            (Wake::Live, Some(session)) if session.framebuffer_size().is_some() => return Ok(session),
            (Wake::Failed(why), _) => return Err(SimErrorWire::Unavailable(why)),
            (Wake::StoppedByUser, _) => return Err(SimErrorWire::Refused(STOPPED_BY_USER.into())),
            (Wake::Starting, _) if Instant::now() >= deadline => {
                let booting = matches!(hub.read_with(cx, |hub, _| hub.phase(udid)), oximux_simulator::registry::Phase::Booting { .. });
                return Err(if booting {
                    SimErrorWire::Unavailable("the simulator is still booting; retry in a few seconds".into())
                } else {
                    SimErrorWire::NotStreaming
                });
            }
            _ if Instant::now() >= deadline => return Err(SimErrorWire::NotStreaming),
            _ => cx.background_executor().timer(Duration::from_millis(250)).await,
        }
    }
}

fn phase_wake(hub: &SimulatorHub, udid: &DeviceId) -> Wake {
    use oximux_simulator::registry::Phase;
    match hub.phase(udid) {
        Phase::Live { .. } => Wake::Live,
        Phase::Failed { error } => Wake::Failed(error),
        Phase::Disconnected { reason } => Wake::Failed(reason),
        _ => Wake::Starting,
    }
}

/// An Android device's `adb` handle, once it streams (the serial is only
/// known then).
async fn android_device(hub: &Entity<SimulatorHub>, udid: &DeviceId, cx: &mut AsyncApp) -> Result<super::android::Device, SimErrorWire> {
    let session = live_session(hub, udid, cx).await?;
    let adb = hub.read_with(cx, |hub, _| hub.android_sdk().map(|s| s.adb()));
    super::android::Device::of(&session, adb)
}

/// `simctl` app verbs need Xcode and a booted device: wake it as the screen
/// verbs do (a live stream means it is booted).
async fn simctl_ready(hub: &Entity<SimulatorHub>, udid: &DeviceId, cx: &mut AsyncApp) -> Result<DeviceId, SimErrorWire> {
    if !hub.read_with(cx, |hub, _| hub.xcode_ok()) {
        return Err(SimErrorWire::Unavailable("Xcode was not found; see `oximux sim status`".into()));
    }
    live_session(hub, udid, cx).await?;
    Ok(udid.clone())
}

fn send(session: &StreamSession, command: Command) -> Result<(), SimErrorWire> {
    session.send(&command).map_err(|e| SimErrorWire::Failed(format!("the simulator did not take the input: {e}")))
}

/// Orientation and portrait framebuffer size, in pixels.
fn screen(session: &StreamSession) -> Result<(Orientation, Size), SimErrorWire> {
    let (w, h) = session.framebuffer_size().ok_or(SimErrorWire::NotStreaming)?;
    Ok((session.orientation(), Size::new(f64::from(w), f64::from(h))))
}

/// The accessibility tree. Retried for a few seconds while it is not there
/// yet: right after a boot SpringBoard has no frontmost app to describe.
async fn describe(session: &StreamSession, cx: &mut AsyncApp) -> Result<Vec<AxNode>, SimErrorWire> {
    let deadline = Instant::now() + AX_SETTLE;
    loop {
        let session = session.clone();
        let tree = cx
            .background_executor()
            .spawn(async move { session.describe(HELPER_TIMEOUT).map_err(|e| e.to_string()) })
            .await;
        match tree {
            Ok(nodes) if !nodes.is_empty() => return Ok(nodes),
            _ if Instant::now() < deadline => cx.background_executor().timer(Duration::from_millis(500)).await,
            Ok(_) => return Err(SimErrorWire::Failed("the accessibility tree is empty".into())),
            Err(e) => return Err(SimErrorWire::Failed(format!("could not read the accessibility tree: {e}"))),
        }
    }
}

/// The AX root frame's size: the whole screen, in the app's orientation.
fn root_size(nodes: &[AxNode]) -> Size {
    nodes.first().map_or(Size::new(0.0, 0.0), |n| Size::new(n.frame.w, n.frame.h))
}

/// Pixels per point, read once per device from the AX root frame.
async fn scale(hub: &Entity<SimulatorHub>, udid: &DeviceId, session: &StreamSession, cx: &mut AsyncApp) -> Result<f64, SimErrorWire> {
    if let Some(scale) = hub.read_with(cx, |hub, _| hub.scale(udid)).or_else(|| session.point_scale()) {
        return Ok(scale);
    }
    let nodes = describe(session, cx).await?;
    scale_from(hub, udid, session, &nodes, cx)
}

/// [`scale`], from a tree already read.
fn scale_from(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    session: &StreamSession,
    nodes: &[AxNode],
    cx: &mut AsyncApp,
) -> Result<f64, SimErrorWire> {
    if let Some(scale) = hub.read_with(cx, |hub, _| hub.scale(udid)).or_else(|| session.point_scale()) {
        return Ok(scale);
    }
    let (o, portrait) = screen(session)?;
    let scale = agent::device_scale(geometry::display_size(o, portrait), root_size(nodes))
        .ok_or_else(|| SimErrorWire::Failed("could not work out the screen's scale from the accessibility tree".into()))?;
    hub.update(cx, |hub, _| hub.set_scale(udid, scale));
    Ok(scale)
}

async fn point_to_portrait(
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

fn off_screen(at: (f64, f64), o: Orientation, portrait: Size, scale: f64) -> SimErrorWire {
    let pts = agent::display_points(o, portrait, scale);
    SimErrorWire::BadInput(format!(
        "({}, {}) is off the screen, which is {}×{} points (x right, y down, from the top-left)",
        at.0, at.1, pts.w, pts.h
    ))
}

/// The centre of the element `query` finds, as a touch coordinate.
async fn element(session: &StreamSession, query: Query<'_>, what: &str, cx: &mut AsyncApp) -> Result<(f64, f64), SimErrorWire> {
    let nodes = describe(session, cx).await?;
    let node = ax::find(&nodes, query).ok_or_else(|| {
        SimErrorWire::NotFound(format!("no on-screen element matches “{what}” (list them with `oximux sim ax`)"))
    })?;
    Ok(agent::ax_point_to_portrait(session.orientation(), root_size(&nodes), ax::center(node)))
}

fn ax_wire(node: &AxNode, depth: usize, frame: geometry::Rect) -> SimAxNodeWire {
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

async fn screenshot(hub: &Entity<SimulatorHub>, udid: &DeviceId, full: bool, cx: &mut AsyncApp) -> Out {
    let session = live_session(hub, udid, cx).await?;
    let scale = scale(hub, udid, &session, cx).await?;
    cx.background_executor()
        .spawn(async move {
            let png = session.screenshot_png(HELPER_TIMEOUT).map_err(|e| SimErrorWire::Failed(format!("screenshot failed: {e}")))?;
            let (w, h) = agent::png_size(&png).ok_or_else(|| SimErrorWire::Failed("the screenshot is not a PNG".into()))?;
            if full {
                return Ok(SimReplyWire::Screenshot { png, width: w, height: h, scale });
            }
            // One pixel per point: a position read off the image is a tap
            // coordinate.
            let (pw, ph) = ((f64::from(w) / scale).round() as u32, (f64::from(h) / scale).round() as u32);
            let png = downscale(&png, pw, ph).map_err(|e| SimErrorWire::Failed(format!("screenshot failed: {e}")))?;
            Ok(SimReplyWire::Screenshot { png, width: pw, height: ph, scale: 1.0 })
        })
        .await
}

fn downscale(png: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    let small = image.resize_exact(w.max(1), h.max(1), image::imageops::FilterType::Triangle);
    let mut out = Vec::new();
    small.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out)
}

/// `path` (relative paths are the worktree's) as an installable `.app`
/// inside the worktree or Xcode's DerivedData.
fn install_path(path: &str, worktree: &Path) -> Result<PathBuf, SimErrorWire> {
    let path = Path::new(path);
    let path = if path.is_absolute() { path.to_path_buf() } else { worktree.join(path) };
    let derived = std::env::var_os("HOME").map(|home| agent::derived_data(Path::new(&home)));
    agent::check_install_path(&path, worktree, derived.as_deref()).map_err(|e| match e {
        InstallPathError::NotAnApp => SimErrorWire::BadInput(format!("{} is not a built .app bundle", path.display())),
        InstallPathError::Outside => SimErrorWire::PathOutsideWorktree,
    })
}
