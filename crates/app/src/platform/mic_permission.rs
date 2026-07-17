//! macOS microphone (TCC) permission for voice dictation.
//!
//! Wraps `AVCaptureDevice.authorizationStatus(for: .audio)` and the async
//! `requestAccess(for:completionHandler:)`. The request's completion block runs
//! on an arbitrary dispatch queue, so callers hand it a `Send` closure and hop
//! back to the GPUI main thread themselves (never touch UI from the block).
//!
//! Without a granted permission, CoreAudio capture "succeeds" but delivers
//! zeroed samples — so the composer checks this before recording and shows an
//! actionable toast on denial rather than a silent empty transcript.
//!
//! Note: an unbundled `cargo run` attributes the TCC prompt to the terminal;
//! the bundled `.app` (with `NSMicrophoneUsageDescription` in Info.plist)
//! attributes it correctly.

/// Current microphone authorization state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    Granted,
    Denied,
    Restricted,
    Undetermined,
}

impl MicPermission {
    /// Whether capture is allowed to start right now.
    pub fn is_granted(self) -> bool {
        matches!(self, MicPermission::Granted)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::MicPermission;
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeAudio};

    /// The `.audio` media type constant, or `None` if the framework didn't
    /// vend it (should never happen on a real device).
    fn audio_media_type() -> Option<&'static objc2_av_foundation::AVMediaType> {
        // Safety: reading an AVFoundation string constant. `Option` guards a
        // null.
        unsafe { AVMediaTypeAudio }
    }

    pub fn status() -> MicPermission {
        let Some(mt) = audio_media_type() else {
            return MicPermission::Undetermined;
        };
        // Safety: valid media-type constant; the call has no side effects.
        let st = unsafe { AVCaptureDevice::authorizationStatusForMediaType(mt) };
        // AVAuthorizationStatus: NotDetermined=0, Restricted=1, Denied=2,
        // Authorized=3.
        match st.0 {
            3 => MicPermission::Granted,
            2 => MicPermission::Denied,
            1 => MicPermission::Restricted,
            _ => MicPermission::Undetermined,
        }
    }

    pub fn request<F: FnOnce(bool) + Send + 'static>(cb: F) {
        let Some(mt) = audio_media_type() else {
            cb(false);
            return;
        };
        // The completion block type is `Fn(Bool)`, but we only fire the callback
        // once — a Mutex<Option<F>> gives FnOnce semantics inside an Fn block.
        let cell = std::sync::Mutex::new(Some(cb));
        let block = RcBlock::new(move |granted: Bool| {
            if let Some(cb) = cell.lock().unwrap().take() {
                cb(granted.as_bool());
            }
        });
        // Safety: valid media type + a live block; AVFoundation retains the
        // block until it fires.
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(mt, &block);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::MicPermission;

    pub fn status() -> MicPermission {
        // Non-macOS builds don't gate on TCC; treat as granted so the flow works.
        MicPermission::Granted
    }

    pub fn request<F: FnOnce(bool) + Send + 'static>(cb: F) {
        cb(true);
    }
}

/// Current microphone authorization state (never prompts).
pub fn status() -> MicPermission {
    imp::status()
}

/// Prompt for microphone access if undetermined. `cb(granted)` runs on an
/// arbitrary thread — the caller must hop back to the UI thread.
pub fn request<F: FnOnce(bool) + Send + 'static>(cb: F) {
    imp::request(cb)
}
