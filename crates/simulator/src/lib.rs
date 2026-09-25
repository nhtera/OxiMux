//! gpui-free core of the iOS Simulator panel.
//!
//! The panel streams and drives a booted iOS Simulator through
//! `oximux-sim-helper`, a stdio-only child built and released by our fork of
//! serve-sim (`nhtera/serve-sim`, branch `oximux`) and bundled beside the app
//! binary. This crate owns everything below the UI:
//!
//! - [`protocol`]: the helper's framed stdin/stdout wire format.
//! - [`helper`] / [`session`]: spawning and supervising the helper; the
//!   latest-frame-wins stream and the input/request API the UI and agent
//!   verbs share.
//! - [`simctl`] / [`availability`]: device discovery and lifecycle via
//!   `xcrun simctl`, gated so a Mac without Xcode never runs `xcrun`.
//! - [`record`]: screen recordings (`simctl io recordVideo`, stopped with
//!   `SIGINT` so the movie is finalized).
//! - [`child_ledger`]: a crash-safe record of the children we spawned, so a
//!   killed app does not leave helpers or recordings behind.
//! - [`geometry`], [`keyboard`], [`gesture`], [`ax`], [`classify`]: pure math
//!   and parsing, unit-tested without a simulator.
//!
//! No sockets and no async runtime: calls that can block (`simctl`, spawn,
//! readiness) are plain blocking functions meant for a background executor,
//! and no lock is held across them.

use serde::{Deserialize, Serialize};

pub mod availability;
pub mod ax;
pub mod boot_watch;
pub mod child_ledger;
pub mod classify;
pub mod geometry;
pub mod gesture;
pub mod helper;
pub mod keyboard;
pub mod protocol;
pub mod record;
pub mod registry;
pub mod runner;
pub mod session;
pub mod simctl;

/// A simulator's UDID, as `simctl` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A device's lifecycle state, from `simctl list devices -j` (`state`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceState {
    Shutdown,
    Booting,
    Booted,
    ShuttingDown,
    Creating,
    /// A state this build does not know; kept verbatim for diagnostics.
    Other(String),
}

impl DeviceState {
    pub fn from_simctl(s: &str) -> Self {
        match s {
            "Shutdown" => Self::Shutdown,
            "Booting" => Self::Booting,
            "Booted" => Self::Booted,
            "Shutting Down" => Self::ShuttingDown,
            "Creating" => Self::Creating,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// The device family, which decides the bezel and default orientation rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Phone,
    Tablet,
    /// Watch, TV, Vision — listed by `simctl`, not supported by the panel.
    Other,
}

/// One simulator device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub udid: DeviceId,
    pub name: String,
    /// Runtime identifier, e.g. `com.apple.CoreSimulator.SimRuntime.iOS-26-3`.
    pub runtime: String,
    /// Human version derived from the runtime, e.g. `26.3`.
    pub os_version: String,
    pub state: DeviceState,
    pub kind: DeviceKind,
    /// `simctl`'s `isAvailable`: false when the runtime is missing.
    pub is_available: bool,
}

/// Device orientation, numbered as UIKit's `UIDeviceOrientation` and as the
/// helper's `configure{orientation}` expects.
///
/// The simulator framebuffer never rotates: in landscape the UI is drawn
/// sideways inside the portrait buffer. The helper rotates frames for display
/// by the *device* orientation (matching Simulator.app, which also shows a
/// portrait-only app sideways on a rotated device), and HID touches are always
/// in portrait space — see [`geometry::display_to_portrait`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Orientation {
    Portrait = 1,
    PortraitUpsideDown = 2,
    /// Device turned counter-clockwise (home side on the right).
    LandscapeLeft = 3,
    /// Device turned clockwise (home side on the left).
    LandscapeRight = 4,
}

impl Orientation {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Portrait),
            2 => Some(Self::PortraitUpsideDown),
            3 => Some(Self::LandscapeLeft),
            4 => Some(Self::LandscapeRight),
            _ => None,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn is_landscape(self) -> bool {
        matches!(self, Self::LandscapeLeft | Self::LandscapeRight)
    }

    /// Simulator.app's "Rotate Left" (⌘←): the device turns counter-clockwise.
    pub fn rotated_left(self) -> Self {
        match self {
            Self::Portrait => Self::LandscapeLeft,
            Self::LandscapeLeft => Self::PortraitUpsideDown,
            Self::PortraitUpsideDown => Self::LandscapeRight,
            Self::LandscapeRight => Self::Portrait,
        }
    }

    /// Simulator.app's "Rotate Right" (⌘→): the device turns clockwise.
    pub fn rotated_right(self) -> Self {
        match self {
            Self::Portrait => Self::LandscapeRight,
            Self::LandscapeRight => Self::PortraitUpsideDown,
            Self::PortraitUpsideDown => Self::LandscapeLeft,
            Self::LandscapeLeft => Self::Portrait,
        }
    }
}

/// Hardware buttons the helper can press. Wire names match the helper's
/// `button{name}` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Home,
    Lock,
    Siri,
    SideButton,
    AppSwitcher,
    SwipeHome,
}

impl Button {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Lock => "lock",
            Self::Siri => "siri",
            Self::SideButton => "side_button",
            Self::AppSwitcher => "app_switcher",
            Self::SwipeHome => "swipe_home",
        }
    }
}

/// Everything that can go wrong below the panel. Messages are written for a
/// person: the UI shows them verbatim.
#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("Xcode is not installed or not selected (xcode-select -p failed)")]
    XcodeMissing,
    #[error("the iOS Simulator panel is not supported here: {0}")]
    Unsupported(String),
    #[error("simulator helper not found: {0}")]
    HelperNotFound(String),
    #[error("simulator helper speaks protocol {got}, this app expects {expected}; update OxiMux")]
    HelperIncompatible { expected: u32, got: u32 },
    #[error("the simulator helper could not load Xcode's simulator frameworks: {0}")]
    FrameworkLoadFailed(String),
    #[error("simulator helper failed: {0}")]
    HelperFailed(String),
    #[error("simulator helper exited (code {code:?})")]
    HelperExited { code: Option<i32> },
    #[error("simulator helper protocol error: {0}")]
    Protocol(String),
    #[error("device {0} not found")]
    DeviceNotFound(String),
    #[error("device is not booted")]
    DeviceNotBooted,
    #[error("{program} failed (exit {code:?}): {stderr}")]
    CommandFailed { program: String, code: Option<i32>, stderr: String },
    #[error("{what} timed out after {secs}s")]
    Timeout { what: String, secs: u64 },
    #[error("cancelled")]
    Cancelled,
    #[error("could not parse {what}: {detail}")]
    Parse { what: String, detail: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = SimError> = std::result::Result<T, E>;
