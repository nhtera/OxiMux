//! One live device session, whatever the platform: the iOS helper
//! ([`HelperSession`]) or an Android device ([`AndroidSession`]).
//!
//! The hub, the screen view and the agent verbs hold a [`StreamSession`] and
//! call the same methods on both: input is the helper's portrait-normalized
//! [`Command`] vocabulary, sizes are portrait framebuffer sizes, and the
//! orientation is the device's. Where the platforms truly differ, the method
//! says so: frames are JPEG or GPU pictures from the helper and GPU pictures
//! from Android ([`FrameData`]); the AX tree comes back already parsed
//! ([`describe`]).
//!
//! [`describe`]: StreamSession::describe

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use crate::android::input::AndroidButton;
use crate::android::session::AndroidSession;
use crate::ax::{self, AxNode};
use crate::helper::Hello;
use crate::protocol::{Command, Frame, StreamFormat};
use crate::session::{HelperSession, SessionEvent};
use crate::{Orientation, Platform, Result, SimError};

/// A frame to show.
#[derive(Clone, Debug)]
pub enum FrameData {
    /// From the iOS helper in JPEG format: decoded by the viewer.
    Jpeg(Arc<Frame>),
    /// H.264 from Android or the iOS helper: already decoded, painted as is.
    #[cfg(target_os = "macos")]
    Picture(crate::video::vt_decoder::Picture),
}

impl FrameData {
    /// Width and height in pixels, as painted (rotated for display).
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Jpeg(frame) => (frame.width, frame.height),
            #[cfg(target_os = "macos")]
            Self::Picture(picture) => {
                let (w, h) = picture.size();
                (w as u32, h as u32)
            }
        }
    }
}

#[derive(Clone)]
pub enum StreamSession {
    Ios(HelperSession),
    Android(AndroidSession),
}

impl From<HelperSession> for StreamSession {
    fn from(s: HelperSession) -> Self {
        Self::Ios(s)
    }
}

impl From<AndroidSession> for StreamSession {
    fn from(s: AndroidSession) -> Self {
        Self::Android(s)
    }
}

impl StreamSession {
    pub fn platform(&self) -> Platform {
        match self {
            Self::Ios(_) => Platform::Ios,
            Self::Android(_) => Platform::Android,
        }
    }

    /// The child process streaming it (the helper, or the `adb shell` running
    /// the scrcpy server): a stable key for "is this still the same session".
    pub fn pid(&self) -> u32 {
        match self {
            Self::Ios(s) => s.pid(),
            Self::Android(s) => s.pid(),
        }
    }

    /// The iOS helper's self-description (`None` on Android).
    pub fn hello(&self) -> Option<&Hello> {
        match self {
            Self::Ios(s) => Some(s.hello()),
            Self::Android(_) => None,
        }
    }

    pub fn android(&self) -> Option<&AndroidSession> {
        match self {
            Self::Android(s) => Some(s),
            Self::Ios(_) => None,
        }
    }

    /// Screen pixels per agent coordinate unit, when the platform fixes it:
    /// Android's density (agents work in dp). `None` on iOS, where it is read
    /// from the AX tree (points).
    pub fn point_scale(&self) -> Option<f64> {
        self.android().and_then(AndroidSession::density)
    }

    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        match self {
            Self::Ios(s) => s.set_wake(wake),
            Self::Android(s) => s.set_wake(wake),
        }
    }

    pub fn take_events(&self) -> Option<mpsc::Receiver<SessionEvent>> {
        match self {
            Self::Ios(s) => s.take_events(),
            Self::Android(s) => s.take_events(),
        }
    }

    /// The newest frame, when newer than `seen` (pass 0 at first).
    pub fn latest_frame(&self, seen: u64) -> Option<(u64, FrameData)> {
        match self {
            Self::Ios(s) => s.latest_frame(seen),
            #[cfg(target_os = "macos")]
            Self::Android(s) => s.latest_picture(seen).map(|(seq, p)| (seq, FrameData::Picture(p))),
            #[cfg(not(target_os = "macos"))]
            Self::Android(_) => None,
        }
    }

    pub fn framebuffer_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Ios(s) => s.framebuffer_size(),
            Self::Android(s) => s.framebuffer_size(),
        }
    }

    pub fn orientation(&self) -> Orientation {
        match self {
            Self::Ios(s) => s.orientation(),
            Self::Android(s) => s.orientation(),
        }
    }

    pub fn exited(&self) -> Option<Option<i32>> {
        match self {
            Self::Ios(s) => s.exited(),
            Self::Android(s) => s.exited(),
        }
    }

    /// Input (touch, key, button, scroll).
    pub fn send(&self, command: &Command) -> Result<()> {
        match self {
            Self::Ios(s) => s.send(command),
            Self::Android(s) => s.send(command),
        }
    }

    /// Back / volume: Android only.
    pub fn press_android(&self, button: AndroidButton) -> Result<()> {
        match self {
            Self::Android(s) => s.press(button),
            Self::Ios(_) => Err(SimError::Unsupported("that button exists only on Android".into())),
        }
    }

    /// A raw helper request (iOS only; Android has no helper).
    pub fn request(&self, command: &Command, timeout: Duration) -> Result<Value> {
        match self {
            Self::Ios(s) => s.request(command, timeout),
            Self::Android(_) => Err(SimError::Unsupported("not an iOS helper session".into())),
        }
    }

    pub fn pause(&self) -> Result<()> {
        match self {
            Self::Ios(s) => s.pause(),
            Self::Android(s) => s.pause(),
        }
    }

    pub fn resume(&self) -> Result<()> {
        match self {
            Self::Ios(s) => s.resume(),
            Self::Android(s) => s.resume(),
        }
    }

    /// Whether the stream's encoding can be switched ([`Self::set_format`]):
    /// an iOS helper with H.264. Android is always H.264.
    pub fn supports_format_switch(&self) -> bool {
        matches!(self, Self::Ios(s) if s.supports_avcc())
    }

    /// Switch an iOS stream between JPEG and H.264; a no-op elsewhere.
    pub fn set_format(&self, format: StreamFormat, timeout: Duration) -> Result<()> {
        match self {
            Self::Ios(s) => s.set_format(format, timeout),
            Self::Android(_) => Ok(()),
        }
    }

    /// Scale / fps / orientation. On Android only the orientation applies
    /// (the stream's size and rate are fixed when it starts).
    pub fn configure(&self, scale: Option<f64>, fps: Option<f64>, orientation: Option<Orientation>, timeout: Duration) -> Result<()> {
        match self {
            Self::Ios(s) => s.configure(scale, fps, orientation, timeout),
            Self::Android(s) => orientation.map_or(Ok(()), |o| s.rotate_to(o, timeout)),
        }
    }

    pub fn screenshot_png(&self, timeout: Duration) -> Result<Vec<u8>> {
        match self {
            Self::Ios(s) => s.screenshot_png(timeout),
            Self::Android(s) => s.screenshot_png(timeout),
        }
    }

    /// The accessibility tree: the helper's (points) or uiautomator's
    /// (display pixels). Blocking.
    pub fn describe(&self, timeout: Duration) -> Result<Vec<AxNode>> {
        match self {
            Self::Ios(s) => {
                let reply = s.request(&Command::AxDescribe, timeout)?;
                ax::parse_describe(&serde_json::to_vec(&reply).map_err(|e| SimError::Protocol(e.to_string()))?)
            }
            Self::Android(s) => s.describe(timeout),
        }
    }

    pub fn shutdown(&self) {
        match self {
            Self::Ios(s) => s.shutdown(),
            Self::Android(s) => s.shutdown(),
        }
    }
}
