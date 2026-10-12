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
use oximux_remote_proto::simulator::{SimButtonWire, SimCmdWire, SimErrorWire, SimOrientationWire, SimReplyWire, SimTargetWire};
use oximux_simulator::agent::{self, InstallPathError};
use oximux_simulator::ax::{self, Query};
use oximux_simulator::ax_refs::TableKey;
use oximux_simulator::geometry;
use oximux_simulator::protocol::{Command, KeyPhase, TouchPhase};
use oximux_simulator::caps::{self, DeviceCaps};
use oximux_simulator::ios_device::input_map::{Gesture, MAX_TEXT, RunnerButton};
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::session::StreamSession;
use oximux_simulator::simctl::SimUdid;
use oximux_simulator::{Button, DeviceId, Orientation, Platform, Source, keyboard, simctl};

use super::Target;
use super::ax_lookup::{self, HELPER_TIMEOUT, Shown, off_screen, point_to_portrait, scale, screen};
use super::guards::{Verb, check_bundle_id, check_text, is_input, is_web_url, unwrap};
use super::settle::{self, Acted};
use super::device_state::confirm;
use crate::shell::simulator::hub::{AskKind, SimulatorHub, Wake, home_button, paste_now};

/// How long a woken device may take to start streaming: a cold boot of a
/// shut-down device measured 40-odd seconds on an M-series Mac.
const START_WAIT: Duration = Duration::from_secs(60);
/// `simctl launch` / `openurl`.
const APP_TIMEOUT: Duration = Duration::from_secs(60);
/// `simctl install` (a large app copies for a while).
const INSTALL_TIMEOUT: Duration = Duration::from_secs(180);
/// Finger down to finger up, for a tap.
const TAP_HOLD: Duration = Duration::from_millis(60);
/// A tapped field taking focus (and its keyboard coming up) before typing.
const FOCUS_WAIT: Duration = Duration::from_millis(300);

/// Run a blocking call on a thread of its own: an install copies for minutes,
/// and the background executor's few threads are shared by the whole app.
pub(super) async fn on_own_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, SimErrorWire> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.map_err(|_| SimErrorWire::Failed("the operation stopped unexpectedly".into()))
}

pub(super) type Out = Result<SimReplyWire, SimErrorWire>;
/// How long the scrcpy server gets to refuse a session's first input.
const INPUT_VERDICT: Duration = Duration::from_millis(300);

/// What an agent hears when the phone refuses injected input.
const INPUT_BLOCKED: &str = "the phone refuses injected input (on Xiaomi/HyperOS: \"USB debugging (Security settings)\" is off); \
     ask the user to turn it on in Developer options and reconnect the phone";

pub(super) async fn run(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    name: &str,
    cmd: SimCmdWire,
    target: &Target,
    cx: &mut AsyncApp,
) -> Out {
    // Envelopes off first: every guard below reads the verb inside.
    let verb = unwrap(cmd)?;
    // An Android phone may drop injected input on the floor (an OEM setting):
    // the server says so only on stderr, so an agent is told here rather
    // than hearing "tapped" for a tap that never happened. Emulators never do.
    let checked = is_input(&verb.inner) && udid.source() == Source::Adb && udid.is_physical();
    let phone = |cx: &mut AsyncApp| hub.read_with(cx, |hub, _| hub.session(udid)).and_then(|s| s.android().cloned());
    if checked && phone(cx).is_some_and(|s| s.input_blocked()) {
        return Err(SimErrorWire::Refused(INPUT_BLOCKED.into()));
    }
    let reply = run_verb(hub, udid, name, verb, target, cx).await;
    // The session the verb used: it may have woken or reconnected the phone.
    let Some(session) = phone(cx).filter(|_| checked && reply.is_ok()) else { return reply };
    let timer = cx.background_executor().timer(INPUT_VERDICT);
    input_verdict(|| session.input_blocked(), || session.input_proven(), || session.note_input_taken(), timer).await?;
    reply
}

/// Whether input sent to a phone went through: refused if the phone blocks
/// it; otherwise, the first time in a session, the server gets `wait` to
/// say so before the session is trusted. The window is a heuristic: a
/// slower refusal still latches, and the next verb is refused up front.
async fn input_verdict(
    blocked: impl Fn() -> bool,
    proven: impl Fn() -> bool,
    mark: impl Fn(),
    wait: impl std::future::Future,
) -> Result<(), SimErrorWire> {
    if !blocked() && !proven() {
        wait.await;
    }
    if blocked() {
        return Err(SimErrorWire::Refused(INPUT_BLOCKED.into()));
    }
    mark();
    Ok(())
}

async fn run_verb(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, verb: Verb, target: &Target, cx: &mut AsyncApp) -> Out {
    // A real device does only what it can now, and opens only web links:
    // a custom scheme can dial, pay or message on someone's own phone.
    if udid.is_physical() {
        if let Some(what) = verb.refused_on(&hub.read_with(cx, |hub, _| hub.caps(udid))) {
            let mut why = caps::refuse(what, udid);
            // Input reaches an iPhone once the user turns its control on.
            if udid.source() == Source::Devicectl && verb.refused_on(&DeviceCaps::for_session(udid, true)).is_none() {
                why = format!("{why}; ask the user to turn its control on in the Mobile Emulator panel (Control from OxiMux…)");
            }
            return Err(SimErrorWire::Refused(why));
        }
        if let SimCmdWire::OpenUrl { url } = &verb.inner
            && !is_web_url(url)
        {
            return Err(SimErrorWire::Refused("only http and https links open on a real device".into()));
        }
        // A sleeping phone streams nothing: a screen verb would only wait.
        if verb.needs_screen() && hub.read_with(cx, |hub, _| hub.screen_off(udid)) {
            return Err(SimErrorWire::Unavailable(
                "the phone's screen is off; ask the user to press Wake in the Mobile Emulator panel".into(),
            ));
        }
    }
    // Input takes turns per device: interleaved touch streams from parallel
    // calls would make gestures nobody asked for. A settle keeps the turn
    // until the screen it reports on is idle.
    let lock = is_input(&verb.inner).then(|| hub.update(cx, |hub, _| hub.input_lock(udid)));
    let _turn = match &lock {
        Some(lock) => Some(lock.lock().await),
        None => None,
    };
    let refs: TableKey = (WorktreeKey::from_path(&target.worktree), udid.clone(), verb.caller);
    if let Some(timeout_ms) = verb.settle {
        return settle::settled(hub, udid, name, verb.inner, timeout_ms, &refs, target, cx).await;
    }
    // A ref's tap reports what it aimed at (and whether it had moved).
    let by_ref = matches!(verb.inner, SimCmdWire::Tap(SimTargetWire::Ref(_)) | SimCmdWire::TypeInto { .. });
    let (started, mut acted) = (Instant::now(), Acted::default());
    let reply = execute(hub, udid, name, verb.inner, &refs, &mut acted, target, cx).await?;
    Ok(if by_ref { SimReplyWire::Settled(acted.report(None, started)) } else { reply })
}

/// Carry out one verb, its envelopes already off and its guards passed.
/// `acted` hears what a tap aimed at (for a settle's report).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    name: &str,
    cmd: SimCmdWire,
    refs: &TableKey,
    acted: &mut Acted,
    target: &Target,
    cx: &mut AsyncApp,
) -> Out {
    match cmd {
        // Answered before consent in `agent_ops::run`.
        SimCmdWire::Status | SimCmdWire::Devices | SimCmdWire::Attach { .. } | SimCmdWire::Detach => Ok(SimReplyWire::Done),
        // Taken off by `unwrap` before anything runs.
        SimCmdWire::Settled { .. } | SimCmdWire::AsCaller { .. } => Err(SimErrorWire::BadInput("a wrapped verb inside a verb".into())),
        SimCmdWire::AxRefs { max } => settle::ax_refs(hub, udid, max, refs, cx).await,
        SimCmdWire::Wait { target: aim, gone, timeout_ms } => super::checks::wait(hub, udid, aim, gone, timeout_ms, refs, cx).await,
        SimCmdWire::Assert { target: aim, check } => super::checks::assert(hub, udid, aim, check, refs, cx).await,
        SimCmdWire::Screenshot { full } => screenshot(hub, udid, full, cx).await,
        SimCmdWire::Ax { max } => {
            let session = live_session(hub, udid, cx).await?;
            let nodes = ax_lookup::read(hub, udid, &session, cx).await?;
            // Frames in the screenshot's space, whichever way the app is turned.
            let shown = Shown::new(hub, udid, &session, &nodes, cx)?;
            let flat = ax::flatten(&nodes, max.clamp(1, 5000) as usize);
            Ok(SimReplyWire::Ax(flat.iter().map(|f| shown.wire(f.node, f.depth)).collect()))
        }
        SimCmdWire::Tap(aim) => {
            let session = live_session(hub, udid, cx).await?;
            let at = match aim {
                SimTargetWire::Point(p) => point_to_portrait(hub, udid, &session, (p.x, p.y), cx).await?,
                SimTargetWire::Label(label) => settle::aim(hub, udid, &session, Query::Label(&label), &label, acted, cx).await?,
                SimTargetWire::Id(id) => settle::aim(hub, udid, &session, Query::Id(&id), &id, acted, cx).await?,
                SimTargetWire::Ref(r) => settle::aim_ref(hub, udid, &session, r, refs, acted, cx).await?,
            };
            if let Some(done) = on_iphone(&session, vec![Gesture::Tap { at, taps: 1 }]).await {
                return done;
            }
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
            let end = map((to.x, to.y))?;
            // A real iPhone takes the swipe whole (its runner paces it).
            let ms = u64::from(duration_ms.clamp(50, 5000));
            if let Some(done) = on_iphone(&session, vec![Gesture::Drag { from: start, to: end, ms, hold: 0, settle: 0 }]).await {
                return done;
            }
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
        // One turn of the input lock: no other input lands between the two.
        // The text is checked before the tap, which cannot be taken back.
        SimCmdWire::TypeInto { target: aim, text, paste } => {
            check_text(&text)?;
            Box::pin(execute(hub, udid, name, SimCmdWire::Tap(aim), refs, acted, target, cx)).await?;
            cx.background_executor().timer(FOCUS_WAIT).await;
            Box::pin(execute(hub, udid, name, SimCmdWire::Type { text, paste }, refs, acted, target, cx)).await
        }
        SimCmdWire::Type { text, paste } => {
            check_text(&text)?;
            let session = live_session(hub, udid, cx).await?;
            // A controlled iPhone types text as text, any Unicode; a CR (alone
            // or before LF) is a newline, never dropped into an empty batch.
            let pieces: Vec<char> = text.replace("\r\n", "\n").replace('\r', "\n").chars().collect();
            let typed = pieces.chunks(MAX_TEXT).map(|p| Gesture::Text(p.iter().collect())).collect();
            if let Some(done) = on_iphone(&session, typed).await {
                return done;
            }
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
        // A controlled iPhone's own buttons (caps checked above).
        SimCmdWire::Button(button @ (SimButtonWire::Home | SimButtonWire::VolumeUp | SimButtonWire::VolumeDown))
            if udid.source() == Source::Devicectl =>
        {
            let session = live_session(hub, udid, cx).await?;
            let pressed = match button {
                SimButtonWire::Home => RunnerButton::Home,
                SimButtonWire::VolumeUp => RunnerButton::VolumeUp,
                _ => RunnerButton::VolumeDown,
            };
            on_iphone(&session, vec![Gesture::Button(pressed)])
                .await
                .unwrap_or_else(|| Err(SimErrorWire::Refused(caps::refuse("That button", udid))))
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
        SimCmdWire::Launch { bundle_id, relaunch } if udid.source() == Source::Devicectl => {
            check_bundle_id(&bundle_id)?;
            let device = hub.read_with(cx, |hub, _| super::iphone::Device::reachable(hub, udid))?;
            let launched = bundle_id.clone();
            let done = on_own_thread(move || device.launch(&bundle_id, relaunch)).await?.map(|()| SimReplyWire::Done);
            // What it launched is what typing and the tree address next.
            if done.is_ok() {
                hub.update(cx, |hub, cx| hub.note_launched(udid, &launched, cx));
            }
            done
        }
        SimCmdWire::Launch { bundle_id, relaunch } => {
            check_bundle_id(&bundle_id)?;
            let udid = simctl_ready(hub, udid, cx).await?;
            on_own_thread(move || {
                if relaunch {
                    // Not running is fine: launching is what matters.
                    let _ = simctl::terminate(&SystemRunner, &udid, &bundle_id, APP_TIMEOUT);
                }
                simctl::launch(&SystemRunner, &udid, &bundle_id, APP_TIMEOUT)
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
        // A web link (checked above, as on every real device), in Safari.
        SimCmdWire::OpenUrl { url } if udid.source() == Source::Devicectl => {
            agent::check_url(&url).map_err(SimErrorWire::BadInput)?;
            let device = hub.read_with(cx, |hub, _| super::iphone::Device::reachable(hub, udid))?;
            on_own_thread(move || device.open_url(url.trim())).await?.map(|()| SimReplyWire::Done)
        }
        SimCmdWire::OpenUrl { url } => {
            agent::check_url(&url).map_err(SimErrorWire::BadInput)?;
            let udid = simctl_ready(hub, udid, cx).await?;
            on_own_thread(move || simctl::open_url(&SystemRunner, &udid, url.trim(), APP_TIMEOUT))
                .await?
                .map(|()| SimReplyWire::Done)
                .map_err(|e| SimErrorWire::Failed(format!("could not open the URL: {e}")))
        }
        SimCmdWire::Install { path } if udid.platform() == Platform::Android => {
            let apk = super::android::apk_path(&path, &target.worktree)?;
            // Every real device's Install arm calls `confirm_install`, after
            // the device is reachable: an unplugged phone is not worth a
            // question.
            let device = android_device(hub, udid, cx).await?;
            if udid.is_physical() {
                confirm(hub, udid, name, AskKind::Install { app: file_name(&apk) }, target, cx).await?;
            }
            on_own_thread(move || device.install(&apk)).await?.map(|()| SimReplyWire::Done)
        }
        SimCmdWire::Install { path } if udid.source() == Source::Devicectl => {
            let app = super::iphone::device_build(install_path(&path, &target.worktree)?)?;
            // Asked every time, once the phone is reachable (as on Android).
            let device = hub.read_with(cx, |hub, _| super::iphone::Device::reachable(hub, udid))?;
            confirm(hub, udid, name, AskKind::Install { app: file_name(&app) }, target, cx).await?;
            let done = on_own_thread(move || device.install(&app)).await?.map(|()| SimReplyWire::Done);
            // The target picker lists the new app next time it opens.
            hub.update(cx, |hub, _| hub.forget_phone_apps(udid));
            done
        }
        SimCmdWire::Install { path } => {
            let udid = simctl_ready(hub, udid, cx).await?;
            let worktree = target.worktree.clone();
            on_own_thread(move || {
                let app = install_path(&path, &worktree)?;
                simctl::install(&SystemRunner, &udid, &app, INSTALL_TIMEOUT)
                    .map_err(|e| SimErrorWire::Failed(format!("install failed: {e}")))
            })
            .await?
            .map(|()| SimReplyWire::Done)
        }
        SimCmdWire::DeviceState(change) => super::device_state::run(hub, udid, name, change, target, cx).await,
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
pub(super) async fn live_session(hub: &Entity<SimulatorHub>, udid: &DeviceId, cx: &mut AsyncApp) -> Result<StreamSession, SimErrorWire> {
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
                    SimErrorWire::Unavailable("it is still starting; retry in a few seconds".into())
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

/// `simctl` app verbs need a simulator, Xcode and a booted device: wake it as
/// the screen verbs do (a live stream means it is booted).
async fn simctl_ready(hub: &Entity<SimulatorHub>, udid: &DeviceId, cx: &mut AsyncApp) -> Result<SimUdid, SimErrorWire> {
    let sim = simctl_udid(udid)?;
    if !hub.read_with(cx, |hub, _| hub.xcode_ok()) {
        return Err(SimErrorWire::Unavailable("Xcode was not found; see `oximux sim status`".into()));
    }
    live_session(hub, udid, cx).await?;
    Ok(sim)
}

/// `udid` as a `simctl` argument (a real iPhone's app verbs have their own
/// arms, through `devicectl`).
fn simctl_udid(udid: &DeviceId) -> Result<SimUdid, SimErrorWire> {
    udid.sim_udid().ok_or_else(|| SimErrorWire::Unavailable(format!("{udid} is not a simulator")))
}

/// Drives a real iPhone whose control is on with whole gestures, in its
/// queue's turn; `None` for any other device (it takes the touch stream).
async fn on_iphone(session: &StreamSession, gestures: Vec<Gesture>) -> Option<Out> {
    #[cfg(target_os = "macos")]
    {
        let control = session.ios_device().and_then(|d| d.control())?;
        let done = on_own_thread(move || gestures.into_iter().try_for_each(|g| control.gesture(g).map(drop))).await;
        Some(match done {
            Ok(Ok(())) => Ok(SimReplyWire::Done),
            Ok(Err(e)) => Err(SimErrorWire::Failed(format!("the iPhone did not take it: {e}"))),
            Err(e) => Err(e),
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (session, gestures);
        None
    }
}

fn send(session: &StreamSession, command: Command) -> Result<(), SimErrorWire> {
    session.send(&command).map_err(|e| SimErrorWire::Failed(format!("the simulator did not take the input: {e}")))
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

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
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

#[cfg(test)]
#[path = "verbs_tests.rs"]
mod tests;
