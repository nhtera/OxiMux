//! Device-state verbs' wire types (v28): what `sim location`, `appearance`,
//! `privacy`, `push`, `status-bar`, `erase` (and, from a later version on,
//! `net`) send inside [`SimCmdWire::DeviceState`](crate::simulator::SimCmdWire::DeviceState).
//!
//! Re-exported from [`crate::simulator`]. Every type is new in v28, so its
//! shape is fixed from its first release (postcard: structs by field
//! position, enums by ordinal). Grow by minting a new type or appending a
//! variant at the end, never by adding a field.

use serde::{Deserialize, Serialize};

/// One change to a device's state. Each needs the user's consent for the
/// device; erase and push are also asked every time, as is every change to a
/// real device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimStateCmdWire {
    Location(SimLocationWire),
    /// Dark or light mode.
    Appearance { dark: bool },
    /// Grant, revoke or reset `app`'s permission for `service` (`location`,
    /// `photos`, `camera`, …: see `oximux sim privacy --help`).
    Privacy { action: SimPrivacyActionWire, service: String, app: String },
    /// Deliver a push notification: `payload` is the JSON (an object with
    /// `aps`, at most 4096 bytes).
    Push { app: String, payload: String },
    /// A clean status bar for screenshots (9:41, full battery and signal),
    /// or the real one back.
    StatusBar { clean: bool },
    /// Wipe the device to factory settings and boot it again. One the user
    /// booted is refused unless `force`.
    Erase { force: bool },
    /// Network shaping. The shape is here so the wire needs no change of its
    /// own when it lands; until then a host refuses it.
    Net(SimNetWire),
}

/// Where the device says it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimLocationWire {
    /// Degrees; named so a longitude-first platform cannot swap them.
    Set { lat: f64, lon: f64 },
    /// Back to no simulated location.
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimPrivacyActionWire {
    Grant,
    Revoke,
    /// Back to "not asked yet", for that app only.
    Reset,
}

/// Network shaping (an Android emulator's speed; on and off on Android).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimNetWire {
    /// A preset (`gsm`, `edge`, `lte`, `full`, …) or a speed in kbit/s, as
    /// the emulator names it.
    Speed(String),
    Offline,
    Online,
}
