//! A live Android device: the scrcpy server running on it, its video decoded
//! on the GPU as it arrives, and its control socket taking the panel's input.
//!
//! It behaves like the iOS helper's session towards everything above it — a
//! latest-frame-wins picture with a sequence number and a wake callback, the
//! same [`SessionEvent`]s, portrait-normalized input, a portrait framebuffer
//! size and the device orientation — so the hub, the screen view and the
//! agent verbs need no Android branch for streaming or input.

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, mpsc};
use std::time::Duration;

use super::adb::Adb;
use super::input::{self, AndroidButton, Modifiers, Screen};
use super::scrcpy_control::ControlMsg;
use super::scrcpy_server::{self, StreamOptions};
use super::scrcpy_video::{self, VideoEvent};
use crate::protocol::Command;
use crate::runner::SystemRunner;
use crate::session::SessionEvent;
#[cfg(target_os = "macos")]
use crate::video::annexb;
use crate::{DeviceId, Orientation, Result, SimError};

#[cfg(target_os = "macos")]
use crate::video::vt_decoder::{Decoder, Picture};

const ADB_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the server may take to name its codec once connected.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Control messages waiting for the socket. Input never waits on the device:
/// past this (a stalled transport), messages are dropped.
const CONTROL_QUEUE: usize = 256;
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

type Wake = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct View {
    #[cfg(target_os = "macos")]
    picture: Option<Picture>,
    /// The video size as streamed (display orientation).
    size: Option<(u32, u32)>,
    /// The rotation last asked for; what the stream's shape says wins.
    requested: Option<Orientation>,
    exited: Option<Option<i32>>,
}

struct Inner {
    id: DeviceId,
    serial: String,
    adb: PathBuf,
    view: Mutex<View>,
    /// Decoded pictures so far (macOS decodes; elsewhere nothing is shown).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    frame_seq: AtomicU64,
    wake: RwLock<Option<Wake>>,
    events_tx: Mutex<Option<mpsc::Sender<SessionEvent>>>,
    events: Mutex<Option<mpsc::Receiver<SessionEvent>>>,
    /// The control socket's writer thread (input is queued, never written on
    /// the caller's thread — which is often the UI's).
    control: Mutex<Option<mpsc::SyncSender<Vec<u8>>>>,
    mods: Mutex<Modifiers>,
    paused: AtomicBool,
    /// Drop media until a key frame: after a resume, the frames in flight
    /// reference pictures that were never decoded.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    need_key: AtomicBool,
    closing: AtomicBool,
    server: Mutex<Option<Child>>,
    video: Mutex<Option<TcpStream>>,
    port: u16,
    server_pid: u32,
    /// The display's own size in pixels, portrait (`wm size`); the video may
    /// be smaller (`max_size`).
    native: Option<(u32, u32)>,
    /// Display pixels per dp (`wm density` / 160): an agent's "points".
    density: Option<f64>,
}

/// A streaming Android device. Cheap to clone; shut down when the last clone
/// is dropped (or by [`AndroidSession::shutdown`]).
#[derive(Clone)]
pub struct AndroidSession {
    inner: Arc<Inner>,
    _guard: Arc<Guard>,
}

struct Guard(Arc<Inner>);

impl Drop for Guard {
    fn drop(&mut self) {
        shutdown(&self.0);
    }
}

impl AndroidSession {
    /// Push the server, forward its socket, start it and begin streaming the
    /// device `serial` (named `id` towards the app). Blocking: a background
    /// executor's work (a second or so).
    pub fn start(adb: &Path, jar: &Path, id: DeviceId, serial: &str, opts: StreamOptions) -> Result<Self> {
        let runner = SystemRunner;
        let client = Adb::new(&runner, adb);
        client.push(serial, jar, scrcpy_server::DEVICE_JAR, ADB_TIMEOUT)?;
        let scid = scrcpy_server::new_scid();
        let port = client.forward(serial, &format!("localabstract:{}", scrcpy_server::socket_name(scid)), ADB_TIMEOUT)?;
        let conn = match scrcpy_server::start(adb, serial, scid, opts, port) {
            Ok(conn) => conn,
            Err(e) => {
                let _ = client.forward_remove(serial, port, ADB_TIMEOUT);
                return Err(e);
            }
        };
        let scrcpy_server::Connection { mut video, control, server, .. } = conn;
        // Until the session owns them, a failure stops the server and drops
        // the forward.
        let mut pending = Pending { server: Some(server), adb: adb.to_path_buf(), serial: serial.to_owned(), port };
        video.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        scrcpy_video::read_codec(&mut video)?;
        video.set_read_timeout(None)?;
        let writer = spawn_writer(control, serial)?;
        let native = client.shell(serial, &["wm", "size"], ADB_TIMEOUT).ok().and_then(|o| super::adb::parse_wm_size(&o)).map(|(w, h)| (w.min(h), w.max(h)));
        let density = client.shell(serial, &["wm", "density"], ADB_TIMEOUT).ok().and_then(|o| super::adb::parse_wm_density(&o)).map(|dpi| f64::from(dpi) / 160.0);
        let (events_tx, events) = mpsc::channel();
        let inner = Arc::new(Inner {
            id,
            serial: serial.to_owned(),
            adb: adb.to_path_buf(),
            view: Mutex::new(View::default()),
            frame_seq: AtomicU64::new(0),
            wake: RwLock::new(None),
            events_tx: Mutex::new(Some(events_tx)),
            events: Mutex::new(Some(events)),
            control: Mutex::new(Some(writer)),
            mods: Mutex::new(Modifiers::default()),
            paused: AtomicBool::new(false),
            need_key: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            server_pid: pending.server.as_ref().map_or(0, Child::id),
            server: Mutex::new(pending.server.take()),
            video: Mutex::new(video.try_clone().ok()),
            port,
            native,
            density,
        });
        // The guard first: if the reader cannot start, dropping it stops all.
        let guard = Arc::new(Guard(Arc::clone(&inner)));
        let reader = Arc::clone(&inner);
        std::thread::Builder::new().name(format!("oximux-android-video-{}", inner.serial)).spawn(move || read_loop(reader, video))?;
        Ok(Self { inner, _guard: guard })
    }

    pub fn id(&self) -> &DeviceId {
        &self.inner.id
    }

    /// The adb serial it is streaming now.
    pub fn serial(&self) -> &str {
        &self.inner.serial
    }

    /// The `adb shell` running the server (for the child ledger).
    pub fn pid(&self) -> u32 {
        self.inner.server_pid
    }

    /// See the iOS session's `set_wake`: the closure must not own a clone.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.inner.wake.write().unwrap() = Some(Arc::new(wake));
    }

    pub fn take_events(&self) -> Option<mpsc::Receiver<SessionEvent>> {
        self.inner.events.lock().unwrap().take()
    }

    /// The newest decoded picture, when newer than `seen`.
    #[cfg(target_os = "macos")]
    pub fn latest_picture(&self, seen: u64) -> Option<(u64, Picture)> {
        let seq = self.inner.frame_seq.load(Ordering::Acquire);
        if seq <= seen {
            return None;
        }
        let picture = self.inner.view.lock().unwrap().picture.clone()?;
        Some((seq, picture))
    }

    /// The screen in portrait, in the display's own pixels (what the iOS
    /// helper reports, whatever the rotation) — known once the stream is.
    pub fn framebuffer_size(&self) -> Option<(u32, u32)> {
        let streamed = self.inner.view.lock().unwrap().size?;
        Some(self.inner.native.unwrap_or((streamed.0.min(streamed.1), streamed.0.max(streamed.1))))
    }

    /// Display pixels per dp: the scale between screenshots and the
    /// coordinates agents use (dp, Android's points).
    pub fn density(&self) -> Option<f64> {
        self.inner.density
    }

    /// The device orientation, read from the stream's shape: landscape video
    /// is a turned device (which way: the rotation last asked for, else
    /// counter-clockwise, Android's usual landscape).
    pub fn orientation(&self) -> Orientation {
        let view = self.inner.view.lock().unwrap();
        orientation_of(view.size, view.requested)
    }

    pub fn exited(&self) -> Option<Option<i32>> {
        self.inner.view.lock().unwrap().exited
    }

    /// Input, as the iOS helper takes it. Non-input commands do nothing.
    pub fn send(&self, command: &Command) -> Result<()> {
        let screen = {
            let view = self.inner.view.lock().unwrap();
            view.size.map(|size| Screen { size, orientation: orientation_of(view.size, view.requested) })
        };
        let msgs = input::translate(command, screen, &mut self.inner.mods.lock().unwrap_or_else(PoisonError::into_inner));
        self.queue(&msgs)
    }

    /// Back, volume up, volume down.
    pub fn press(&self, button: AndroidButton) -> Result<()> {
        self.queue(&input::android_button(button))
    }

    /// Type `text` as text, not key by key. ASCII goes through scrcpy's text
    /// injection (in pieces of its limit); anything else is pasted through the
    /// device clipboard — injection only types what the device keymap can,
    /// and silently skips the rest (emoji, CJK, most accented letters).
    pub fn type_text(&self, text: &str) -> Result<()> {
        if !text.is_ascii() {
            return self.queue(&[ControlMsg::SetClipboard { sequence: 0, paste: true, text: text.to_owned() }]);
        }
        let mut rest = text;
        let mut msgs = Vec::new();
        while !rest.is_empty() {
            let piece = super::scrcpy_control::truncate_utf8(rest, super::scrcpy_control::MAX_TEXT);
            msgs.push(ControlMsg::Text(piece.to_owned()));
            rest = &rest[piece.len()..];
        }
        self.queue(&msgs)
    }

    /// Hand messages to the writer thread. A full queue (a stalled device)
    /// drops them rather than make the caller wait.
    fn queue(&self, msgs: &[ControlMsg]) -> Result<()> {
        let control = self.inner.control.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(tx) = control.as_ref() else { return Err(SimError::HelperExited { code: None }) };
        for msg in msgs {
            match tx.try_send(msg.serialize()) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => tracing::debug!("android control queue full; input dropped"),
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(SimError::HelperExited { code: None }),
            }
        }
        Ok(())
    }

    /// Stop decoding (a hidden panel). Packets keep arriving and are dropped.
    pub fn pause(&self) -> Result<()> {
        self.inner.paused.store(true, Ordering::Release);
        Ok(())
    }

    /// Decode again, starting from a key frame the encoder is asked for now.
    pub fn resume(&self) -> Result<()> {
        self.inner.need_key.store(true, Ordering::Release);
        self.inner.paused.store(false, Ordering::Release);
        self.queue(&[ControlMsg::ResetVideo])
    }

    /// Rotate an emulator (the stream's next session packet brings the new
    /// shape). A phone is refused: this turns its auto-rotate off, a setting
    /// that is the person's own. Scale and frame rate are fixed at start.
    pub fn rotate_to(&self, orientation: Orientation, timeout: Duration) -> Result<()> {
        if matches!(super::Target::from_id(&self.inner.id), Some(super::Target::Serial(_))) {
            return Err(SimError::Unsupported("rotate the phone itself; OxiMux does not change a phone's rotation settings".into()));
        }
        let rotation = match orientation {
            Orientation::Portrait => "0",
            Orientation::LandscapeLeft => "1",
            Orientation::PortraitUpsideDown => "2",
            Orientation::LandscapeRight => "3",
        };
        let runner = SystemRunner;
        let adb = Adb::new(&runner, &self.inner.adb);
        adb.shell(&self.inner.serial, &["settings", "put", "system", "accelerometer_rotation", "0"], timeout)?;
        adb.shell(&self.inner.serial, &["settings", "put", "system", "user_rotation", rotation], timeout)?;
        self.inner.view.lock().unwrap().requested = Some(orientation);
        Ok(())
    }

    /// A PNG of the screen at full resolution (`screencap`).
    pub fn screenshot_png(&self, timeout: Duration) -> Result<Vec<u8>> {
        let runner = SystemRunner;
        Adb::new(&runner, &self.inner.adb).screencap_png(&self.inner.serial, timeout)
    }

    /// The accessibility tree (`uiautomator dump`), in display pixels.
    pub fn describe(&self, timeout: Duration) -> Result<Vec<crate::ax::AxNode>> {
        super::uiautomator::describe(&SystemRunner, &self.inner.adb, &self.inner.serial, timeout)
    }

    /// Stop the server and close the sockets. Returns at once; idempotent.
    pub fn shutdown(&self) {
        shutdown(&self.inner);
    }
}

fn orientation_of(size: Option<(u32, u32)>, requested: Option<Orientation>) -> Orientation {
    let landscape = size.is_some_and(|(w, h)| w > h);
    match requested {
        Some(o) if o.is_landscape() == landscape => o,
        _ if landscape => Orientation::LandscapeLeft,
        _ => Orientation::Portrait,
    }
}

fn read_loop(inner: Arc<Inner>, mut video: TcpStream) {
    #[cfg(target_os = "macos")]
    let mut decoder: Option<Decoder> = None;
    let emit = |event: SessionEvent| {
        if let Some(tx) = inner.events_tx.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
        wake(&inner);
    };
    let failure = loop {
        let event = match scrcpy_video::read_event(&mut video) {
            Ok(event) => event,
            Err(e) => break e.to_string(),
        };
        match event {
            VideoEvent::Session { width, height, .. } => {
                inner.view.lock().unwrap().size = Some((width, height));
                let (w, h) = (width.min(height), width.max(height));
                emit(SessionEvent::Size { width: w, height: h });
                let orientation = orientation_of(Some((width, height)), inner.view.lock().unwrap().requested);
                emit(SessionEvent::Orientation(orientation));
            }
            #[cfg(target_os = "macos")]
            VideoEvent::Media(packet) if packet.config => {
                let Some(params) = annexb::parameter_sets(&packet.data) else { continue };
                if decoder.as_ref().is_some_and(|d| d.params() == &params) {
                    continue;
                }
                let sink = Arc::downgrade(&inner);
                let made = Decoder::new(&params, move |picture| {
                    if let Some(inner) = sink.upgrade() {
                        // Runs inside VideoToolbox's callback: never panic.
                        inner.view.lock().unwrap_or_else(PoisonError::into_inner).picture = Some(picture);
                        inner.frame_seq.fetch_add(1, Ordering::AcqRel);
                        wake(&inner);
                    }
                });
                match made {
                    Ok(made) => {
                        decoder = Some(made);
                        inner.need_key.store(true, Ordering::Release);
                    }
                    // A session that can never show a picture is a failure,
                    // not a blank screen reported as live.
                    Err(e) => break format!("the video decoder could not start: {e}"),
                }
            }
            #[cfg(target_os = "macos")]
            VideoEvent::Media(packet) => {
                if inner.paused.load(Ordering::Acquire) {
                    continue;
                }
                if inner.need_key.load(Ordering::Acquire) {
                    if !packet.key_frame {
                        continue;
                    }
                    inner.need_key.store(false, Ordering::Release);
                }
                if let Some(decoder) = &decoder
                    && let Err(e) = decoder.decode(&packet.data)
                {
                    tracing::debug!("android video frame: {e}");
                }
            }
            #[cfg(not(target_os = "macos"))]
            VideoEvent::Media(_) => {}
        }
    };
    let closing = inner.closing.load(Ordering::Acquire);
    inner.view.lock().unwrap().exited = Some(None);
    *inner.control.lock().unwrap() = None;
    let fatal = (!closing).then(|| format!("the Android stream ended: {failure}"));
    emit(SessionEvent::Exited { code: None, fatal });
    *inner.events_tx.lock().unwrap() = None;
}

/// The control socket's writer: queued messages, written with a timeout, so
/// a stalled device costs dropped input — never a frozen app.
fn spawn_writer(mut socket: TcpStream, serial: &str) -> Result<mpsc::SyncSender<Vec<u8>>> {
    socket.set_write_timeout(Some(CONTROL_WRITE_TIMEOUT))?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(CONTROL_QUEUE);
    std::thread::Builder::new().name(format!("oximux-android-control-{serial}")).spawn(move || {
        for bytes in rx {
            if let Err(e) = socket.write_all(&bytes) {
                tracing::debug!("android control socket: {e}");
                break;
            }
        }
        let _ = socket.shutdown(std::net::Shutdown::Both);
    })?;
    Ok(tx)
}

/// The server and forward of a session still starting: stopped if the start
/// fails before the session owns them.
struct Pending {
    server: Option<Child>,
    adb: PathBuf,
    serial: String,
    port: u16,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(mut server) = self.server.take() {
            let _ = server.kill();
            let _ = server.wait();
            let _ = Adb::new(&SystemRunner, &self.adb).forward_remove(&self.serial, self.port, ADB_TIMEOUT);
        }
    }
}

fn wake(inner: &Inner) {
    let wake = inner.wake.read().unwrap().clone();
    if let Some(wake) = wake {
        wake();
    }
}

fn shutdown(inner: &Arc<Inner>) {
    if inner.closing.swap(true, Ordering::AcqRel) {
        return;
    }
    *inner.control.lock().unwrap() = None;
    if let Some(video) = inner.video.lock().unwrap().take() {
        let _ = video.shutdown(std::net::Shutdown::Both);
    }
    let server = inner.server.lock().unwrap().take();
    let (adb, serial, port) = (inner.adb.clone(), inner.serial.clone(), inner.port);
    // Off the caller's thread: removing the forward talks to adb.
    let _ = std::thread::Builder::new().name("oximux-android-stop".into()).spawn(move || {
        if let Some(mut server) = server {
            let _ = server.kill();
            let _ = server.wait();
        }
        let runner = SystemRunner;
        let _ = Adb::new(&runner, &adb).forward_remove(&serial, port, ADB_TIMEOUT);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orientation_follows_the_stream_shape() {
        assert_eq!(orientation_of(None, None), Orientation::Portrait);
        assert_eq!(orientation_of(Some((1080, 2400)), None), Orientation::Portrait);
        assert_eq!(orientation_of(Some((2400, 1080)), None), Orientation::LandscapeLeft);
        assert_eq!(orientation_of(Some((2400, 1080)), Some(Orientation::LandscapeRight)), Orientation::LandscapeRight);
        // Asked to turn, not turned yet: the stream's shape wins.
        assert_eq!(orientation_of(Some((1080, 2400)), Some(Orientation::LandscapeRight)), Orientation::Portrait);
        assert_eq!(orientation_of(Some((1080, 2400)), Some(Orientation::PortraitUpsideDown)), Orientation::PortraitUpsideDown);
    }
}
