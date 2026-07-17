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
/// Samples of audio to keep *before* each detected segment start. Silero flags
/// speech only once its probability crosses [`THRESHOLD`], a few frames after
/// the true onset, so the reported start lands late and clips the first (often
/// short) word. Backing up ~0.25 s recovers that onset. 0.25 s × 16 kHz.
const PRE_PAD_SAMPLES: usize = (0.25 * crate::resample::TARGET_SAMPLE_RATE as f32) as usize;
/// Samples to keep *after* each segment end — the mirror case, recovering a
/// trailing word whose tail dips below threshold before it truly ends. 0.20 s.
const POST_PAD_SAMPLES: usize = (0.20 * crate::resample::TARGET_SAMPLE_RATE as f32) as usize;

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

/// A single-use Silero VAD detector. Construct one per recording and drop it —
/// sherpa-rs 0.6.8 exposes only `clear()` (the segment queue), NOT a full model
/// reset, so a reused detector would leak the previous recording's buffer + LSTM
/// state and progressively corrupt detection. The model is small (~629 KB) so a
/// fresh load per recording is cheap.
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

    /// Return only the speech portions of a 16 kHz mono buffer, silence trimmed.
    /// An empty result means the detector found no speech.
    ///
    /// The detector's speech/silence state machine advances one `window_size`
    /// frame at a time, so audio MUST be fed in `window_size` chunks — feeding
    /// one large buffer collapses the state machine (its segment start is taken
    /// relative to the post-feed buffer tail, yielding only a tiny tail
    /// fragment). Segments are drained as they close during feeding, then a
    /// final `flush` emits any still-open segment.
    ///
    /// Rather than concatenating each segment's clipped `samples`, we take the
    /// segment's `start` offset and re-slice the ORIGINAL buffer with pre/post
    /// padding (see [`PRE_PAD_SAMPLES`]). That restores the word onset Silero's
    /// threshold lag would otherwise cut, and keeps the untouched original audio
    /// (no re-extraction artifacts). Padded ranges are merged so overlapping
    /// segments never duplicate samples.
    pub fn keep_speech(&mut self, samples_16k: &[f32]) -> Vec<f32> {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for chunk in samples_16k.chunks(WINDOW_SIZE as usize) {
            self.inner.accept_waveform(chunk.to_vec());
            self.collect_ranges(&mut ranges, samples_16k.len());
        }
        self.inner.flush();
        self.collect_ranges(&mut ranges, samples_16k.len());
        Self::slice_padded(samples_16k, &ranges)
    }

    /// Append every currently-queued speech segment as a `[start, end)` sample
    /// range into the original buffer (bounds-clamped), then drop it from the
    /// queue. `start` is the segment's absolute offset in the fed stream, which
    /// — since the whole recording is fed once into a fresh detector — indexes
    /// straight into the original buffer.
    fn collect_ranges(&mut self, ranges: &mut Vec<(usize, usize)>, len: usize) {
        while !self.inner.is_empty() {
            let seg = self.inner.front();
            let start = (seg.start.max(0) as usize).min(len);
            let end = start.saturating_add(seg.samples.len()).min(len);
            if end > start {
                ranges.push((start, end));
            }
            self.inner.pop();
        }
    }

    /// Slice the original buffer for each range, padded and merged. Ranges arrive
    /// in emission order (ascending start), so a single forward merge suffices.
    fn slice_padded(samples_16k: &[f32], ranges: &[(usize, usize)]) -> Vec<f32> {
        let n = samples_16k.len();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for &(s, e) in ranges {
            let s = s.saturating_sub(PRE_PAD_SAMPLES);
            let e = (e + POST_PAD_SAMPLES).min(n);
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        let mut out = Vec::new();
        for (s, e) in merged {
            out.extend_from_slice(&samples_16k[s..e]);
        }
        out
    }
}
