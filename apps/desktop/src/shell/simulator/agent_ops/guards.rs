//! What a control verb may do, decided before it runs.
//!
//! Envelopes come off first ([`unwrap`]): `AsCaller` (whose refs), then
//! `Settled` (wait and report), then the verb itself — and every guard reads
//! that inner verb, so wrapping a verb never gets it past a check the bare
//! verb would fail. The matches here name every variant (no `_` arm): a new
//! verb does not compile until each guard has decided about it.

use oximux_remote_proto::simulator::{SimButtonWire, SimCmdWire, SimErrorWire, SimTargetWire};
use oximux_simulator::caps::{ButtonSet, DeviceCaps};

/// A verb with its envelopes taken off.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Verb {
    /// Whose refs (`None`: the shared table).
    pub caller: Option<u64>,
    /// Wait for the screen to go idle after it, up to this long (0: the
    /// device class's default).
    pub settle: Option<u32>,
    pub inner: SimCmdWire,
}

/// Take the envelopes off `cmd`, in their one allowed order.
pub(super) fn unwrap(cmd: SimCmdWire) -> Result<Verb, SimErrorWire> {
    let (caller, cmd) = match cmd {
        SimCmdWire::AsCaller { caller, cmd } => (Some(caller), *cmd),
        other => (None, other),
    };
    let (settle, inner) = match cmd {
        SimCmdWire::Settled { action, timeout_ms } => (Some(timeout_ms), *action),
        other => (None, other),
    };
    if matches!(inner, SimCmdWire::AsCaller { .. } | SimCmdWire::Settled { .. }) {
        return Err(SimErrorWire::BadInput("a verb may be wrapped once for its caller, then once to settle — nothing else".into()));
    }
    if settle.is_some() && !is_action(&inner) {
        return Err(SimErrorWire::BadInput(
            "only an action settles: tap, swipe, type, button, rotate, launch or open-url".into(),
        ));
    }
    if caller.is_some() && !inner.is_control() {
        return Err(SimErrorWire::BadInput("only a control verb is run on a caller's behalf".into()));
    }
    Ok(Verb { caller, settle, inner })
}

/// The verbs `--settle` applies to: they change what is on screen.
pub(super) fn is_action(cmd: &SimCmdWire) -> bool {
    match cmd {
        SimCmdWire::Tap(_)
        | SimCmdWire::Swipe { .. }
        | SimCmdWire::Type { .. }
        | SimCmdWire::Button(_)
        | SimCmdWire::Rotate(_)
        | SimCmdWire::Launch { .. }
        | SimCmdWire::OpenUrl { .. } => true,
        SimCmdWire::Status
        | SimCmdWire::Devices
        | SimCmdWire::Attach { .. }
        | SimCmdWire::Detach
        | SimCmdWire::Screenshot { .. }
        | SimCmdWire::Ax { .. }
        | SimCmdWire::Install { .. }
        | SimCmdWire::Shutdown { .. }
        | SimCmdWire::Settled { .. }
        | SimCmdWire::AxRefs { .. }
        | SimCmdWire::Wait { .. }
        | SimCmdWire::Assert { .. }
        | SimCmdWire::AsCaller { .. } => false,
    }
}

/// The verbs that inject input (and take the per-device input turn).
pub(super) fn is_input(cmd: &SimCmdWire) -> bool {
    match cmd {
        SimCmdWire::Tap(_) | SimCmdWire::Swipe { .. } | SimCmdWire::Type { .. } | SimCmdWire::Button(_) => true,
        SimCmdWire::Settled { action, .. } => is_input(action),
        SimCmdWire::AsCaller { cmd, .. } => is_input(cmd),
        SimCmdWire::Status
        | SimCmdWire::Devices
        | SimCmdWire::Attach { .. }
        | SimCmdWire::Detach
        | SimCmdWire::Screenshot { .. }
        | SimCmdWire::Ax { .. }
        | SimCmdWire::Rotate(_)
        | SimCmdWire::Launch { .. }
        | SimCmdWire::OpenUrl { .. }
        | SimCmdWire::Install { .. }
        | SimCmdWire::Shutdown { .. }
        | SimCmdWire::AxRefs { .. }
        | SimCmdWire::Wait { .. }
        | SimCmdWire::Assert { .. } => false,
    }
}

/// Verbs that read or drive the live screen (not the app verbs, which go
/// through adb whether the screen is on or not). A settle reads the tree, so
/// it needs the screen whatever it wraps.
pub(super) fn needs_screen(cmd: &SimCmdWire) -> bool {
    match cmd {
        SimCmdWire::Screenshot { .. }
        | SimCmdWire::Ax { .. }
        | SimCmdWire::Tap(_)
        | SimCmdWire::Swipe { .. }
        | SimCmdWire::Type { .. }
        | SimCmdWire::Button(_)
        | SimCmdWire::Rotate(_)
        | SimCmdWire::Settled { .. }
        | SimCmdWire::AxRefs { .. }
        | SimCmdWire::Wait { .. }
        | SimCmdWire::Assert { .. } => true,
        SimCmdWire::AsCaller { cmd, .. } => needs_screen(cmd),
        SimCmdWire::Status
        | SimCmdWire::Devices
        | SimCmdWire::Attach { .. }
        | SimCmdWire::Detach
        | SimCmdWire::Launch { .. }
        | SimCmdWire::OpenUrl { .. }
        | SimCmdWire::Install { .. }
        | SimCmdWire::Shutdown { .. } => false,
    }
}

const AX: &str = "The accessibility tree";

/// The capability `cmd` needs that `caps` lacks, named for
/// [`oximux_simulator::caps::refuse`].
pub(super) fn refused_on(caps: &DeviceCaps, cmd: &SimCmdWire) -> Option<&'static str> {
    let (can, what) = match cmd {
        SimCmdWire::Status | SimCmdWire::Devices | SimCmdWire::Attach { .. } | SimCmdWire::Detach => return None,
        SimCmdWire::Screenshot { .. } => (caps.screenshot, "Screenshots"),
        SimCmdWire::Ax { .. } | SimCmdWire::AxRefs { .. } | SimCmdWire::Wait { .. } | SimCmdWire::Assert { .. } => (caps.ax, AX),
        // A ref is resolved in the tree before the touch.
        SimCmdWire::Tap(SimTargetWire::Ref(_)) if caps.touch => (caps.ax, AX),
        SimCmdWire::Tap(_) | SimCmdWire::Swipe { .. } => (caps.touch, "Touch input"),
        SimCmdWire::Type { .. } => (caps.keys, "Typing"),
        SimCmdWire::Button(button) => (caps.buttons.contains(button_set(*button)), "That button"),
        SimCmdWire::Rotate(_) => (caps.rotate, "Rotation"),
        SimCmdWire::Launch { .. } => (caps.launch, "Launching apps"),
        SimCmdWire::OpenUrl { .. } => (caps.open_url, "Opening URLs"),
        SimCmdWire::Install { .. } => (caps.install, "Installing apps"),
        SimCmdWire::Shutdown { .. } => (caps.shutdown, "Shutting down"),
        // The action's own needs, then the tree the settle reads.
        SimCmdWire::Settled { action, .. } => return refused_on(caps, action).or((!caps.ax).then_some(AX)),
        SimCmdWire::AsCaller { cmd, .. } => return refused_on(caps, cmd),
    };
    (!can).then_some(what)
}

impl Verb {
    /// [`refused_on`] for the inner verb, and for the tree a settle reads.
    pub(super) fn refused_on(&self, caps: &DeviceCaps) -> Option<&'static str> {
        refused_on(caps, &self.inner).or(self.settle.and((!caps.ax).then_some(AX)))
    }

    /// [`needs_screen`] for the inner verb; a settle always reads the screen.
    pub(super) fn needs_screen(&self) -> bool {
        self.settle.is_some() || needs_screen(&self.inner)
    }
}

fn button_set(button: SimButtonWire) -> ButtonSet {
    match button {
        SimButtonWire::Home => ButtonSet::HOME,
        SimButtonWire::Lock => ButtonSet::LOCK,
        SimButtonWire::Siri => ButtonSet::SIRI,
        SimButtonWire::SideButton => ButtonSet::SIDE,
        SimButtonWire::AppSwitcher => ButtonSet::APP_SWITCHER,
        SimButtonWire::Back => ButtonSet::BACK,
        SimButtonWire::VolumeUp => ButtonSet::VOLUME_UP,
        SimButtonWire::VolumeDown => ButtonSet::VOLUME_DOWN,
    }
}

/// An `http`/`https` URL (any case), the only kind a real device opens.
pub(super) fn is_web_url(url: &str) -> bool {
    url.trim().split_once(':').is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::simulator::{SimCheckWire, SimOrientationWire};
    use oximux_simulator::DeviceId;

    fn settled(cmd: SimCmdWire) -> SimCmdWire {
        SimCmdWire::Settled { action: Box::new(cmd), timeout_ms: 0 }
    }

    /// The verbs that inject input (and take the per-device input turn) —
    /// settled or not.
    #[test]
    fn only_input_verbs_count_as_input() {
        assert!(is_input(&SimCmdWire::Type { text: "a".into(), paste: false }));
        assert!(!is_input(&SimCmdWire::Screenshot { full: false }));
        assert!(!is_input(&SimCmdWire::Ax { max: 10 }));
        let tap = SimCmdWire::Tap(SimTargetWire::Ref(3));
        assert!(is_input(&settled(tap.clone())), "a settled tap takes the input turn");
        let verb = unwrap(SimCmdWire::AsCaller { caller: 1, cmd: Box::new(settled(tap)) }).unwrap();
        assert!(is_input(&verb.inner) && verb.settle == Some(0) && verb.caller == Some(1));
        assert!(!is_input(&SimCmdWire::Wait { target: SimTargetWire::Id("a".into()), gone: false, timeout_ms: 1 }));
    }

    /// Envelopes come off in one order, once each, around the verbs they fit.
    #[test]
    fn envelopes_unwrap_in_one_order_only() {
        let tap = || SimCmdWire::Tap(SimTargetWire::Label("OK".into()));
        assert_eq!(unwrap(tap()).unwrap(), Verb { caller: None, settle: None, inner: tap() });
        let bad = |cmd| matches!(unwrap(cmd), Err(SimErrorWire::BadInput(_)));
        assert!(bad(settled(settled(tap()))), "nested settle");
        assert!(bad(settled(SimCmdWire::AsCaller { caller: 1, cmd: Box::new(tap()) })), "caller inside settle");
        assert!(bad(SimCmdWire::AsCaller { caller: 1, cmd: Box::new(SimCmdWire::AsCaller { caller: 2, cmd: Box::new(tap()) }) }));
        assert!(bad(settled(SimCmdWire::Screenshot { full: false })), "not an action");
        assert!(bad(settled(SimCmdWire::Install { path: "a.app".into() })));
        assert!(bad(SimCmdWire::AsCaller { caller: 1, cmd: Box::new(SimCmdWire::Status) }), "not a control verb");
        for action in [
            SimCmdWire::Rotate(SimOrientationWire::Portrait),
            SimCmdWire::Launch { bundle_id: "a.b".into(), relaunch: false },
            SimCmdWire::OpenUrl { url: "https://a".into() },
        ] {
            assert!(unwrap(settled(action)).is_ok());
        }
    }

    /// A settled verb is refused exactly like the bare one, and also where
    /// the tree its settle reads is missing.
    #[test]
    fn a_settled_verb_meets_the_bare_verbs_guards() {
        let phone = DeviceCaps::for_id(&DeviceId("adb:R58".into()));
        let rotate = SimCmdWire::Rotate(SimOrientationWire::LandscapeLeft);
        assert_eq!(refused_on(&phone, &rotate), Some("Rotation"));
        let verb = unwrap(settled(rotate)).unwrap();
        assert_eq!(refused_on(&phone, &verb.inner), Some("Rotation"));
        // A real iPhone without its runner: no touch, no tree.
        let iphone = DeviceCaps::for_id(&DeviceId("iosdev:00008110-001A2C3E0A88401E".into()));
        assert_eq!(refused_on(&iphone, &SimCmdWire::Tap(SimTargetWire::Ref(1))), Some("Touch input"));
        assert_eq!(refused_on(&iphone, &settled(SimCmdWire::OpenUrl { url: "https://a".into() })), Some(AX));
        assert_eq!(refused_on(&iphone, &SimCmdWire::Assert { target: SimTargetWire::Id("a".into()), check: SimCheckWire::Exists }), Some(AX));
        // `tel:` is refused on a real device by the inner verb's URL check,
        // which reads `verb.inner` — so the envelope cannot carry it past.
        let tel = unwrap(settled(SimCmdWire::OpenUrl { url: "tel:+15555550100".into() })).unwrap();
        assert!(matches!(&tel.inner, SimCmdWire::OpenUrl { url } if !is_web_url(url)));
    }

    /// A real device opens web links only: a custom scheme can dial, pay or
    /// message on someone's own phone.
    #[test]
    fn only_web_links_open_on_a_real_device() {
        for ok in ["https://example.com", "HTTP://localhost:3000/x", "  http://a"] {
            assert!(is_web_url(ok), "{ok}");
        }
        for refused in ["tel:+15555550100", "sms:1", "myapp://pay?amount=1", "httpx://a", "file:///etc", "https"] {
            assert!(!is_web_url(refused), "{refused}");
        }
    }

    /// Each class refuses what it cannot do, named for one refusal wording;
    /// an iPhone without its runner is not touched or typed into.
    #[test]
    fn real_devices_refuse_verbs_their_class_lacks() {
        let phone = DeviceCaps::for_id(&DeviceId("adb:R58".into()));
        assert_eq!(refused_on(&phone, &SimCmdWire::Shutdown { force: true }), Some("Shutting down"));
        assert_eq!(refused_on(&phone, &SimCmdWire::Rotate(SimOrientationWire::LandscapeLeft)), Some("Rotation"));
        assert_eq!(refused_on(&phone, &SimCmdWire::Button(SimButtonWire::Siri)), Some("That button"));
        assert_eq!(refused_on(&phone, &SimCmdWire::Button(SimButtonWire::Back)), None);
        assert_eq!(refused_on(&phone, &SimCmdWire::Install { path: "a.apk".into() }), None);
        let iphone = DeviceCaps::for_id(&DeviceId("iosdev:00008110-001A2C3E0A88401E".into()));
        assert_eq!(refused_on(&iphone, &SimCmdWire::Type { text: "hi".into(), paste: false }), Some("Typing"));
        assert_eq!(refused_on(&iphone, &SimCmdWire::Ax { max: 10 }), Some(AX));
        assert_eq!(refused_on(&iphone, &SimCmdWire::Screenshot { full: false }), None);
        // With its control on, an iPhone takes input and gives its tree, but
        // never turns or shuts down from here.
        let controlled = DeviceCaps::for_session(&DeviceId("iosdev:00008110-001A2C3E0A88401E".into()), true);
        for cmd in [
            SimCmdWire::Type { text: "hi".into(), paste: false },
            SimCmdWire::Ax { max: 10 },
            SimCmdWire::Button(SimButtonWire::Home),
            SimCmdWire::Button(SimButtonWire::VolumeUp),
            SimCmdWire::Tap(SimTargetWire::Ref(2)),
            settled(SimCmdWire::Tap(SimTargetWire::Label("OK".into()))),
        ] {
            assert_eq!(refused_on(&controlled, &cmd), None, "{cmd:?}");
        }
        assert_eq!(refused_on(&controlled, &SimCmdWire::Rotate(SimOrientationWire::Portrait)), Some("Rotation"));
        assert_eq!(refused_on(&controlled, &SimCmdWire::Button(SimButtonWire::Lock)), Some("That button"));
        assert_eq!(refused_on(&controlled, &SimCmdWire::Shutdown { force: false }), Some("Shutting down"));
    }

    /// While a phone sleeps only the screen verbs wait on it; app verbs go
    /// through adb either way — unless settled, which reads the screen.
    #[test]
    fn only_screen_verbs_need_the_screen() {
        assert!(needs_screen(&SimCmdWire::Screenshot { full: false }) && needs_screen(&SimCmdWire::Ax { max: 1 }));
        assert!(!needs_screen(&SimCmdWire::Launch { bundle_id: "a.b".into(), relaunch: false }));
        assert!(!needs_screen(&SimCmdWire::Install { path: "a.apk".into() }) && !needs_screen(&SimCmdWire::Status));
        assert!(needs_screen(&settled(SimCmdWire::Launch { bundle_id: "a.b".into(), relaunch: false })));
        assert!(needs_screen(&SimCmdWire::AxRefs { max: 1 }));
    }
}
