//! [`DeviceSession`]: a real iPhone, as a [`StreamSession`] holds it.
//!
//! Its video is the capture helper ([`HelperKind::DeviceCapture`]): a
//! [`HelperSession`] like a simulator's, minus everything a screen-capture
//! device cannot do. Without its control runner it is view-only: input and
//! the accessibility tree are refused here, with the one wording every path
//! uses ([`crate::caps::refuse`]), rather than sent to a helper that would
//! only answer "unsupported". With it ([`DeviceSession::set_control`]),
//! they go to the runner, through the phone's [`control::DeviceControl`].
//!
//! [`StreamSession`]: crate::stream::StreamSession
//! [`HelperKind::DeviceCapture`]: crate::helper::HelperKind::DeviceCapture

#[cfg(target_os = "macos")]
pub mod control;
pub mod input_map;
#[cfg(target_os = "macos")]
pub mod runner_build;
#[cfg(target_os = "macos")]
pub mod runner_client;
#[cfg(target_os = "macos")]
pub mod runner_supervisor;
pub mod snapshot;
#[cfg(target_os = "macos")]
pub mod team;
#[cfg(target_os = "macos")]
pub mod usbmux;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::caps;
use crate::protocol::Command;
use crate::session::HelperSession;
use crate::{Result, SimError};

/// A signing team id as Apple issues them: ten upper-case letters or digits
/// (checked before one reaches an `xcodebuild` argument).
pub fn is_team_id(id: &str) -> bool {
    id.len() == 10 && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A capture session's phone gone mid-stream (the helper's
/// `device_not_connected` after `ready`), in one phrase the registry knows:
/// an iPhone briefly re-enumerating on USB comes straight back, so it gets
/// one automatic restart (`Registry::session_exited`).
pub const PHONE_DROPPED: &str = "the iPhone was unplugged";

/// What to do instead, while OxiMux only shows an iPhone.
pub const ENABLE_CONTROL_HINT: &str = "turn its control on in the Mobile Emulator panel (Control from OxiMux…), or use the phone itself";

#[cfg(target_os = "macos")]
type ControlSlot = Arc<Mutex<Option<Arc<control::DeviceControl>>>>;
/// No runner off macOS: the slot is always empty.
#[cfg(not(target_os = "macos"))]
type ControlSlot = Arc<Mutex<Option<std::convert::Infallible>>>;

#[derive(Clone)]
pub struct DeviceSession {
    video: HelperSession,
    /// The runner's control, while it is on (shared by every clone).
    control: ControlSlot,
}

impl DeviceSession {
    pub fn new(video: HelperSession) -> Self {
        Self { video, control: Arc::default() }
    }

    /// Whether input reaches the phone (its control is on).
    pub fn controlled(&self) -> bool {
        self.control.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// The phone's control, while it is on.
    #[cfg(target_os = "macos")]
    pub fn control(&self) -> Option<Arc<control::DeviceControl>> {
        self.control.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Turns control on (`Some`) or off; the one it replaces, if any.
    #[cfg(target_os = "macos")]
    pub fn set_control(&self, control: Option<Arc<control::DeviceControl>>) -> Option<Arc<control::DeviceControl>> {
        std::mem::replace(&mut *self.control.lock().unwrap_or_else(|e| e.into_inner()), control)
    }

    /// Screen pixels per point, from the screen's width: 3 on every iPhone
    /// at least ~1000 pixels wide (all since the X, bar the XR/11 and the
    /// SE), else 2. `None` until the first frame's size is known.
    pub fn point_scale(&self) -> Option<f64> {
        let (w, h) = self.video.framebuffer_size()?;
        Some(if w.min(h) >= 1000 { 3.0 } else { 2.0 })
    }

    /// The capture helper streaming the phone's screen.
    pub fn video(&self) -> &HelperSession {
        &self.video
    }

    /// Input: to the runner while control is on, else refused.
    pub fn send(&self, command: &Command) -> Result<()> {
        match command {
            Command::Pause | Command::Resume => self.video.send(command),
            #[cfg(target_os = "macos")]
            Command::Touch { .. } | Command::Multitouch { .. } | Command::Key { .. } | Command::Button { .. } if self.controlled() => {
                if let Some(control) = self.control() {
                    control.send(command);
                }
                Ok(())
            }
            _ => Err(self.refused(command)),
        }
    }

    /// A helper request the capture helper answers (screenshot, stream
    /// settings, recording); anything else is refused.
    pub fn request(&self, command: &Command, timeout: Duration) -> Result<Value> {
        match command {
            Command::Ping | Command::Screenshot | Command::RecordStart { .. } | Command::RecordStop => {
                self.video.request(command, timeout)
            }
            Command::Configure { orientation: None, .. } => self.video.request(command, timeout),
            _ => Err(self.refused(command)),
        }
    }

    /// The accessibility tree: the runner's, while control is on.
    pub fn describe(&self) -> Result<Vec<crate::ax::AxNode>> {
        #[cfg(target_os = "macos")]
        if let Some(control) = self.control() {
            return control.describe().map_err(control_failed);
        }
        Err(self.refused(&Command::AxDescribe))
    }

    pub fn record_start(&self, path: &Path, timeout: Duration) -> Result<()> {
        self.video.record_start(path, timeout)
    }

    pub fn record_stop(&self, timeout: Duration) -> Result<u64> {
        self.video.record_stop(timeout)
    }

    fn refused(&self, command: &Command) -> SimError {
        refusal(command, self.video.udid())
    }
}

/// A runner failure, as this crate's error.
#[cfg(target_os = "macos")]
pub fn control_failed(error: runner_supervisor::ControlError) -> SimError {
    match error {
        runner_supervisor::ControlError::Cancelled => SimError::Cancelled,
        runner_supervisor::ControlError::NotConnected => SimError::DeviceNotFound("the iPhone is not connected over USB".into()),
        other => SimError::CommandFailed { program: "the iPhone's control runner".into(), code: None, stderr: other.to_string() },
    }
}

/// Why `command` is refused on the iPhone `udid` while it is view-only.
fn refusal(command: &Command, udid: &crate::DeviceId) -> SimError {
    let what = match command {
        Command::Touch { .. } | Command::Multitouch { .. } | Command::Scroll { .. } => "Touch",
        Command::Key { .. } => "Typing",
        Command::Button { .. } => "Hardware buttons",
        Command::Configure { .. } => "Rotation",
        Command::AxDescribe | Command::AxFrontmost => "Accessibility",
        _ => "That",
    };
    let why = caps::refuse(what, udid);
    // Rotation never comes with control: the phone turns by itself.
    SimError::Unsupported(if matches!(command, Command::Configure { .. }) { why } else { format!("{why}; {ENABLE_CONTROL_HINT}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TouchPhase;
    use crate::{DeviceId, Orientation};

    #[test]
    fn a_view_only_iphone_refuses_input_and_points_at_control() {
        let udid = DeviceId("iosdev:00008130-0001".into());
        let touch = Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.5, edge: 0 };
        let SimError::Unsupported(why) = refusal(&touch, &udid) else { panic!() };
        assert!(why.starts_with("Touch is not available on a real iPhone") && why.ends_with(ENABLE_CONTROL_HINT), "{why}");
        let SimError::Unsupported(why) = refusal(&Command::AxDescribe, &udid) else { panic!() };
        assert!(why.starts_with("Accessibility"), "{why}");
        let rotate = Command::Configure { scale: None, fps: None, orientation: Some(Orientation::LandscapeLeft), format: None };
        let SimError::Unsupported(why) = refusal(&rotate, &udid) else { panic!() };
        assert!(why.starts_with("Rotation") && !why.contains(ENABLE_CONTROL_HINT), "{why}");
    }

    #[test]
    fn all_input_commands_are_refused_on_view_only_iphone() {
        let udid = DeviceId("iosdev:00008130-0002".into());
        let commands = vec![
            Command::Touch { phase: TouchPhase::Begin, x: 0.5, y: 0.5, edge: 0 },
            Command::Multitouch { phase: TouchPhase::Begin, x1: 0.5, y1: 0.5, x2: 0.6, y2: 0.6 },
            Command::Scroll { x: Some(0.5), y: Some(0.5), dx: 10.0, dy: 10.0 },
            Command::Key { phase: crate::protocol::KeyPhase::Down, usage: 0 },
            Command::Button { name: crate::Button::Home },
        ];
        for cmd in commands {
            let err = refusal(&cmd, &udid);
            assert!(matches!(err, SimError::Unsupported(_)));
        }
    }

    #[test]
    fn frontmost_is_refused_like_ax_describe() {
        let udid = DeviceId("iosdev:00008130-0004".into());
        let SimError::Unsupported(why) = refusal(&Command::AxFrontmost, &udid) else { panic!() };
        assert!(why.starts_with("Accessibility"));
    }
}

