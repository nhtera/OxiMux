//! The session's decoder thread: the helper's H.264 (`video` messages) into
//! GPU pictures, on macOS.
//!
//! One VideoToolbox session per avcC record (the helper sends one before
//! every key frame); pictures only from a key frame on; a key frame asked for
//! whenever the stream is broken (a failed or dropped picture, no decoder
//! yet); and, when decoding keeps failing, a fallback to JPEG that lasts
//! until the user picks H.264 again. The dispatcher feeds this thread through
//! a bounded queue and never waits on it: a stalled decode drops pictures,
//! it never holds up replies or grows memory.

use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};

use super::{Inner, SessionEvent, notify};
use crate::protocol::{self, Command, StreamFormat, Video};

/// Pictures the dispatcher may queue ahead of the decoder.
pub(super) const QUEUE: usize = 8;

/// Consecutive decode failures after which the session asks for JPEG: a
/// stream that keeps failing must not freeze the screen.
#[cfg(target_os = "macos")]
const MAX_DECODE_FAILURES: u32 = 3;

pub(super) fn decode_loop(inner: &Arc<Inner>, rx: &mpsc::Receiver<Video>, events: &mpsc::Sender<SessionEvent>) {
    let mut state = VideoState::default();
    for message in rx {
        if let Some(why) = state.handle(inner, message) {
            let _ = events.send(SessionEvent::EncodingFallback(why));
            notify(inner);
        }
    }
}

#[derive(Default)]
struct VideoState {
    #[cfg(target_os = "macos")]
    decoder: Option<crate::video::vt_decoder::Decoder>,
    /// A key frame was asked for and has not arrived: deltas are useless.
    need_key: bool,
    #[cfg(target_os = "macos")]
    failures: u32,
    /// JPEG was asked for (at this many format requests): ignore H.264 until
    /// the user asks for it again.
    gave_up: Option<u64>,
}

impl VideoState {
    /// Handle one message; `Some(why)` when the stream just fell back to JPEG.
    #[cfg(target_os = "macos")]
    fn handle(&mut self, inner: &Arc<Inner>, message: Video) -> Option<String> {
        use crate::protocol::VideoTag;
        use crate::video::{annexb, vt_decoder::Decoder};
        use std::sync::PoisonError;
        if !self.accepting(inner) {
            return None;
        }
        if inner.video_dropped.swap(false, Ordering::AcqRel) {
            self.want_key(inner);
        }
        match message.tag {
            VideoTag::Description => {
                let Some(params) = annexb::from_avcc_record(&message.data) else {
                    return self.fail(inner, "an unreadable H.264 description");
                };
                if self.decoder.as_ref().is_some_and(|d| d.params() == &params) {
                    return None;
                }
                let sink = Arc::downgrade(inner);
                let made = Decoder::new(&params, move |picture| {
                    if let Some(inner) = sink.upgrade() {
                        // Runs inside VideoToolbox's callback: never panic.
                        inner.view.lock().unwrap_or_else(PoisonError::into_inner).shown =
                            Some(super::FrameData::Picture(picture));
                        inner.frame_seq.fetch_add(1, Ordering::AcqRel);
                        notify(&inner);
                    }
                });
                match made {
                    Ok(decoder) => {
                        self.decoder = Some(decoder);
                        // A description always precedes a key frame.
                        self.need_key = true;
                        None
                    }
                    Err(e) => self.give_up(inner, format!("the H.264 decoder could not start: {e}")),
                }
            }
            VideoTag::Keyframe | VideoTag::Delta => {
                if self.need_key && message.tag == VideoTag::Delta {
                    return None;
                }
                let Some(decoder) = &self.decoder else {
                    // Joined mid-stream, or the decoder was dropped: the next
                    // key frame brings a description, which rebuilds it.
                    self.want_key(inner);
                    return None;
                };
                match decoder.decode_avcc(message.data) {
                    Ok(()) => {
                        self.need_key = false;
                        self.failures = 0;
                        None
                    }
                    Err(e) => self.fail(inner, &e.to_string()),
                }
            }
        }
    }

    /// Off macOS there is no decoder: ask for JPEG once.
    #[cfg(not(target_os = "macos"))]
    fn handle(&mut self, inner: &Arc<Inner>, _message: Video) -> Option<String> {
        if self.accepting(inner) { self.give_up(inner, "no H.264 decoder on this platform".into()) } else { None }
    }

    /// False while a fallback to JPEG stands; a new format request ends it.
    fn accepting(&mut self, inner: &Inner) -> bool {
        match self.gave_up {
            Some(at) if at == inner.format_requests.load(Ordering::Acquire) => false,
            Some(_) => {
                *self = Self::default();
                true
            }
            None => true,
        }
    }

    /// Ask the helper for a key frame, once per break.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn want_key(&mut self, inner: &Inner) {
        if !self.need_key {
            self.need_key = true;
            // Any `configure` makes the helper's next picture a key frame.
            send_now(inner, &Command::Configure { scale: None, fps: None, orientation: None, format: None });
        }
    }

    /// One bad picture: drop the decoder (a dead VideoToolbox session, e.g.
    /// after sleep, fails every picture) and wait for the next key frame and
    /// its description; too many in a row, and fall back to JPEG.
    #[cfg(target_os = "macos")]
    fn fail(&mut self, inner: &Inner, why: &str) -> Option<String> {
        self.failures += 1;
        tracing::debug!(udid = %inner.udid, "H.264 picture: {why}");
        if self.failures >= MAX_DECODE_FAILURES {
            return self.give_up(inner, why.to_owned());
        }
        self.decoder = None;
        self.need_key = false;
        self.want_key(inner);
        None
    }

    fn give_up(&mut self, inner: &Inner, why: String) -> Option<String> {
        tracing::warn!(udid = %inner.udid, "falling back to JPEG: {why}");
        #[cfg(target_os = "macos")]
        {
            self.decoder = None;
        }
        self.gave_up = Some(inner.format_requests.load(Ordering::Acquire));
        let jpeg = Command::Configure { scale: None, fps: None, orientation: None, format: Some(StreamFormat::Jpeg) };
        send_now(inner, &jpeg);
        Some(format!("H.264 stopped working ({why}); streaming JPEG"))
    }
}

/// Queue `command` (no reply wanted) from a session thread.
fn send_now(inner: &Inner, command: &Command) {
    if let Some(writer) = inner.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
        let _ = writer.send((protocol::encode_command(command, None), false));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Mutex, RwLock};

    use serde_json::{Value, json};

    use super::*;
    use crate::DeviceId;
    use crate::helper::Hello;
    use crate::protocol::VideoTag;
    use crate::video::annexb;

    /// A session core with a stand-in child and a captured stdin.
    fn inner() -> (Arc<Inner>, mpsc::Receiver<super::super::WriteReq>) {
        let (tx, rx) = mpsc::channel();
        let child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let inner = Inner {
            udid: DeviceId("TEST".into()),
            pid: child.id(),
            hello: Hello { proto: 2, version: "test".into(), xcode: None },
            view: Mutex::default(),
            frame_seq: AtomicU64::new(0),
            format_requests: AtomicU64::new(0),
            video_dropped: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(Some(HashMap::new())),
            wake: RwLock::default(),
            events: Mutex::new(None),
            writer: Mutex::new(Some(tx)),
            child: Mutex::new(child),
            ledger: None,
        };
        (Arc::new(inner), rx)
    }

    /// Every command the state machine sent to the helper, as JSON.
    fn sent(rx: &mpsc::Receiver<super::super::WriteReq>) -> Vec<Value> {
        rx.try_iter().map(|(bytes, _)| serde_json::from_slice(&bytes[4..]).unwrap()).collect()
    }

    fn message(tag: VideoTag, data: Vec<u8>) -> Video {
        Video { width: 160, height: 160, tag, data }
    }

    /// The avcC record for the Android fixture's parameter sets.
    fn fixture() -> (Vec<u8>, Vec<u8>) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/android-160");
        let params = annexb::parameter_sets(&std::fs::read(dir.join("config.h264")).unwrap()).unwrap();
        let (sps, pps) = (&params.sps, &params.pps);
        let mut record = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        record.extend_from_slice(sps);
        record.push(1);
        record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        record.extend_from_slice(pps);
        (record, annexb::to_avcc(&std::fs::read(dir.join("key.h264")).unwrap()))
    }

    fn stop(inner: &Inner) {
        let _ = inner.child.lock().unwrap().kill();
    }

    #[test]
    fn decodes_from_the_key_frame_on() {
        let (inner, rx) = inner();
        let (record, key) = fixture();
        let mut state = VideoState::default();
        assert_eq!(state.handle(&inner, message(VideoTag::Description, record)), None);
        // A delta before the key frame is skipped, not fed to the decoder.
        assert_eq!(state.handle(&inner, message(VideoTag::Delta, key.clone())), None);
        assert_eq!(inner.frame_seq.load(Ordering::Acquire), 0);
        assert_eq!(state.handle(&inner, message(VideoTag::Keyframe, key)), None);
        assert_eq!(inner.frame_seq.load(Ordering::Acquire), 1);
        assert!(matches!(inner.view.lock().unwrap().shown, Some(super::super::FrameData::Picture(_))));
        assert!(sent(&rx).is_empty(), "a healthy stream asks for nothing");
        stop(&inner);
    }

    #[test]
    fn a_delta_without_a_decoder_asks_for_one_key_frame() {
        let (inner, rx) = inner();
        let mut state = VideoState::default();
        for _ in 0..3 {
            assert_eq!(state.handle(&inner, message(VideoTag::Delta, vec![0, 0, 0, 1, 0x41])), None);
        }
        assert_eq!(sent(&rx), [json!({"cmd": "configure"})]);
        // A picture dropped at the full queue asks again, once.
        state.need_key = false;
        inner.video_dropped.store(true, Ordering::Release);
        let (record, _) = fixture();
        state.handle(&inner, message(VideoTag::Description, record));
        assert_eq!(sent(&rx), [json!({"cmd": "configure"})]);
        stop(&inner);
    }

    #[test]
    fn repeated_failures_fall_back_to_jpeg_until_h264_is_asked_for_again() {
        let (inner, rx) = inner();
        let (record, key) = fixture();
        let mut state = VideoState::default();
        let bad = || message(VideoTag::Description, vec![1, 2, 3]);
        assert_eq!(state.handle(&inner, bad()), None);
        assert_eq!(state.handle(&inner, bad()), None);
        assert_eq!(sent(&rx), [json!({"cmd": "configure"}), json!({"cmd": "configure"})]);
        let why = state.handle(&inner, bad()).expect("the third failure falls back");
        assert!(why.contains("streaming JPEG"), "{why}");
        assert_eq!(sent(&rx), [json!({"cmd": "configure", "format": "jpeg"})]);
        // H.264 still in flight is ignored while the fallback stands...
        assert_eq!(state.handle(&inner, message(VideoTag::Description, record.clone())), None);
        assert!(state.decoder.is_none());
        // ...and decoded again once the user picks H.264.
        inner.format_requests.fetch_add(1, Ordering::AcqRel);
        assert_eq!(state.handle(&inner, message(VideoTag::Description, record)), None);
        assert_eq!(state.handle(&inner, message(VideoTag::Keyframe, key)), None);
        assert_eq!(inner.frame_seq.load(Ordering::Acquire), 1);
        stop(&inner);
    }

    #[test]
    fn a_failed_picture_drops_the_decoder() {
        let (inner, rx) = inner();
        let (record, _) = fixture();
        let mut state = VideoState::default();
        state.handle(&inner, message(VideoTag::Description, record));
        assert!(state.decoder.is_some());
        // Garbage that VideoToolbox cannot turn into a picture.
        assert_eq!(state.handle(&inner, message(VideoTag::Keyframe, vec![0, 0, 0, 2, 0x65, 0xff])), None);
        assert!(state.decoder.is_none(), "a failing decoder is rebuilt from the next description");
        assert_eq!(sent(&rx), [json!({"cmd": "configure"})]);
        stop(&inner);
    }
}
