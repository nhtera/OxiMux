//! The device-state verbs (`sim location`, `appearance`, `privacy`, `push`,
//! `status-bar`, `erase`, `net`), once consent for the device is settled.
//!
//! Everything about a request is checked before the user is asked: the
//! device class can do it (an emulator takes no push, a real iPhone none of
//! these), the names and numbers are valid, and the platform has a command
//! for it — a question about something that would then fail is noise. Erase
//! and push are asked every time, as is every change to a real device; the
//! change itself is the hub's (see `hub::device_state`), shared with the
//! panel's Device menu.

use std::time::{Duration, Instant};

use gpui::{AsyncApp, Entity};
use oximux_remote_proto::simulator::{SimErrorWire, SimLocationWire, SimNetWire, SimPrivacyActionWire, SimReplyWire, SimStateCmdWire};
use oximux_simulator::caps::{self, StateSet};
use oximux_simulator::state::{IOS_NET_REFUSAL, NetChange, PrivacyAction, Reject, Service, StateChange};
use oximux_simulator::{DeviceId, Platform, Source, android, simctl_state};

use super::Target;
use super::verbs::{Out, live_session};
use crate::shell::simulator::hub::{AskKind, PerCallAnswer, SimulatorHub};

pub(super) async fn run(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, cmd: SimStateCmdWire, target: &Target, cx: &mut AsyncApp) -> Out {
    // Why, rather than "not available": the obvious fix would slow the Mac.
    if matches!(cmd, SimStateCmdWire::Net(_)) && udid.platform() != Platform::Android {
        let why = if udid.is_physical() { "a real iPhone's network is not shaped from here; throttle inside your app" } else { IOS_NET_REFUSAL };
        return Err(SimErrorWire::Refused(why.into()));
    }
    if let Some((need, what)) = needs(&cmd)
        && !hub.read_with(cx, |hub, _| hub.caps(udid)).state.contains(need)
    {
        return Err(SimErrorWire::Refused(caps::refuse(what, udid)));
    }
    match cmd {
        SimStateCmdWire::Erase { force } => erase(hub, udid, name, force, target, cx).await,
        other => {
            let change = change_of(other)?;
            check(udid, &change)?;
            let ask = match &change {
                StateChange::Push { app, .. } => Some(AskKind::Push { app: app.clone() }),
                _ if udid.is_physical() => Some(AskKind::PhysicalState { what: describe(&change) }),
                _ => None,
            };
            // Woken (it may be parked or shut down): an Android device's
            // serial is only known while it streams.
            let session = live_session(hub, udid, cx).await?;
            if let Some(android) = session.android() {
                android::net::check_transport(android.serial(), &change).map_err(rejected)?;
            }
            if let Some(kind) = ask {
                confirm(hub, udid, name, kind, target, cx).await?;
            }
            let visible = matches!(change, StateChange::Appearance { .. } | StateChange::StatusBar { .. });
            let applied = hub.update(cx, |hub, cx| hub.apply_state(udid, change, cx));
            applied.await.map_err(SimErrorWire::Failed)?;
            if visible {
                // From the tool's return, not its start: motion left over
                // from before (a tap's animation) is not the change.
                shown(&session, Instant::now(), cx).await;
            }
            Ok(SimReplyWire::Done)
        }
    }
}

/// The longest a change the screen shows waits to be shown.
const SHOWN_WAIT: Duration = Duration::from_millis(1500);
/// Motion this recent when the change returns means the screen was busy.
const BUSY_BEFORE: Duration = Duration::from_millis(300);
/// What a busy screen waits for a change to show.
const BUSY_REDRAW: Duration = Duration::from_millis(500);

/// Wait (at most [`SHOWN_WAIT`]) until the screen has changed since
/// `since`. The tools return before the device redraws, so a screenshot
/// taken right after `status-bar clean` showed the old bar; the reply now
/// comes once the change is on screen. Done either way: the change was made
/// (one that changes nothing, `appearance dark` when dark, waits it out).
async fn shown(session: &oximux_simulator::stream::StreamSession, since: Instant, cx: &mut AsyncApp) {
    // A screen already moving (a spinner) moves on whatever happens: its
    // motion would end the wait before the redraw. Then wait what a redraw
    // was measured to take instead (a screenshot 0.5 s after `status-bar
    // clean` always showed it).
    if session.frame_clock().last_motion_at.is_some_and(|motion| since.saturating_duration_since(motion) < BUSY_BEFORE) {
        cx.background_executor().timer(BUSY_REDRAW).await;
        return;
    }
    let deadline = since + SHOWN_WAIT;
    while Instant::now() < deadline {
        if session.frame_clock().last_motion_at.is_some_and(|motion| motion > since) {
            return;
        }
        cx.background_executor().timer(Duration::from_millis(50)).await;
    }
}

/// The capability a change needs, named for [`caps::refuse`].
fn needs(cmd: &SimStateCmdWire) -> Option<(StateSet, &'static str)> {
    Some(match cmd {
        SimStateCmdWire::Location(_) => (StateSet::LOCATION, "Setting a location"),
        SimStateCmdWire::Appearance { .. } => (StateSet::APPEARANCE, "Changing the appearance"),
        SimStateCmdWire::Privacy { .. } => (StateSet::PRIVACY, "Changing privacy settings"),
        SimStateCmdWire::Push { .. } => (StateSet::PUSH, "A push notification"),
        SimStateCmdWire::StatusBar { .. } => (StateSet::STATUS_BAR, "Overriding the status bar"),
        SimStateCmdWire::Erase { .. } => (StateSet::ERASE, "Erasing"),
        SimStateCmdWire::Net(_) => (StateSet::NETWORK, "Shaping the network"),
    })
}

/// The wire's change as the backend's, its names checked.
fn change_of(cmd: SimStateCmdWire) -> Result<StateChange, SimErrorWire> {
    Ok(match cmd {
        SimStateCmdWire::Location(SimLocationWire::Set { lat, lon }) => StateChange::Location { lat, lon },
        SimStateCmdWire::Location(SimLocationWire::Clear) => StateChange::ClearLocation,
        SimStateCmdWire::Appearance { dark } => StateChange::Appearance { dark },
        SimStateCmdWire::Privacy { action, service, app } => StateChange::Privacy {
            action: match action {
                SimPrivacyActionWire::Grant => PrivacyAction::Grant,
                SimPrivacyActionWire::Revoke => PrivacyAction::Revoke,
                SimPrivacyActionWire::Reset => PrivacyAction::Reset,
            },
            service: Service::parse(&service).map_err(rejected)?,
            app,
        },
        SimStateCmdWire::Push { app, payload } => StateChange::Push { app, payload },
        SimStateCmdWire::StatusBar { clean } => StateChange::StatusBar { clean },
        SimStateCmdWire::Net(net) => StateChange::Net(match net {
            SimNetWire::Speed(speed) => NetChange::Speed(speed),
            SimNetWire::Offline => NetChange::Offline,
            SimNetWire::Online => NetChange::Online,
        }),
        SimStateCmdWire::Erase { .. } => return Err(SimErrorWire::BadInput("not a state change".into())),
    })
}

/// Whether the platform has a command for `change` (built, not run).
fn check(udid: &DeviceId, change: &StateChange) -> Result<(), SimErrorWire> {
    match udid.source() {
        Source::Simctl => {
            let sim = udid.sim_udid().ok_or_else(|| SimErrorWire::Unavailable(format!("{udid} is not a simulator")))?;
            simctl_state::argv(&sim, change).map(drop).map_err(rejected)
        }
        Source::Adb => {
            let target = android::Target::from_id(udid).ok_or_else(|| SimErrorWire::Unavailable(format!("{udid} is not an Android device")))?;
            android::state::steps(&target, change).map(drop).map_err(rejected)
        }
        Source::Devicectl => Err(SimErrorWire::Refused(caps::refuse("Changing its state", udid))),
    }
}

fn rejected(r: Reject) -> SimErrorWire {
    match r {
        Reject::BadInput(why) => SimErrorWire::BadInput(why),
        Reject::Unsupported(why) => SimErrorWire::Refused(why),
    }
}

/// A change in words, for the question on a real device.
fn describe(change: &StateChange) -> String {
    match change {
        StateChange::Location { lat, lon } => format!("set its location to {lat}, {lon}"),
        StateChange::ClearLocation => "clear its location".into(),
        StateChange::Appearance { dark } => format!("switch to {} mode", if *dark { "dark" } else { "light" }),
        StateChange::Privacy { action, service, app } => format!("{} {} for {app}", action.word(), service.name()),
        StateChange::Push { app, .. } => format!("send a notification to {app}"),
        StateChange::StatusBar { clean: true } => "show a clean status bar".into(),
        StateChange::StatusBar { clean: false } => "restore the status bar".into(),
        StateChange::Net(NetChange::Speed(speed)) => format!("set its network speed to {speed}"),
        // Both radios: on a phone that is the user's own mobile data too.
        StateChange::Net(NetChange::Offline) => "turn off its Wi-Fi and mobile data".into(),
        StateChange::Net(NetChange::Online) => "turn its Wi-Fi and mobile data back on".into(),
    }
}

async fn erase(hub: &Entity<SimulatorHub>, udid: &DeviceId, name: &str, force: bool, target: &Target, cx: &mut AsyncApp) -> Out {
    if !force && !hub.read_with(cx, |hub, _| hub.is_owned(udid)) {
        return Err(SimErrorWire::Refused(
            "this device was booted outside OxiMux; pass --force only if the user asked to erase it".into(),
        ));
    }
    confirm(hub, udid, name, AskKind::Erase, target, cx).await?;
    let erased = hub.update(cx, |hub, cx| hub.erase(udid, cx));
    erased.await.map(|()| SimReplyWire::Done).map_err(SimErrorWire::Failed)
}

/// Ask the user, in the worktree's panel, before an agent does `kind` to the
/// device — every time, whatever was allowed before — and wait (up to
/// `ASK_TTL`) for the answer.
pub(super) async fn confirm(
    hub: &Entity<SimulatorHub>,
    udid: &DeviceId,
    name: &str,
    kind: AskKind,
    target: &Target,
    cx: &mut AsyncApp,
) -> Result<(), SimErrorWire> {
    let what = kind.what(name);
    let id = hub.update(cx, |hub, cx| hub.ask_per_call(target.principal, udid, &target.worktree, name, kind, cx));
    let (asked, toast) = (target.clone(), what.clone());
    super::with_root(cx, target, move |root, window, cx| {
        let ids = (asked.project_id.clone(), asked.workspace_id.clone());
        root.ask_simulator_consent(&asked.worktree, (&asked.label, asked.principal), &toast, ids, window, cx);
    });
    // Named as the banner named it.
    let who = match target.principal {
        oximux_remote_host::SimPrincipal::Agent => "an agent",
        oximux_remote_host::SimPrincipal::Mcp => "this MCP client",
    };
    loop {
        match hub.update(cx, |hub, _| hub.per_call_answer(id)) {
            PerCallAnswer::Allowed => return Ok(()),
            PerCallAnswer::Refused => return Err(SimErrorWire::Refused(format!("the user declined to let {who} {what}"))),
            PerCallAnswer::Gone => {
                return Err(SimErrorWire::Refused(format!("the user did not answer whether {who} may {what}; ask them, then retry")));
            }
            PerCallAnswer::Pending => cx.background_executor().timer(Duration::from_millis(250)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_is_checked_before_anyone_is_asked() {
        let bad = SimStateCmdWire::Privacy { action: SimPrivacyActionWire::Grant, service: "keychain".into(), app: "com.a.b".into() };
        assert!(matches!(change_of(bad), Err(SimErrorWire::BadInput(_))));
        let phone = DeviceId("adb:R58".into());
        let push = StateChange::Push { app: "com.a.b".into(), payload: r#"{"aps":{}}"#.into() };
        assert!(matches!(check(&phone, &push), Err(SimErrorWire::Refused(_))));
        let sim = DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into());
        assert!(check(&sim, &push).is_ok());
        assert!(matches!(check(&sim, &StateChange::Location { lat: 95.0, lon: 0.0 }), Err(SimErrorWire::BadInput(_))));
    }

    #[test]
    fn a_network_change_is_checked_per_platform() {
        let avd = DeviceId("avd:Medium_Phone".into());
        let speed = |s: &str| change_of(SimStateCmdWire::Net(SimNetWire::Speed(s.into()))).unwrap();
        assert!(check(&avd, &speed("edge")).is_ok());
        assert!(matches!(check(&avd, &speed("12;x")), Err(SimErrorWire::BadInput(_))));
        assert!(matches!(check(&DeviceId("adb:R58".into()), &speed("edge")), Err(SimErrorWire::Refused(_))), "emulator only");
        let sim = DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into());
        assert!(matches!(check(&sim, &StateChange::Net(NetChange::Offline)), Err(SimErrorWire::Refused(why)) if why.contains("Mac's network")));
    }

    #[test]
    fn each_change_names_its_capability() {
        assert_eq!(needs(&SimStateCmdWire::Erase { force: false }).map(|n| n.0), Some(StateSet::ERASE));
        assert_eq!(needs(&SimStateCmdWire::Net(SimNetWire::Offline)).map(|n| n.0), Some(StateSet::NETWORK));
        assert_eq!(describe(&StateChange::Appearance { dark: true }), "switch to dark mode");
        let grant = StateChange::Privacy { action: PrivacyAction::Grant, service: Service::Camera, app: "com.a.b".into() };
        assert_eq!(describe(&grant), "grant camera for com.a.b");
    }
}
