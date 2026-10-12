//! The iOS Simulator surface (v25): `oximux sim …` verbs an agent uses to drive
//! the simulator attached to its worktree in the desktop app.
//!
//! One request variant ([`Request::Simulator`](crate::proto::Request::Simulator))
//! carries every verb as a [`SimCmdWire`], and one reply
//! ([`Response::Simulator`](crate::proto::Response::Simulator)) carries a
//! `Result`, so the verb set can grow by appending to these enums without
//! touching the envelope again. The same append-only rule applies: postcard
//! encodes every enum here by ordinal and every struct by field position.
//!
//! Errors a caller acts on differently — waiting for the user's consent, being
//! refused, the device not being there — are [`SimErrorWire`] variants rather
//! than prose inside `RpcError::BadRequest`, so the CLI can map them to distinct
//! exit codes. Authorization and capability failures stay `RpcError`
//! (`Unauthorized`, `Unsupported`), like every other surface.
//!
//! **Coordinates are points** (UIKit's logical units), in the screen's current
//! orientation — the space the accessibility tree reports frames in, and the
//! one that stays put across devices and stream resolutions. A default
//! screenshot is rendered at one pixel per point, so a position read off the
//! image is already a tap coordinate; a full-resolution one says how many
//! pixels make a point ([`SimReplyWire::Screenshot::scale`]).

use serde::{Deserialize, Serialize};

pub use crate::simulator_verify::{
    SimCheckWire, SimDiffLineWire, SimDiffOpWire, SimRefNodeWire, SimSettleOutcomeWire, SimSettleWire,
};

/// [`Request::Simulator`](crate::proto::Request::Simulator) payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimRequestWire {
    /// The worktree whose simulator to drive, as a path inside it. The host
    /// resolves it to a worktree it knows. **Ignored for a session-scoped
    /// caller**, whose worktree is its session's own working directory — a
    /// confined agent cannot aim at another project's device.
    pub worktree: Option<String>,
    pub cmd: SimCmdWire,
}

/// One simulator verb. Everything from [`SimCmdWire::Screenshot`] on is a
/// **control** verb: it needs the user's per-device consent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimCmdWire {
    /// Availability, the attached device and its consent state.
    Status,
    /// Every simulator device on the machine.
    Devices,
    /// Attach a device (a name or udid; `None` picks one), booting it if needed.
    Attach { device: Option<String> },
    Detach,
    /// A PNG of the screen: one pixel per point, or the device's full
    /// resolution when `full`.
    Screenshot { full: bool },
    /// The accessibility tree, flattened depth-first, at most `max` nodes.
    Ax { max: u32 },
    /// Tap a point, or the centre of the element a label or identifier names.
    Tap(SimTargetWire),
    Swipe { from: SimPointWire, to: SimPointWire, duration_ms: u32 },
    /// Type text. ASCII goes key by key; `paste` (or any non-ASCII) goes
    /// through the device clipboard and ⌘V.
    Type { text: String, paste: bool },
    Button(SimButtonWire),
    Rotate(SimOrientationWire),
    /// Launch an installed app; `relaunch` terminates it first.
    Launch { bundle_id: String, relaunch: bool },
    /// Open an `http(s)` or custom-scheme URL. `file:` is refused.
    OpenUrl { url: String },
    /// Install a built `.app`. The path must be inside the caller's worktree or
    /// Xcode's DerivedData.
    Install { path: String },
    /// Shut the device down. A device the user booted is refused unless
    /// `force`.
    Shutdown { force: bool },
    // v27: the verify loop.
    /// Run `action` (tap, swipe, type, button, rotate, launch, open-url), wait
    /// up to `timeout_ms` (0: the device class's default) for the screen to go
    /// idle, and report what changed.
    Settled {
        #[serde(deserialize_with = "crate::simulator_verify::nested")]
        action: Box<SimCmdWire>,
        timeout_ms: u32,
    },
    /// The elements that can be acted on, at most `max`, each with its `@eN`
    /// ref.
    AxRefs { max: u32 },
    /// Wait up to `timeout_ms` for an element to appear (or, `gone`, to go).
    Wait { target: SimTargetWire, gone: bool, timeout_ms: u32 },
    /// Check an element once.
    Assert { target: SimTargetWire, check: SimCheckWire },
    /// `cmd` on behalf of one caller (a hash of its agent session), whose
    /// refs are its own. Outermost only.
    AsCaller {
        caller: u64,
        #[serde(deserialize_with = "crate::simulator_verify::nested")]
        cmd: Box<SimCmdWire>,
    },
    /// Tap `target`, then type `text` into it, in one turn: no other input
    /// lands between the two (`type @eN TEXT`).
    TypeInto { target: SimTargetWire, text: String, paste: bool },
}

impl SimCmdWire {
    /// Needs the user's consent for the device (everything that looks at or
    /// changes it); `Status`, `Devices`, `Attach` and `Detach` do not.
    pub fn is_control(&self) -> bool {
        !matches!(self, Self::Status | Self::Devices | Self::Attach { .. } | Self::Detach)
    }
}

/// A position in points.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SimPointWire {
    pub x: f64,
    pub y: f64,
}

/// What a tap aims at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimTargetWire {
    Point(SimPointWire),
    /// An accessibility label: exact match first, then case-insensitive
    /// substring.
    Label(String),
    /// An accessibility identifier, exact.
    Id(String),
    /// v27: an `@eN` ref from `ax -i` or a settle diff, re-resolved when used.
    Ref(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimButtonWire {
    Home,
    Lock,
    Siri,
    SideButton,
    AppSwitcher,
    // v26: Android's buttons (an iOS device answers `BadInput`).
    Back,
    VolumeUp,
    VolumeDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimOrientationWire {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
    UpsideDown,
}

/// [`Response::Simulator`](crate::proto::Response::Simulator)'s success value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimReplyWire {
    Status(SimStatusWire),
    Devices(Vec<SimDeviceWire>),
    Attached(SimDeviceWire),
    /// The verb was carried out; nothing to report.
    Done,
    /// `width`×`height` pixels; `scale` pixels per point (1 for a default
    /// screenshot).
    Screenshot { png: Vec<u8>, width: u32, height: u32, scale: f64 },
    Ax(Vec<SimAxNodeWire>),
    // v27: the verify loop.
    Settled(SimSettleWire),
    AxRefs(Vec<SimRefNodeWire>),
    /// A `wait` or `assert` held: the element (`None` once gone).
    Matched { node: Option<SimAxNodeWire>, elapsed_ms: u32 },
}

/// One simulator device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimDeviceWire {
    pub udid: String,
    pub name: String,
    /// e.g. `iOS 26.0`.
    pub runtime: String,
    /// `Booted`, `Shutdown`, … as `simctl` reports it.
    pub state: String,
}

/// Where the user stands on letting agents control a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SimConsentWire {
    /// Nobody has asked yet.
    NotAsked,
    /// Asked; the user has not answered.
    Pending,
    Allowed,
    /// The user said no; asking again is possible after this many seconds.
    Denied { retry_after_secs: u64 },
}

/// Reply to [`SimCmdWire::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimStatusWire {
    /// The simulator can be used at all (Xcode, a runtime, the helper).
    pub available: bool,
    /// Why not, when it cannot.
    pub reason: Option<String>,
    /// The active Xcode's version, when found.
    pub xcode: Option<String>,
    /// The worktree the request resolved to.
    pub worktree: String,
    /// The device attached to that worktree.
    pub device: Option<SimDeviceWire>,
    /// Its screen is streaming (touch, keys and the AX tree need that).
    pub streaming: bool,
    /// The consent state for `device` (`NotAsked` with no device).
    pub consent: SimConsentWire,
    /// The global switch for agent control is on.
    pub agent_control: bool,
}

/// One accessibility node, flattened. `frame` is `[x, y, width, height]` in
/// points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimAxNodeWire {
    pub depth: u32,
    pub role: String,
    pub label: Option<String>,
    pub identifier: Option<String>,
    pub value: Option<String>,
    pub enabled: bool,
    pub frame: [f64; 4],
}

/// Why a simulator verb did not happen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimErrorWire {
    /// The user has been asked to let agents control this device and has not
    /// answered yet. Not a failure: wait (`sim wait-consent`) and retry.
    ConsentPending,
    /// The user said no. Asking again is possible after this many seconds.
    ConsentDenied { retry_after_secs: u64 },
    /// Agent control of the simulator is switched off in Settings.
    AgentControlDisabled,
    /// No device is attached to the worktree (or the path names no worktree
    /// the desktop knows).
    NoDevice,
    /// The verb needs the live screen stream, which is not running yet.
    NotStreaming,
    /// The simulator cannot be used on this machine right now.
    Unavailable(String),
    /// An install path outside the caller's worktree and DerivedData.
    PathOutsideWorktree,
    /// Nothing matched (a device name, an accessibility label).
    NotFound(String),
    /// The request itself is unusable (a `file:` URL, an empty text).
    BadInput(String),
    /// The device refused or the operation failed.
    Failed(String),
    /// Not done on the user's behalf without being asked (shutting down a
    /// device the user booted): the message says how to ask explicitly.
    Refused(String),
    // v27: the verify loop's failures (only ever for v27 verbs).
    /// An `@eN` ref whose element is gone, or no longer told apart.
    StaleRef(String),
    /// `assert` did not hold: the message says what was found.
    AssertFailed(String),
    /// `wait` ran out.
    TimedOut(String),
}

impl std::fmt::Display for SimErrorWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConsentPending => f.write_str("waiting for the user to allow agent control of this device"),
            Self::ConsentDenied { retry_after_secs } => write!(
                f,
                "the user did not allow agent control of this device (ask again in {} min)",
                retry_after_secs.div_ceil(60)
            ),
            Self::AgentControlDisabled => f.write_str("agent control of devices is turned off in OxiMux Settings"),
            Self::NoDevice => f.write_str("no device is attached to this worktree"),
            Self::NotStreaming => f.write_str("the device's screen is not streaming yet"),
            Self::Unavailable(why) => write!(f, "the device is unavailable: {why}"),
            Self::PathOutsideWorktree => {
                f.write_str("the app must be inside this worktree or Xcode's DerivedData")
            }
            Self::NotFound(what) => write!(f, "not found: {what}"),
            Self::BadInput(why) => f.write_str(why),
            Self::Failed(why) | Self::Refused(why) => f.write_str(why),
            Self::StaleRef(why) | Self::AssertFailed(why) | Self::TimedOut(why) => f.write_str(why),
        }
    }
}
