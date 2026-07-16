//! Session lifecycle for one dictation press: capture → resample → decode.
//!
//! One worker thread owns the warm engine cache and the session state machine
//! (Idle → Recording → Transcribing → Idle/Failed). A capture thread fills a
//! shared buffer immediately on start (buffer-while-warming) even while the
//! worker is still loading the ONNX graphs. Nothing here touches GPUI; the app
//! drains [`DictationEvent`]s from the returned channel.
//!
//! `Drop` must never block the GPUI main thread (see the shutdown-on-main-thread
//! hazard): it just drops the command sender, which ends the worker's `recv`,
//! and signals capture to stop. The detached threads unwind on their own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::capture::{self, SessionBuffer};
use crate::engine::{Engine, ModelPaths, is_silent};
use crate::resample::{TARGET_SAMPLE_RATE, resample_linear};

/// Events emitted to the app for one session. Level is pre-throttled (~10 Hz).
#[derive(Debug, Clone)]
pub enum DictationEvent {
    /// Capture started (permission granted, device open).
    Started,
    /// RMS microphone level in `0.0..~1.0`, for the meter.
    Level(f32),
    /// Recording stopped; decoding in progress.
    Transcribing,
    /// The final transcript (may be empty when only silence was captured).
    Final(String),
    /// Recording was cancelled (Escape); no transcript will follow.
    Cancelled,
    /// The 2-minute hard cap fired and recording auto-stopped; a `Final` follows.
    Capped,
    /// A fatal error for this session; the pill returns to Idle.
    Error(String),
}

/// Hard cap on a single recording — 2 minutes. The crate enforces this (bounds
/// memory); the UI shows its own timer/toast.
pub const MAX_RECORDING_SECS: u64 = 120;

/// Drop a warm engine after this long idle to release ONNX memory.
const IDLE_TEARDOWN: Duration = Duration::from_secs(600);
/// How often the worker polls for stop/cancel/cap while recording.
const POLL_INTERVAL: Duration = Duration::from_millis(30);

// One command per user press over an mpsc channel — never hot. Boxing the large
// Start payload to shrink Stop/Cancel would only add an allocation for no gain.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Begin a session with the given model and optional named input device
    /// (`None` = system default).
    Start(ModelPaths, Option<String>),
    Stop,
    Cancel,
}

/// Handle to the dictation worker. Cheap to clone-free share via `&self`.
pub struct DictationController {
    cmd_tx: Sender<Command>,
    /// Set while a session is active, so the app can gate a second `start`.
    active: Arc<AtomicBool>,
}

impl DictationController {
    /// Spawn the worker thread and return the controller plus the event stream
    /// the app drains (via `cx.spawn`).
    pub fn new() -> (Self, UnboundedReceiver<DictationEvent>) {
        let (cmd_tx, cmd_rx) = channel::<Command>();
        let (evt_tx, evt_rx) = unbounded::<DictationEvent>();
        let active = Arc::new(AtomicBool::new(false));
        let worker_active = Arc::clone(&active);
        std::thread::Builder::new()
            .name("oximux-dictation".into())
            .spawn(move || worker_loop(cmd_rx, evt_tx, worker_active))
            .expect("spawn dictation worker");
        (Self { cmd_tx, active }, evt_rx)
    }

    /// Begin a recording session with `paths`, capturing from `device` (a cpal
    /// input-device name, or `None` for the system default). A second start
    /// while active is a no-op (returns `false`) — one session at a time.
    pub fn start(&self, paths: ModelPaths, device: Option<String>) -> bool {
        if self.active.swap(true, Ordering::SeqCst) {
            return false; // already recording
        }
        self.cmd_tx.send(Command::Start(paths, device)).is_ok()
    }

    /// Stop recording and transcribe → emits `Transcribing` then `Final`.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    /// Cancel recording and discard the audio → emits `Cancelled`.
    pub fn cancel(&self) {
        let _ = self.cmd_tx.send(Command::Cancel);
    }

    /// Whether a session is currently active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
}

impl Drop for DictationController {
    fn drop(&mut self) {
        // Dropping `cmd_tx` ends the worker's blocking recv → it returns and the
        // capture thread (if any) is stopped inside the worker. No join here:
        // this can run on the GPUI main thread and must not block.
    }
}

/// Outcome of a recording poll loop.
enum Outcome {
    Stop,
    Cancel,
    Capped,
    Disconnected,
}

fn worker_loop(
    cmd_rx: Receiver<Command>,
    evt_tx: UnboundedSender<DictationEvent>,
    active: Arc<AtomicBool>,
) {
    // Warm engine cache: kept across presses, dropped after idle teardown.
    let mut engine: Option<Engine> = None;

    loop {
        // Block for the next Start, tearing the engine down if idle too long.
        let cmd = match cmd_rx.recv_timeout(IDLE_TEARDOWN) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                engine = None; // release ONNX memory
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };

        let (paths, device) = match cmd {
            Command::Start(paths, device) => (paths, device),
            // Stop/Cancel with no active session: stale, ignore.
            Command::Stop | Command::Cancel => continue,
        };

        // Catch a panic inside the session (e.g. a poisoned session-buffer
        // mutex if the capture callback panicked) so a single failed session
        // never wedges dictation: without this, the panic would unwind the
        // worker thread, `active` would stay stuck `true`, and every future
        // press would report "busy" until an app restart.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_session(&cmd_rx, &evt_tx, &mut engine, paths, device);
        }));
        active.store(false, Ordering::SeqCst);
        if outcome.is_err() {
            // The engine may be in an inconsistent state after a panic — drop
            // it so the next session rebuilds cleanly.
            engine = None;
            let _ = evt_tx.unbounded_send(DictationEvent::Error(
                "dictation recovered from an internal error; try again".into(),
            ));
        }
    }
}

/// Drive one Start→(Stop|Cancel|Cap) session end to end.
fn run_session(
    cmd_rx: &Receiver<Command>,
    evt_tx: &UnboundedSender<DictationEvent>,
    engine: &mut Option<Engine>,
    paths: ModelPaths,
    device: Option<String>,
) {
    let shared = Arc::new(Mutex::new(SessionBuffer::default()));
    {
        shared.lock().unwrap().reset();
    }
    let capture_stop = Arc::new(AtomicBool::new(false));

    // Start capturing immediately (buffer-while-warming).
    let cap_shared = Arc::clone(&shared);
    let cap_events = evt_tx.clone();
    let cap_stop = Arc::clone(&capture_stop);
    let capture_handle = std::thread::Builder::new()
        .name("oximux-dictation-capture".into())
        .spawn(move || capture::run(cap_shared, cap_events, cap_stop, device))
        .ok();

    let _ = evt_tx.unbounded_send(DictationEvent::Started);

    // Load (or reuse) the engine while capture fills the buffer.
    let load_ok = ensure_engine(engine, &paths, evt_tx);

    // Poll for stop/cancel/cap.
    let outcome = poll_recording(cmd_rx, &shared);
    capture_stop.store(true, Ordering::SeqCst);
    if let Some(h) = capture_handle {
        let _ = h.join(); // capture thread exits promptly after stop flag
    }

    match outcome {
        Outcome::Cancel => {
            let _ = evt_tx.unbounded_send(DictationEvent::Cancelled);
            return;
        }
        Outcome::Disconnected => return, // controller dropped mid-session
        Outcome::Capped => {
            let _ = evt_tx.unbounded_send(DictationEvent::Capped);
        }
        Outcome::Stop => {}
    }

    if !load_ok {
        return; // engine failed to load; error already emitted
    }

    // Take the audio + device rate, resample to 16 kHz, decode.
    let (samples, device_rate) = {
        let mut buf = shared.lock().unwrap();
        (std::mem::take(&mut buf.samples), buf.device_rate)
    };
    let _ = evt_tx.unbounded_send(DictationEvent::Transcribing);

    let s16 = if device_rate == 0 || device_rate == TARGET_SAMPLE_RATE {
        samples
    } else {
        resample_linear(&samples, device_rate, TARGET_SAMPLE_RATE)
    };

    if is_silent(&s16) {
        let _ = evt_tx.unbounded_send(DictationEvent::Final(String::new()));
        return;
    }

    match engine.as_mut() {
        Some(eng) => {
            let text = eng.decode_recording(&s16);
            let _ = evt_tx.unbounded_send(DictationEvent::Final(text));
        }
        None => {
            let _ = evt_tx.unbounded_send(DictationEvent::Error("engine unavailable".into()));
        }
    }
}

/// Load `paths` into the warm cache unless the same model is already loaded.
/// Emits `Error` and returns false on failure.
fn ensure_engine(
    engine: &mut Option<Engine>,
    paths: &ModelPaths,
    evt_tx: &UnboundedSender<DictationEvent>,
) -> bool {
    if engine.as_ref().map(|e| e.model_id()) == Some(paths.id.as_str()) {
        return true;
    }
    match Engine::load(paths) {
        Ok(e) => {
            *engine = Some(e);
            true
        }
        Err(e) => {
            *engine = None;
            let _ = evt_tx.unbounded_send(DictationEvent::Error(format!("model load: {e}")));
            false
        }
    }
}

/// Poll the command channel + shared cap flag until the session ends.
fn poll_recording(cmd_rx: &Receiver<Command>, shared: &Arc<Mutex<SessionBuffer>>) -> Outcome {
    loop {
        match cmd_rx.try_recv() {
            Ok(Command::Stop) => return Outcome::Stop,
            Ok(Command::Cancel) => return Outcome::Cancel,
            Ok(Command::Start(..)) => { /* ignore re-start during a session */ }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Outcome::Disconnected,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if shared.lock().unwrap().capped {
            return Outcome::Capped;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_start_is_rejected_while_active() {
        let (ctrl, _rx) = DictationController::new();
        // A fake ModelPaths whose files don't exist — the session will fail to
        // load, but `start` still flips active and the second start must fail.
        let paths = ModelPaths {
            id: "test".into(),
            dir: std::path::PathBuf::from("/nonexistent"),
            kind: crate::engine::EngineKind::Whisper { language: None },
            encoder: "/nonexistent/e.onnx".into(),
            decoder: "/nonexistent/d.onnx".into(),
            joiner: None,
            tokens: "/nonexistent/t.txt".into(),
            model: None,
        };
        assert!(ctrl.start(paths.clone(), None), "first start accepted");
        assert!(!ctrl.start(paths, None), "second start rejected while active");
    }

    #[test]
    fn stop_and_cancel_before_start_are_harmless() {
        let (ctrl, _rx) = DictationController::new();
        // No active session; these are dropped by the worker without panic.
        ctrl.stop();
        ctrl.cancel();
        assert!(!ctrl.is_active());
    }
}
