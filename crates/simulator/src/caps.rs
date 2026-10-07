//! What a device can do, by class: a simulator, an Android emulator, an
//! Android phone, a real iPhone (view-only, or with its control runner).
//!
//! [`DeviceCaps::for_id`] is the class default — a hint for the device menu
//! and the toolbar before anything runs. [`DeviceCaps::for_session`] is the
//! runtime truth: an iPhone can be touched only while its runner is up. A verb
//! or button a device lacks is refused with [`refuse`], one wording for every
//! path.

use crate::{DeviceId, Source};

/// Hardware buttons, as a small set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ButtonSet(u16);

impl ButtonSet {
    pub const NONE: Self = Self(0);
    pub const HOME: Self = Self(1);
    pub const LOCK: Self = Self(1 << 1);
    pub const SIRI: Self = Self(1 << 2);
    pub const SIDE: Self = Self(1 << 3);
    pub const APP_SWITCHER: Self = Self(1 << 4);
    pub const BACK: Self = Self(1 << 5);
    pub const VOLUME_UP: Self = Self(1 << 6);
    pub const VOLUME_DOWN: Self = Self(1 << 7);
    /// The iPhone's Action button (the panel only: no agent verb names it).
    pub const ACTION: Self = Self(1 << 8);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Everything the panel and the agent verbs may ask of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceCaps {
    pub touch: bool,
    pub multitouch: bool,
    pub keys: bool,
    pub buttons: ButtonSet,
    pub rotate: bool,
    pub ax: bool,
    /// OxiMux may shut it down (never a real device).
    pub shutdown: bool,
    pub record: bool,
    pub screenshot: bool,
    pub logs: bool,
    pub install: bool,
    pub launch: bool,
    pub open_url: bool,
}

const SIMULATOR_BUTTONS: ButtonSet =
    ButtonSet::HOME.union(ButtonSet::LOCK).union(ButtonSet::SIRI).union(ButtonSet::SIDE).union(ButtonSet::APP_SWITCHER);
const ANDROID_BUTTONS: ButtonSet = ButtonSet::HOME
    .union(ButtonSet::BACK)
    .union(ButtonSet::APP_SWITCHER)
    .union(ButtonSet::LOCK)
    .union(ButtonSet::VOLUME_UP)
    .union(ButtonSet::VOLUME_DOWN);
const IPHONE_RUNNER_BUTTONS: ButtonSet =
    ButtonSet::HOME.union(ButtonSet::VOLUME_UP).union(ButtonSet::VOLUME_DOWN).union(ButtonSet::ACTION);

impl DeviceCaps {
    const ALL: Self = Self {
        touch: true,
        multitouch: true,
        keys: true,
        buttons: SIMULATOR_BUTTONS,
        rotate: true,
        ax: true,
        shutdown: true,
        record: true,
        screenshot: true,
        logs: true,
        install: true,
        launch: true,
        open_url: true,
    };

    /// A real iPhone without its runner: the screen-capture device and
    /// `devicectl` only — it can be watched, recorded and have apps put on
    /// it, not touched.
    const IPHONE_VIEW_ONLY: Self = Self {
        touch: false,
        multitouch: false,
        keys: false,
        buttons: ButtonSet::NONE,
        rotate: false,
        ax: false,
        shutdown: false,
        record: true,
        screenshot: true,
        logs: false,
        install: true,
        launch: true,
        open_url: true,
    };

    /// The class default for `id` (an iPhone reads as view-only: whether its
    /// runner is up is a runtime fact, see [`Self::for_session`]).
    pub fn for_id(id: &DeviceId) -> Self {
        Self::for_session(id, false)
    }

    /// What `id` can do now; `runner`: a real iPhone's control runner is up.
    pub fn for_session(id: &DeviceId, runner: bool) -> Self {
        match id.source() {
            Source::Simctl => Self::ALL,
            // A phone is the user's own: OxiMux never rotates (it follows
            // the phone) or shuts it down.
            Source::Adb if id.is_physical() => Self { buttons: ANDROID_BUTTONS, rotate: false, shutdown: false, ..Self::ALL },
            Source::Adb => Self { buttons: ANDROID_BUTTONS, ..Self::ALL },
            Source::Devicectl if runner => Self {
                touch: true,
                keys: true,
                buttons: IPHONE_RUNNER_BUTTONS,
                ax: true,
                ..Self::IPHONE_VIEW_ONLY
            },
            Source::Devicectl => Self::IPHONE_VIEW_ONLY,
        }
    }
}

/// Why `what` (a capability, e.g. "Rotation") is refused on `id`. One
/// wording for the toolbar, the palette and the agent verbs.
pub fn refuse(what: &str, id: &DeviceId) -> String {
    let device = match id.source() {
        Source::Devicectl => "a real iPhone (without its control runner)",
        Source::Adb if id.is_physical() => "a real phone",
        Source::Adb => "this emulator",
        Source::Simctl => "this simulator",
    };
    format!("{what} is not available on {device}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> DeviceId {
        DeviceId(s.into())
    }

    #[test]
    fn each_class_has_its_own_defaults() {
        let sim = DeviceCaps::for_id(&id("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3"));
        assert!(sim.shutdown && sim.rotate && sim.touch && sim.buttons.contains(ButtonSet::SIRI));

        let avd = DeviceCaps::for_id(&id("avd:Pixel"));
        assert!(avd.shutdown && avd.rotate && avd.buttons.contains(ButtonSet::BACK));

        let phone = DeviceCaps::for_id(&id("adb:R58M123"));
        assert!(!phone.shutdown && !phone.rotate, "never rotated or shut down");
        assert!(phone.touch && phone.keys && phone.ax && phone.logs && phone.install && phone.open_url);

        let iphone = DeviceCaps::for_id(&id("iosdev:00008110-001A2C3E0A88401E"));
        assert!(iphone.screenshot && iphone.record && iphone.launch && iphone.install && iphone.open_url);
        assert!(!iphone.touch && !iphone.keys && !iphone.rotate && !iphone.ax && !iphone.shutdown && !iphone.logs);
        assert_eq!(iphone.buttons, ButtonSet::NONE);
    }

    #[test]
    fn an_iphones_runner_adds_single_finger_input() {
        let iphone = DeviceCaps::for_session(&id("iosdev:X"), true);
        assert!(iphone.touch && iphone.keys && iphone.ax && !iphone.multitouch);
        assert!(iphone.buttons.contains(ButtonSet::HOME.union(ButtonSet::VOLUME_UP).union(ButtonSet::ACTION)));
        assert!(!iphone.buttons.contains(ButtonSet::SIRI) && !iphone.rotate && !iphone.shutdown);
    }

    #[test]
    fn refusals_name_the_device_class() {
        assert_eq!(refuse("Rotation", &id("adb:R58")), "Rotation is not available on a real phone");
        assert!(refuse("Touch", &id("iosdev:X")).contains("real iPhone"));
    }
}
