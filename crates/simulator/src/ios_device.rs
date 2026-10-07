//! [`DeviceSession`]: a real iPhone, as a [`StreamSession`] holds it.
//!
//! Its video is the capture helper ([`HelperKind::DeviceCapture`]): a
//! [`HelperSession`] like a simulator's, minus everything a screen-capture
//! device cannot do. Without its control runner (Phase 8) it is view-only:
//! input and the accessibility tree are refused here, with the one wording
//! every path uses ([`crate::caps::refuse`]), rather than sent to a helper
//! that would only answer "unsupported".
//!
//! [`StreamSession`]: crate::stream::StreamSession
//! [`HelperKind::DeviceCapture`]: crate::helper::HelperKind::DeviceCapture

#[cfg(target_os = "macos")]
pub mod runner_build;
#[cfg(target_os = "macos")]
pub mod runner_client;
#[cfg(target_os = "macos")]
pub mod runner_supervisor;
#[cfg(target_os = "macos")]
pub mod team;
#[cfg(target_os = "macos")]
pub mod usbmux;

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::caps;
use crate::protocol::Command;
use crate::session::HelperSession;
use crate::{Result, SimError};

/// What to do instead, while OxiMux only shows an iPhone.
pub const ENABLE_CONTROL_HINT: &str = "use the phone itself";

#[derive(Clone)]
pub struct DeviceSession {
    video: HelperSession,
}

impl DeviceSession {
    pub fn new(video: HelperSession) -> Self {
        Self { video }
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

    /// Input: refused until the control runner exists.
    pub fn send(&self, command: &Command) -> Result<()> {
        match command {
            Command::Pause | Command::Resume => self.video.send(command),
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

    /// The accessibility tree: refused until the control runner exists.
    pub fn describe(&self) -> Result<Vec<crate::ax::AxNode>> {
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

