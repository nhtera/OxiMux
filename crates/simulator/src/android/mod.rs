//! Android devices for the same panel (P10): emulators (AVDs) and phones with
//! USB debugging, streamed and driven through a scrcpy server we push to the
//! device and speak to ourselves — no scrcpy client binary.
//!
//! - [`sdk`]: finding the Android SDK (`adb`, `emulator`).
//! - [`adb`] / [`avd`]: `adb` and `emulator` as data, behind the same
//!   [`Runner`](crate::runner::Runner) seam `simctl` uses.
//! - [`scrcpy_control`] / [`scrcpy_video`]: the scrcpy 4.1 wire formats, byte
//!   for byte (the client and server must match exactly; see
//!   [`scrcpy_server::VERSION`]).
//!
//! Like the rest of the crate: no async runtime; blocking calls are meant for
//! a background executor.

pub mod adb;
pub mod avd;
pub mod devices;
pub mod input;
pub mod keycode;
pub mod scrcpy_control;
pub mod scrcpy_server;
pub mod scrcpy_video;
pub mod sdk;
pub mod session;
pub mod uiautomator;

use crate::DeviceId;

const AVD_PREFIX: &str = "avd:";
const ADB_PREFIX: &str = "adb:";

/// The Android device a [`DeviceId`] names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// An emulator, by AVD name. Its adb serial (`emulator-5554`) is only
    /// known while it runs, and may differ from one boot to the next.
    Avd(String),
    /// A phone (or any device adb lists that is not one of our emulators), by
    /// adb serial.
    Serial(String),
}

impl Target {
    /// `None` for an iOS UDID.
    pub fn from_id(id: &DeviceId) -> Option<Self> {
        let id = id.as_str();
        if let Some(name) = id.strip_prefix(AVD_PREFIX).filter(|n| !n.is_empty()) {
            return Some(Self::Avd(name.to_owned()));
        }
        id.strip_prefix(ADB_PREFIX).filter(|s| !s.is_empty()).map(|serial| Self::Serial(serial.to_owned()))
    }

    pub fn id(&self) -> DeviceId {
        match self {
            Self::Avd(name) => DeviceId(format!("{AVD_PREFIX}{name}")),
            Self::Serial(serial) => DeviceId(format!("{ADB_PREFIX}{serial}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Platform;

    #[test]
    fn ids_round_trip_and_name_their_platform() {
        for target in [Target::Avd("Medium_Phone".into()), Target::Serial("R58M123ABC".into())] {
            let id = target.id();
            assert_eq!(Target::from_id(&id), Some(target));
            assert_eq!(id.platform(), Platform::Android);
        }
        let udid = DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into());
        assert_eq!(Target::from_id(&udid), None);
        assert_eq!(udid.platform(), Platform::Ios, "ids saved before Android existed stay iOS");
        assert_eq!(Target::from_id(&DeviceId("avd:".into())), None, "an empty name is not a device");
    }
}
