//! Microphone capture via cpal (CoreAudio on macOS).
//!
//! Runs on a dedicated thread that owns the cpal `Stream` for its whole life
//! (the stream is `!Send` on macOS, so it can never cross a thread boundary).
//! The audio callback downmixes to mono, appends to a shared session buffer at
//! the device-default rate, and emits throttled RMS level events. Capture never
//! requests 16 kHz from the device — that path can yield silence on macOS; the
//! worker resamples on stop instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures::channel::mpsc::UnboundedSender;

use crate::resample::downmix_to_mono;
use crate::{DictationEvent, MAX_RECORDING_SECS};

/// Audio accumulated for one dictation session, written by the capture thread
/// and drained by the worker on stop. Guarded by a mutex; the audio callback
/// locks it only to append (cheap; contention is negligible at ~100 Hz).
#[derive(Default)]
pub struct SessionBuffer {
    pub samples: Vec<f32>,
    /// Device-default capture rate; set once when the stream config is known.
    pub device_rate: u32,
    /// Set true when the 2-minute hard cap is hit — the worker auto-stops.
    pub capped: bool,
}

impl SessionBuffer {
    pub fn reset(&mut self) {
        self.samples.clear();
        self.device_rate = 0;
        self.capped = false;
    }
}

/// Emit RMS level at ~10 Hz to stay inside the repaint budget.
const LEVEL_INTERVAL: Duration = Duration::from_millis(100);

/// Build + run the capture stream until `stop` is set, then drop it. Blocking;
/// call on its own thread. All failures are reported as [`DictationEvent::Error`]
/// (never a silent zeroed-audio "success").
pub fn run(
    shared: Arc<Mutex<SessionBuffer>>,
    events: UnboundedSender<DictationEvent>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = run_inner(&shared, &events, &stop) {
        let _ = events.unbounded_send(DictationEvent::Error(format!("microphone: {e}")));
        stop.store(true, Ordering::SeqCst);
    }
}

fn run_inner(
    shared: &Arc<Mutex<SessionBuffer>>,
    events: &UnboundedSender<DictationEvent>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    let supported = device.default_input_config()?;
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels;
    // cpal 0.18: `SampleRate` is a bare `u32`.
    let device_rate = config.sample_rate;
    let max_samples = MAX_RECORDING_SECS as usize * device_rate as usize;

    {
        let mut buf = shared.lock().unwrap();
        buf.device_rate = device_rate;
    }

    let err_events = events.clone();
    let err_fn = move |err| {
        let _ = err_events.unbounded_send(DictationEvent::Error(format!("stream: {err}")));
    };

    // The callback appends mono samples and emits throttled RMS. `last_emit`
    // persists across callback invocations (FnMut captures it by value).
    let cb_shared = Arc::clone(shared);
    let cb_events = events.clone();
    let mut last_emit = Instant::now();
    let mut on_mono = move |mono: Vec<f32>| {
        // RMS of this chunk, throttled.
        if last_emit.elapsed() >= LEVEL_INTERVAL && !mono.is_empty() {
            let sum_sq: f32 = mono.iter().map(|s| s * s).sum();
            let rms = (sum_sq / mono.len() as f32).sqrt();
            let _ = cb_events.unbounded_send(DictationEvent::Level(rms));
            last_emit = Instant::now();
        }
        let mut buf = cb_shared.lock().unwrap();
        if buf.samples.len() >= max_samples {
            buf.capped = true;
            return;
        }
        let room = max_samples - buf.samples.len();
        if mono.len() <= room {
            buf.samples.extend_from_slice(&mono);
        } else {
            buf.samples.extend_from_slice(&mono[..room]);
            buf.capped = true;
        }
    };

    // cpal 0.18 passes `config` by value and the callback info by ref. We
    // handle the formats CoreAudio actually hands out for input (F32 is the
    // overwhelming default on macOS); anything exotic errors clearly rather
    // than capturing garbage.
    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| on_mono(downmix_to_mono(data, channels)),
            err_fn,
            None,
        )?,
        SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                on_mono(downmix_to_mono(&f, channels))
            },
            err_fn,
            None,
        )?,
        SampleFormat::I32 => device.build_input_stream(
            config,
            move |data: &[i32], _| {
                let f: Vec<f32> = data.iter().map(|s| *s as f32 / i32::MAX as f32).collect();
                on_mono(downmix_to_mono(&f, channels))
            },
            err_fn,
            None,
        )?,
        SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| {
                // u16 origin is 32768; center then normalize to [-1, 1].
                let f: Vec<f32> = data
                    .iter()
                    .map(|s| (*s as f32 - 32768.0) / 32768.0)
                    .collect();
                on_mono(downmix_to_mono(&f, channels))
            },
            err_fn,
            None,
        )?,
        other => anyhow::bail!("unsupported input sample format {other:?}"),
    };

    stream.play()?;
    // Hold the stream alive until asked to stop; the callback runs on
    // CoreAudio's own thread meanwhile.
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(30));
    }
    drop(stream);
    Ok(())
}

/// Whether a default input device exists at all — cheap gate for the mic
/// button's "no microphone" state, checked without opening a stream.
pub fn has_input_device() -> bool {
    cpal::default_host().default_input_device().is_some()
}
