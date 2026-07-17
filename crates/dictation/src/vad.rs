//! Voice-activity detection (Silero VAD) for trimming silence before decode.
//!
//! The default silence gate ([`crate::engine::is_silent`]) is all-or-nothing: it
//! only skips a recording that is silent end to end. Silero VAD instead segments
//! a recording into speech spans and drops the gaps, so leading/trailing pauses
//! and mid-utterance silence never reach whisper. That both speeds decode and
//! kills whisper's habit of hallucinating captions ("(sad music)") over silence.
//!
//! sherpa-onnx bundles Silero VAD (`sherpa_rs::silero_vad`), so this needs no new
//! dependency — only the ~629 KB `silero_vad.onnx`, fetched once on demand into
//! the app data dir (the archive-oriented [`crate::model_manager`] only handles
//! `.tar.bz2`, so a raw single-file model gets its own tiny fetch here).
//!
//! Lifecycle: the worker warms one [`Vad`] and reuses it, calling [`Vad::clear`]
//! between recordings to reset the detector's state. Loading is cheap (small
//! model) and failure is non-fatal — the caller falls back to the peak gate.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};

/// On-disk name of the Silero VAD model in the app data dir.
pub const VAD_MODEL_FILE: &str = "silero_vad.onnx";

/// Source for the Silero VAD model — the same k2-fsa `asr-models` release the
/// speech models come from. ~629 KB, single raw `.onnx` (no archive).
pub const VAD_MODEL_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// Silero v4 detection window at 16 kHz (samples). The model is trained on this
/// size; other values degrade detection.
const WINDOW_SIZE: i32 = 512;
/// Speech probability above this counts as voice. Silero's usual default;
/// conservative enough not to clip quiet dictation.
const THRESHOLD: f32 = 0.5;
/// Trailing silence this long (s) ends a speech segment — long enough that a
/// brief pause between words does not split an utterance or clip consonants.
const MIN_SILENCE_SECS: f32 = 0.35;
/// A run of voice shorter than this (s) is treated as noise, not speech.
const MIN_SPEECH_SECS: f32 = 0.10;
/// Never let a single segment exceed this (s); the whisper chunker handles long
/// audio, so this only bounds the detector's internal buffer.
const MAX_SPEECH_SECS: f32 = 30.0;

/// The path the VAD model lives at, given the dictation data dir (the parent of
/// the per-model directories).
pub fn model_path(data_dir: &Path) -> PathBuf {
    data_dir.join(VAD_MODEL_FILE)
}

/// Ensure `silero_vad.onnx` exists in `data_dir`, downloading it once if absent.
/// Blocking (meant for the worker thread). Returns the model path on success.
/// The write is atomic (temp file + rename) so an interrupted download never
/// leaves a truncated model that would fail to load.
pub fn ensure_downloaded(data_dir: &Path) -> Result<PathBuf> {
    let path = model_path(data_dir);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(data_dir).context("create data dir for VAD model")?;

    let resp = ureq::get(VAD_MODEL_URL)
        .call()
        .context("download silero_vad.onnx")?;
    let tmp = path.with_extension("onnx.partial");
    // Stream into the temp file; on any error, remove the partial so it never
    // lingers as disk debris (a later retry re-downloads from scratch anyway).
    let stream = || -> Result<()> {
        use std::io::Write;
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&tmp).context("create VAD temp file")?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = std::io::Read::read(&mut reader, &mut buf).context("read VAD body")?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).context("write VAD temp file")?;
        }
        file.flush().ok();
        std::fs::rename(&tmp, &path).context("finalize VAD model")?;
        Ok(())
    };
    if let Err(e) = stream() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    tracing::info!(path = %path.display(), "downloaded Silero VAD model");
    Ok(path)
}

/// A warm Silero VAD detector. Reused across recordings; call [`Vad::clear`]
/// between them.
pub struct Vad {
    inner: SileroVad,
}

impl Vad {
    /// Load the detector from a `silero_vad.onnx` at `model_path`. The buffer
    /// size (30 s) bounds how much audio the detector holds internally.
    pub fn load(model_path: &Path) -> Result<Self> {
        let config = SileroVadConfig {
            model: model_path.to_string_lossy().into_owned(),
            min_silence_duration: MIN_SILENCE_SECS,
            min_speech_duration: MIN_SPEECH_SECS,
            max_speech_duration: MAX_SPEECH_SECS,
            threshold: THRESHOLD,
            sample_rate: crate::resample::TARGET_SAMPLE_RATE,
            window_size: WINDOW_SIZE,
            ..Default::default()
        };
        let inner = SileroVad::new(config, MAX_SPEECH_SECS)
            .map_err(|e| anyhow::anyhow!("load Silero VAD: {e}"))?;
        Ok(Self { inner })
    }

    /// Return only the speech portions of a 16 kHz mono buffer, silence trimmed,
    /// with segments concatenated in order. An empty result means the detector
    /// found no speech (caller should treat it as silence). Resets detector
    /// state first so a prior recording can't leak in.
    pub fn keep_speech(&mut self, samples_16k: &[f32]) -> Vec<f32> {
        self.inner.clear();
        self.inner.accept_waveform(samples_16k.to_vec());
        self.inner.flush();

        let mut out = Vec::new();
        while !self.inner.is_empty() {
            let seg = self.inner.front();
            out.extend_from_slice(&seg.samples);
            self.inner.pop();
        }
        out
    }

    /// Reset detector state between recordings (also done at the start of
    /// [`Vad::keep_speech`], but exposed for explicit teardown).
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}
