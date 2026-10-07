//! The app verbs on a real iPhone (`devicectl`, where a simulator uses
//! `simctl`): launch an app, open a web page, install a built `.app`. The
//! screen verbs go through the capture helper's session, as on a simulator;
//! touch, keys and the AX tree wait for the control runner.

use std::path::{Path, PathBuf};
use std::time::Duration;

use oximux_remote_proto::simulator::SimErrorWire;
use oximux_simulator::runner::SystemRunner;
use oximux_simulator::{DeviceId, devicectl};

use crate::shell::simulator::hub::SimulatorHub;

const APP_TIMEOUT: Duration = Duration::from_secs(60);
/// A device install copies the whole app over USB.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// A real iPhone that `devicectl` can reach now.
pub(super) struct Device(DeviceId);

impl Device {
    /// `udid`, when Xcode is there and the last listing saw it cabled and
    /// trusting this Mac (so an unplugged phone is not asked anything).
    pub(super) fn reachable(hub: &SimulatorHub, udid: &DeviceId) -> Result<Self, SimErrorWire> {
        if !hub.xcode_ok() {
            return Err(SimErrorWire::Unavailable("Xcode was not found; see `oximux sim status`".into()));
        }
        match hub.devices().iter().find(|d| &d.udid == udid) {
            Some(d) if d.is_available => Ok(Self(udid.clone())),
            Some(d) => Err(SimErrorWire::Unavailable(d.note.clone().unwrap_or_else(|| "the iPhone cannot be reached".into()))),
            None => Err(SimErrorWire::Unavailable("the iPhone is not connected; ask the user to plug it in".into())),
        }
    }

    pub(super) fn launch(&self, bundle_id: &str, relaunch: bool) -> Result<(), SimErrorWire> {
        devicectl::launch(&SystemRunner, &self.0, bundle_id, relaunch, None, APP_TIMEOUT)
            .map_err(|e| SimErrorWire::Failed(format!("launch failed: {e}")))
    }

    /// The caller has checked `url` is a web link.
    pub(super) fn open_url(&self, url: &str) -> Result<(), SimErrorWire> {
        devicectl::open_url(&SystemRunner, &self.0, url, APP_TIMEOUT)
            .map_err(|e| SimErrorWire::Failed(format!("could not open the URL: {e}")))
    }

    pub(super) fn install(&self, app: &Path) -> Result<(), SimErrorWire> {
        devicectl::install(&SystemRunner, &self.0, app, INSTALL_TIMEOUT)
            .map_err(|e| SimErrorWire::Failed(format!("install failed: {e}")))
    }
}

/// An iPhone installs a device build (`*-iphoneos`), never a simulator's.
pub(super) fn device_build(app: PathBuf) -> Result<PathBuf, SimErrorWire> {
    let simulator_build = app.components().any(|c| c.as_os_str().to_string_lossy().ends_with("-iphonesimulator"));
    if simulator_build {
        return Err(SimErrorWire::BadInput(format!(
            "{} is a simulator build; build for the device (`-destination generic/platform=iOS`)",
            app.display()
        )));
    }
    Ok(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_simulator_build_is_not_installed_on_an_iphone() {
        let sim = PathBuf::from("/d/Build/Products/Debug-iphonesimulator/My.app");
        assert!(matches!(device_build(sim), Err(SimErrorWire::BadInput(_))));
        let device = PathBuf::from("/d/Build/Products/Debug-iphoneos/My.app");
        assert_eq!(device_build(device.clone()).unwrap(), device);
    }
}
