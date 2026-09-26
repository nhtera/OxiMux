//! [`HelperSession`]: one running helper, as the UI and the agent verbs see it.
//!
//! Four threads per session, none of them ever blocking the caller:
//! - **reader** — parses the helper's stdout ([`helper::pump`]);
//! - **dispatcher** — keeps only the newest frame (latest-frame-wins, so a
//!   slow UI drops frames instead of queueing them and the helper never
//!   blocks on a full pipe), hands H.264 to the decoder thread, routes
//!   replies to waiting requests by id, and turns everything else into
//!   [`SessionEvent`]s;
//! - **decoder** — H.264 pictures on the GPU (macOS; see `video`);
//! - **writer** — owns the helper's stdin and paces key events 4 ms apart
//!   (the simulator drops keys sent back-to-back).
//!
//! Dropping the session closes stdin, which is the helper's signal to exit;
//! a helper that has not exited a second later is killed. Neither `Drop`
//! nor [`HelperSession::shutdown`] waits for that.

pub use crate::stream::{FrameData, StreamSession};

mod video;

use std::collections::HashMap;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::Value;

use crate::child_ledger::{Entry, Kind, Ledger};
use crate::helper::{self, Handshake, Hello, HelperOptions};
use crate::protocol::{self, AVCC_MIN_VERSION, Command, Event, Outbound, StreamFormat};
use crate::{DeviceId, Orientation, Result, SimError};

/// Pause between consecutive key commands.
pub const KEY_PACING: Duration = Duration::from_millis(4);

/// How long a closed helper gets to exit before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(1);

/// Something the UI should react to. Frames are not events: poll
/// [`HelperSession::latest_frame`] when woken.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// Portrait framebuffer size in pixels changed (or became known).
    Size { width: u32, height: u32 },
    /// A commanded orientation took effect; frames that follow are rotated.
    Orientation(Orientation),
    /// A non-fatal helper complaint (malformed/dropped command).
    Error(String),
    /// H.264 kept failing (to encode in the helper, or to decode here), so
    /// the stream is JPEG until the user picks H.264 again.
    EncodingFallback(String),
    /// The helper is gone. `fatal` carries its last words when it had any.
    Exited { code: Option<i32>, fatal: Option<String> },
    /// An event from a newer helper this build does not know.
    Unknown(Value),
}

type Wake = Arc<dyn Fn() + Send + Sync>;
type Reply = std::result::Result<Value, String>;
/// `(bytes, pace)`: `pace` asks the writer to wait [`KEY_PACING`] after.
type WriteReq = (Vec<u8>, bool);

/// Mutable view of the stream, shared with the dispatcher.
#[derive(Debug, Default)]
struct View {
    /// The newest picture, JPEG or decoded H.264, whichever came last.
    shown: Option<FrameData>,
    size: Option<(u32, u32)>,
    orientation: Option<Orientation>,
    exited: Option<Option<i32>>,
}

struct Inner {
    udid: DeviceId,
    pid: u32,
    hello: Hello,
    view: Mutex<View>,
    frame_seq: AtomicU64,
    /// Bumped by every [`HelperSession::set_format`], so a JPEG fallback
    /// ends when the user asks for H.264 again.
    format_requests: AtomicU64,
    /// Set when the decoder's queue was full and a picture was dropped: the
    /// decoder then waits for (and asks for) a key frame.
    video_dropped: std::sync::atomic::AtomicBool,
    next_id: AtomicU64,
    /// Requests awaiting a reply; `None` once the helper's output has ended,
    /// so a late request fails at once instead of waiting out its timeout.
    pending: Mutex<Option<HashMap<u64, mpsc::SyncSender<Reply>>>>,
    wake: RwLock<Option<Wake>>,
    events: Mutex<Option<mpsc::Receiver<SessionEvent>>>,
    writer: Mutex<Option<mpsc::Sender<WriteReq>>>,
    child: Mutex<Child>,
    ledger: Option<Arc<Ledger>>,
}

/// A running helper streaming one device. Cheap to clone; when the last
/// clone is dropped the helper is shut down (see [`HelperSession::shutdown`]).
#[derive(Clone)]
pub struct HelperSession {
    inner: Arc<Inner>,
    /// Shared by the clones only (the session threads hold `inner`, not
    /// this), so its drop marks "nobody is using the session any more".
    _guard: Arc<Guard>,
}

struct Guard(Arc<Inner>);

impl Drop for Guard {
    fn drop(&mut self) {
        shutdown(&self.0);
    }
}

impl HelperSession {
    /// Spawn the helper at `path` for `udid` and wait (≤ 10 s per step) for
    /// its `hello` and `ready`. Blocking: call from a background executor.
    pub fn start(path: &Path, udid: &DeviceId, opts: &HelperOptions) -> Result<Self> {
        Self::start_with_args(path, &opts.args(udid), udid, opts)
    }

    /// [`start`](Self::start) with an explicit argument list — for the
    /// helper's `--conformance` mode in tests, which has no device.
    pub fn start_with_args(path: &Path, args: &[String], udid: &DeviceId, opts: &HelperOptions) -> Result<Self> {
        let spawned = helper::spawn(path, args, opts.log_path.as_deref())?;
        let mut child = spawned.child;
        let pid = child.id();
        // Recorded before the handshake: a crash while waiting for `ready`
        // must not leave an unrecorded helper behind.
        if let Some(ledger) = &opts.ledger {
            let entry = Entry {
                pid,
                kind: Kind::Helper,
                exe: path.to_path_buf(),
                started_at_unix: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
                udid: Some(udid.0.clone()),
                owner_pid: std::process::id(),
            };
            if let Err(e) = ledger.record(entry) {
                tracing::warn!("could not record simulator helper {pid} in the child ledger: {e}");
            }
        }
        // Any failure from here on must not leave the helper running.
        let abandon = |child: &mut Child, e: SimError| {
            let _ = child.kill();
            let _ = child.wait();
            forget(opts.ledger.as_deref(), pid);
            e
        };
        let (tx, rx) = mpsc::channel();
        let stdout = spawned.stdout;
        if let Err(e) = std::thread::Builder::new()
            .name("oximux-sim-reader".into())
            .spawn(move || helper::pump(stdout, tx))
        {
            return Err(abandon(&mut child, e.into()));
        }
        let (hello, early) = match Handshake::run(&rx, true, helper::HANDSHAKE_TIMEOUT) {
            Ok(done) => done,
            Err(e) => return Err(abandon(&mut child, e)),
        };

        let (writer_tx, writer_rx) = mpsc::channel::<WriteReq>();
        let stdin = spawned.stdin;
        if let Err(e) = std::thread::Builder::new()
            .name("oximux-sim-writer".into())
            .spawn(move || write_loop(stdin, writer_rx))
        {
            return Err(abandon(&mut child, e.into()));
        }

        let (events_tx, events_rx) = mpsc::channel();
        let inner = Arc::new(Inner {
            udid: udid.clone(),
            pid,
            hello,
            view: Mutex::new(View { orientation: Some(opts.orientation), ..View::default() }),
            frame_seq: AtomicU64::new(0),
            format_requests: AtomicU64::new(0),
            video_dropped: std::sync::atomic::AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(Some(HashMap::new())),
            wake: RwLock::default(),
            events: Mutex::new(Some(events_rx)),
            writer: Mutex::new(Some(writer_tx)),
            child: Mutex::new(child),
            ledger: opts.ledger.clone(),
        });
        let (video_tx, video_rx) = mpsc::sync_channel(video::QUEUE);
        let decoder = Arc::clone(&inner);
        let decoder_events = events_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("oximux-sim-decode".into())
            .spawn(move || video::decode_loop(&decoder, &video_rx, &decoder_events))
        {
            shutdown(&inner);
            forget(inner.ledger.as_deref(), pid);
            return Err(e.into());
        }
        let dispatcher = Arc::clone(&inner);
        if let Err(e) = std::thread::Builder::new()
            .name("oximux-sim-dispatch".into())
            .spawn(move || dispatch_loop(&dispatcher, early, &rx, &events_tx, &video_tx))
        {
            shutdown(&inner);
            forget(inner.ledger.as_deref(), pid);
            return Err(e.into());
        }
        Ok(Self { _guard: Arc::new(Guard(Arc::clone(&inner))), inner })
    }

    pub fn udid(&self) -> &DeviceId {
        &self.inner.udid
    }

    /// The helper's process id (for the child ledger and diagnostics).
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// The helper's self-description from its `hello`.
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// Called (from a session thread) after every new frame and event. Keep it
    /// cheap: wake the UI, do the work there.
    ///
    /// The closure must not own a `HelperSession` clone: that clone would keep
    /// the session alive forever, so "shut down when the last clone drops"
    /// could never fire. Capture a weak UI handle instead.
    pub fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.inner.wake.write().unwrap() = Some(Arc::new(wake));
    }

    /// The event receiver. There is one per session; later calls get `None`.
    pub fn take_events(&self) -> Option<mpsc::Receiver<SessionEvent>> {
        self.inner.events.lock().unwrap().take()
    }

    /// The newest frame, when it is newer than `seen` (pass 0 at first). The
    /// returned sequence number is what to pass next time.
    pub fn latest_frame(&self, seen: u64) -> Option<(u64, FrameData)> {
        let seq = self.inner.frame_seq.load(Ordering::Acquire);
        if seq <= seen {
            return None;
        }
        let frame = self.inner.view.lock().unwrap().shown.clone()?;
        Some((seq, frame))
    }

    /// Whether this helper can stream H.264 ([`StreamFormat::Avcc`]).
    pub fn supports_avcc(&self) -> bool {
        self.inner.hello.proto >= AVCC_MIN_VERSION
    }

    /// Portrait framebuffer size in pixels, once the helper reported it.
    pub fn framebuffer_size(&self) -> Option<(u32, u32)> {
        self.inner.view.lock().unwrap().size
    }

    /// The orientation frames are currently rotated for.
    pub fn orientation(&self) -> Orientation {
        self.inner.view.lock().unwrap().orientation.unwrap_or(Orientation::Portrait)
    }

    /// `Some(code)` once the helper has exited.
    pub fn exited(&self) -> Option<Option<i32>> {
        self.inner.view.lock().unwrap().exited
    }

    /// Fire-and-forget input (touch, key, button, scroll, pause/resume). Key
    /// commands are paced by the writer thread.
    pub fn send(&self, command: &Command) -> Result<()> {
        let pace = matches!(command, Command::Key { .. });
        self.write(protocol::encode_command(command, None), pace)
    }

    /// Send `command` with an id and wait (≤ `timeout`) for its reply.
    /// Blocking: call from a background executor.
    pub fn request(&self, command: &Command, timeout: Duration) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::sync_channel(1);
        match self.inner.pending.lock().unwrap().as_mut() {
            Some(pending) => pending.insert(id, tx),
            None => return Err(SimError::HelperExited { code: self.exited().flatten() }),
        };
        let forget = || {
            if let Some(pending) = self.inner.pending.lock().unwrap().as_mut() {
                pending.remove(&id);
            }
        };
        if let Err(e) = self.write(protocol::encode_command(command, Some(id)), false) {
            forget();
            return Err(e);
        }
        let reply = rx.recv_timeout(timeout);
        forget();
        match reply {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(SimError::HelperFailed(message)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(SimError::Timeout {
                what: format!("simulator helper `{}`", command_name(command)),
                secs: timeout.as_secs(),
            }),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(SimError::HelperExited { code: self.exited().flatten() })
            }
        }
    }

    pub fn pause(&self) -> Result<()> {
        self.send(&Command::Pause)
    }

    pub fn resume(&self) -> Result<()> {
        self.send(&Command::Resume)
    }

    /// Change scale / fps / orientation. With an orientation, the reply
    /// arrives after the device rotated and the `Orientation` event fired.
    pub fn configure(
        &self,
        scale: Option<f64>,
        fps: Option<f64>,
        orientation: Option<Orientation>,
        timeout: Duration,
    ) -> Result<()> {
        self.request(&Command::Configure { scale, fps, orientation, format: None }, timeout).map(drop)
    }

    /// Switch the stream's encoding. A no-op on a helper without H.264 (it
    /// would reject the whole command).
    pub fn set_format(&self, format: StreamFormat, timeout: Duration) -> Result<()> {
        if !self.supports_avcc() {
            return Ok(());
        }
        self.inner.format_requests.fetch_add(1, Ordering::AcqRel);
        let command = Command::Configure { scale: None, fps: None, orientation: None, format: Some(format) };
        self.request(&command, timeout).map(drop)
    }

    /// A full-resolution PNG of the screen, rotated like the stream.
    pub fn screenshot_png(&self, timeout: Duration) -> Result<Vec<u8>> {
        let reply = self.request(&Command::Screenshot, timeout)?;
        let b64 = reply
            .get("png_base64")
            .and_then(Value::as_str)
            .ok_or_else(|| SimError::Protocol("screenshot reply has no png_base64".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| SimError::Protocol(format!("screenshot is not base64: {e}")))
    }

    /// Close stdin (the helper exits on EOF) and kill it if it has not exited
    /// within a second. Returns immediately; idempotent. Also runs when the
    /// last clone is dropped.
    pub fn shutdown(&self) {
        shutdown(&self.inner);
    }

    fn write(&self, bytes: Vec<u8>, pace: bool) -> Result<()> {
        let writer = self.inner.writer.lock().unwrap();
        let sent = writer.as_ref().map(|w| w.send((bytes, pace)).is_ok()).unwrap_or(false);
        if sent { Ok(()) } else { Err(SimError::HelperExited { code: self.exited().flatten() }) }
    }
}

fn shutdown(inner: &Arc<Inner>) {
    let Some(writer) = inner.writer.lock().unwrap().take() else { return };
    drop(writer);
    let inner = Arc::clone(inner);
    let _ = std::thread::Builder::new()
        .name("oximux-sim-reaper".into())
        .spawn(move || reap(&inner.child, EXIT_GRACE));
}

/// How long a killed helper may take to be reaped. A process wedged in
/// uninterruptible kernel state ignores SIGKILL; we give up on it rather
/// than block this thread (and the ledger cleanup after it) forever.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Wait up to `grace` for the helper to exit, then kill it and wait up to
/// [`KILL_GRACE`] more. Returns its exit code (`None` when killed by a signal
/// or unreapable). Never holds the lock while sleeping.
fn reap(child: &Mutex<Child>, grace: Duration) -> Option<i32> {
    let deadline = Instant::now() + grace;
    let mut killed_at: Option<Instant> = None;
    loop {
        {
            let mut child = child.lock().unwrap();
            if let Ok(Some(status)) = child.try_wait() {
                return status.code();
            }
            match killed_at {
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    killed_at = Some(Instant::now());
                }
                Some(at) if at.elapsed() >= KILL_GRACE => {
                    tracing::warn!(pid = child.id(), "simulator helper did not die after SIGKILL");
                    return None;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_loop(mut stdin: std::process::ChildStdin, rx: mpsc::Receiver<WriteReq>) {
    use std::io::Write as _;
    for (bytes, pace) in rx {
        if stdin.write_all(&bytes).and_then(|()| stdin.flush()).is_err() {
            return;
        }
        // Key events only: the one command whose back-to-back delivery the
        // simulator is known to drop.
        if pace {
            std::thread::sleep(KEY_PACING);
        }
    }
    // Channel closed: dropping `stdin` here is the helper's cue to exit.
}

fn dispatch_loop(
    inner: &Inner,
    early: Vec<Outbound>,
    rx: &mpsc::Receiver<Result<Outbound>>,
    events: &mpsc::Sender<SessionEvent>,
    video: &mpsc::SyncSender<protocol::Video>,
) {
    let mut fatal = None;
    for message in early.into_iter().map(Ok).chain(rx.iter()) {
        match message {
            Ok(Outbound::Frame(frame)) => {
                inner.view.lock().unwrap().shown = Some(FrameData::Jpeg(Arc::new(frame)));
                inner.frame_seq.fetch_add(1, Ordering::AcqRel);
            }
            Ok(Outbound::Video(message)) => {
                // Never wait on the decoder: replies and events behind this
                // picture must keep flowing. Decoded pictures wake the UI
                // from the decoder's callback.
                if let Err(mpsc::TrySendError::Full(_)) = video.try_send(message) {
                    inner.video_dropped.store(true, Ordering::Release);
                }
                continue;
            }
            Ok(Outbound::Event(event)) => match event {
                Event::Response { id, result } => {
                    let waiter = inner.pending.lock().unwrap().as_mut().and_then(|p| p.remove(&id));
                    if let Some(waiter) = waiter {
                        let _ = waiter.try_send(result);
                    }
                    continue; // replies wake their waiter, not the UI
                }
                Event::Size { width, height } => {
                    inner.view.lock().unwrap().size = Some((width, height));
                    let _ = events.send(SessionEvent::Size { width, height });
                }
                Event::Orientation(o) => {
                    inner.view.lock().unwrap().orientation = Some(o);
                    let _ = events.send(SessionEvent::Orientation(o));
                }
                Event::Error { message } => {
                    let _ = events.send(SessionEvent::Error(message));
                }
                Event::Format { message, .. } => {
                    let _ = events.send(SessionEvent::EncodingFallback(message));
                }
                Event::Fatal { message, .. } => fatal = Some(message),
                Event::Unknown(value) => {
                    let _ = events.send(SessionEvent::Unknown(value));
                }
                Event::Malformed(why) => {
                    let _ = events.send(SessionEvent::Error(format!("undecodable helper event: {why}")));
                }
                // Handshake and conformance events carry nothing new here.
                Event::Hello { .. } | Event::Ready { .. } | Event::Parsed { .. } | Event::ConformanceReady => {
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!(udid = %inner.udid, "simulator helper stream: {e}");
                break;
            }
        }
        notify(inner);
    }
    // stdout closed: the helper is exiting (or already gone). Fail every
    // waiting request now, refuse new ones, and stop accepting writes.
    inner.pending.lock().unwrap().take();
    inner.writer.lock().unwrap().take();
    let code = reap(&inner.child, EXIT_GRACE);
    forget(inner.ledger.as_deref(), inner.pid);
    inner.view.lock().unwrap().exited = Some(code);
    let _ = events.send(SessionEvent::Exited { code, fatal });
    notify(inner);
}

/// The helper is reaped: drop it from the ledger (a failure only means the
/// next launch's `reap_stale` prunes a dead pid).
fn forget(ledger: Option<&Ledger>, pid: u32) {
    if let Some(Err(e)) = ledger.map(|l| l.remove(pid)) {
        tracing::warn!("could not drop simulator helper {pid} from the child ledger: {e}");
    }
}

/// Also runs inside VideoToolbox's callback, so it never panics.
fn notify(inner: &Inner) {
    let wake = inner.wake.read().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    if let Some(wake) = wake {
        wake();
    }
}

fn command_name(command: &Command) -> String {
    command.to_json(None).get("cmd").and_then(Value::as_str).unwrap_or("?").to_owned()
}
