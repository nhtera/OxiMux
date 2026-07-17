//! Offline speech recognition over sherpa-onnx.
//!
//! Two model families, one decode surface:
//! - **Whisper** (multilingual incl. Vietnamese) — the default; `language`
//!   `None` means whisper auto-detect (empty language string upstream).
//! - **Transducer** (NeMo Parakeet) — best English quality, no Vietnamese.
//!
//! The engine is CPU-only (sherpa-rs disables CoreML upstream because several
//! models misbehave on it). It is intentionally free of any capture/GPUI type
//! — the app only ever sees text out through the controller.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sherpa_rs::sense_voice::{SenseVoiceConfig, SenseVoiceRecognizer};
use sherpa_rs::transducer::{TransducerConfig, TransducerRecognizer};
use sherpa_rs::whisper::{WhisperConfig, WhisperRecognizer};
use sherpa_rs::zipformer::{ZipFormer, ZipFormerConfig};

use crate::resample::TARGET_SAMPLE_RATE;

/// Which recognizer family a model drives, plus the whisper language selector.
#[derive(Debug, Clone)]
pub enum EngineKind {
    /// Whisper. `language == None` → auto-detect; `Some("vi")` / `Some("en")`
    /// pin the language (sherpa passes it straight to whisper).
    Whisper { language: Option<String> },
    /// NeMo transducer (Parakeet). English/European only.
    Transducer,
    /// Icefall zipformer offline transducer (e.g. the dedicated Vietnamese
    /// models). Same 4 files as a transducer, but a distinct sherpa recognizer
    /// (`ZipFormer`, generic model type — not `nemo_transducer`).
    Zipformer,
    /// SenseVoice — a single-file multilingual (zh/en/ja/ko/yue) offline model.
    SenseVoice,
}

/// Concrete on-disk model files resolved by the Phase 2 catalog for a ready
/// model. The engine is built purely from this — the catalog owns file naming.
#[derive(Debug, Clone)]
pub struct ModelPaths {
    /// Stable model id, used as the warm-cache key.
    pub id: String,
    /// Directory holding the model files (for logging / relative resolution).
    pub dir: PathBuf,
    pub kind: EngineKind,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    /// Transducer / zipformer models only. `None` for whisper / sense-voice.
    pub joiner: Option<PathBuf>,
    pub tokens: PathBuf,
    /// Single-file model families (SenseVoice). `None` for the encoder/decoder
    /// families (whisper / transducer / zipformer).
    pub model: Option<PathBuf>,
}

impl ModelPaths {
    fn require_files(&self) -> Result<()> {
        // Single-file families (SenseVoice) carry only `model` + `tokens`; the
        // encoder/decoder families carry encoder/decoder/tokens (+joiner).
        let needed: Vec<&Path> = match &self.kind {
            EngineKind::SenseVoice => {
                let mut v = vec![self.tokens.as_path()];
                if let Some(m) = &self.model {
                    v.push(m.as_path());
                }
                v
            }
            _ => {
                let mut v = vec![
                    self.encoder.as_path(),
                    self.decoder.as_path(),
                    self.tokens.as_path(),
                ];
                if let Some(j) = &self.joiner {
                    v.push(j.as_path());
                }
                v
            }
        };
        let missing: Vec<&Path> = needed.into_iter().filter(|p| !p.exists()).collect();
        if !missing.is_empty() {
            bail!("model {} missing files: {:?}", self.id, missing);
        }
        Ok(())
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// A loaded recognizer. Warm-cached by the controller across dictation presses;
/// `Send` (sherpa's recognizers are `Send + Sync`) so it can live on the worker
/// thread.
pub struct Engine {
    model_id: String,
    inner: Recognizer,
}

enum Recognizer {
    Whisper(WhisperRecognizer),
    Transducer(TransducerRecognizer),
    Zipformer(ZipFormer),
    SenseVoice(SenseVoiceRecognizer),
}

impl Engine {
    /// The id of the model this engine was built from (warm-cache key).
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Build the recognizer for `paths`. Expensive (loads ONNX graphs) — the
    /// controller does this off the main thread and caches the result.
    pub fn load(paths: &ModelPaths) -> Result<Self> {
        paths.require_files()?;
        let inner = match &paths.kind {
            EngineKind::Whisper { language } => {
                let cfg = WhisperConfig {
                    encoder: path_str(&paths.encoder),
                    decoder: path_str(&paths.decoder),
                    tokens: path_str(&paths.tokens),
                    // Empty string → whisper auto-detect; otherwise the pinned code.
                    language: language.clone().unwrap_or_default(),
                    num_threads: Some(default_threads()),
                    ..Default::default()
                };
                Recognizer::Whisper(
                    WhisperRecognizer::new(cfg)
                        .map_err(|e| anyhow::anyhow!("whisper load failed: {e}"))
                        .with_context(|| format!("model {}", paths.id))?,
                )
            }
            EngineKind::Transducer => {
                let joiner = paths
                    .joiner
                    .as_ref()
                    .context("transducer model requires a joiner file")?;
                let cfg = TransducerConfig {
                    encoder: path_str(&paths.encoder),
                    decoder: path_str(&paths.decoder),
                    joiner: path_str(joiner),
                    tokens: path_str(&paths.tokens),
                    num_threads: default_threads(),
                    sample_rate: TARGET_SAMPLE_RATE as i32,
                    feature_dim: 80,
                    decoding_method: "greedy_search".to_string(),
                    // NeMo Parakeet TDT models are transducers; sherpa keys the
                    // decode path off this string.
                    model_type: "nemo_transducer".to_string(),
                    ..Default::default()
                };
                Recognizer::Transducer(
                    TransducerRecognizer::new(cfg)
                        .map_err(|e| anyhow::anyhow!("transducer load failed: {e}"))
                        .with_context(|| format!("model {}", paths.id))?,
                )
            }
            EngineKind::Zipformer => {
                let joiner = paths
                    .joiner
                    .as_ref()
                    .context("zipformer model requires a joiner file")?;
                let cfg = ZipFormerConfig {
                    encoder: path_str(&paths.encoder),
                    decoder: path_str(&paths.decoder),
                    joiner: path_str(joiner),
                    tokens: path_str(&paths.tokens),
                    num_threads: Some(default_threads()),
                    ..Default::default()
                };
                Recognizer::Zipformer(
                    ZipFormer::new(cfg)
                        .map_err(|e| anyhow::anyhow!("zipformer load failed: {e}"))
                        .with_context(|| format!("model {}", paths.id))?,
                )
            }
            EngineKind::SenseVoice => {
                let model = paths
                    .model
                    .as_ref()
                    .context("sense-voice model requires a model file")?;
                let cfg = SenseVoiceConfig {
                    model: path_str(model),
                    tokens: path_str(&paths.tokens),
                    num_threads: Some(default_threads()),
                    ..Default::default()
                };
                Recognizer::SenseVoice(
                    SenseVoiceRecognizer::new(cfg)
                        .map_err(|e| anyhow::anyhow!("sense-voice load failed: {e}"))
                        .with_context(|| format!("model {}", paths.id))?,
                )
            }
        };
        Ok(Self {
            model_id: paths.id.clone(),
            inner,
        })
    }

    /// Decode one buffer of 16 kHz mono f32 samples to text.
    fn decode_one(&mut self, samples_16k: &[f32]) -> String {
        match &mut self.inner {
            Recognizer::Whisper(w) => w.transcribe(TARGET_SAMPLE_RATE, samples_16k).text,
            Recognizer::Transducer(t) => t.transcribe(TARGET_SAMPLE_RATE, samples_16k),
            // `ZipFormer::decode` takes an owned buffer.
            Recognizer::Zipformer(z) => z.decode(TARGET_SAMPLE_RATE, samples_16k.to_vec()),
            Recognizer::SenseVoice(s) => s.transcribe(TARGET_SAMPLE_RATE, samples_16k).text,
        }
    }

    /// Decode a full recording, splitting long audio into ≤30 s chunks at quiet
    /// windows. A single huge ONNX allocation can SIGTRAP (sherpa #7925); the
    /// chunker keeps every decode bounded. Segment texts join with one space.
    pub fn decode_recording(&mut self, samples_16k: &[f32]) -> String {
        let bounds = chunk_boundaries(samples_16k, TARGET_SAMPLE_RATE);
        let mut parts: Vec<String> = Vec::with_capacity(bounds.len().saturating_sub(1).max(1));
        for pair in bounds.windows(2) {
            let seg = &samples_16k[pair[0]..pair[1]];
            let text = self.decode_one(seg);
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
        parts.join(" ")
    }
}

/// One worker thread; leaving a spare core for the UI. Speech decode is not
/// embarrassingly parallel enough to warrant more, and more threads inflate
/// onnxruntime memory.
fn default_threads() -> i32 {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (n.saturating_sub(1)).clamp(1, 4) as i32
}

/// Peak amplitude below the int16-equivalent 300 counts as silence — skip the
/// decode (whisper hallucinates text on pure silence). 300/32768 ≈ 0.00916.
const SILENCE_PEAK: f32 = 300.0 / 32768.0;

/// True when `samples` never rises above the silence floor, i.e. nothing worth
/// decoding was captured. An empty buffer is silent.
pub fn is_silent(samples: &[f32]) -> bool {
    samples.iter().fold(0.0_f32, |m, s| m.max(s.abs())) < SILENCE_PEAK
}

/// Max samples per decode chunk (30 s at the target rate).
const CHUNK_SAMPLES: usize = 30 * TARGET_SAMPLE_RATE as usize;
/// How far back from a chunk edge to hunt for a quiet split (5 s).
const SEARCH_SAMPLES: usize = 5 * TARGET_SAMPLE_RATE as usize;
/// Quiet-window size (~100 ms).
const WINDOW_SAMPLES: usize = TARGET_SAMPLE_RATE as usize / 10;

/// Compute chunk cut points over a 16 kHz buffer: `[0, .., len]`. Short buffers
/// return `[0, len]` (single decode). Long buffers are cut roughly every 30 s,
/// each cut nudged to the quietest ~100 ms window in the preceding 5 s so a
/// split rarely lands mid-word.
pub fn chunk_boundaries(samples: &[f32], _sample_rate: u32) -> Vec<usize> {
    let len = samples.len();
    if len <= CHUNK_SAMPLES {
        return vec![0, len];
    }
    let mut bounds = vec![0usize];
    let mut cursor = 0usize;
    while len - cursor > CHUNK_SAMPLES {
        let target = cursor + CHUNK_SAMPLES;
        let search_start = target.saturating_sub(SEARCH_SAMPLES).max(cursor + WINDOW_SAMPLES);
        let cut = quietest_window_center(samples, search_start, target).unwrap_or(target);
        // Guarantee forward progress even if the quiet search degenerates.
        let cut = cut.max(cursor + WINDOW_SAMPLES).min(len);
        bounds.push(cut);
        cursor = cut;
    }
    bounds.push(len);
    bounds
}

/// Index of the center of the lowest-energy `WINDOW_SAMPLES` window whose start
/// lies in `[from, to)`. `None` if the range is too small to hold a window.
fn quietest_window_center(samples: &[f32], from: usize, to: usize) -> Option<usize> {
    let to = to.min(samples.len());
    if to < from + WINDOW_SAMPLES {
        return None;
    }
    // Sliding sum of squares over WINDOW_SAMPLES; step by 1/10 window for speed.
    let step = (WINDOW_SAMPLES / 10).max(1);
    let mut best_start = from;
    let mut best_energy = f32::MAX;
    let mut start = from;
    while start + WINDOW_SAMPLES <= to {
        let energy: f32 = samples[start..start + WINDOW_SAMPLES]
            .iter()
            .map(|s| s * s)
            .sum();
        if energy < best_energy {
            best_energy = energy;
            best_start = start;
        }
        start += step;
    }
    Some(best_start + WINDOW_SAMPLES / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_gate_flags_quiet_and_passes_loud() {
        assert!(is_silent(&[]));
        assert!(is_silent(&vec![0.0001_f32; 1000]));
        let mut loud = vec![0.0_f32; 1000];
        loud[500] = 0.5;
        assert!(!is_silent(&loud));
    }

    #[test]
    fn short_buffer_is_a_single_chunk() {
        let short = vec![0.1_f32; CHUNK_SAMPLES]; // exactly 30 s → still single
        assert_eq!(chunk_boundaries(&short, TARGET_SAMPLE_RATE), vec![0, short.len()]);
    }

    #[test]
    fn long_buffer_splits_at_injected_quiet_window() {
        // 40 s of loud tone with a silent 100 ms window planted ~2 s before the
        // 30 s edge; the first cut should land inside that window.
        let total = 40 * TARGET_SAMPLE_RATE as usize;
        let mut buf = vec![0.4_f32; total];
        // Quiet window centered at 28 s (2 s before the 30 s target edge).
        let quiet_center = 28 * TARGET_SAMPLE_RATE as usize;
        let qs = quiet_center - WINDOW_SAMPLES / 2;
        for s in buf.iter_mut().skip(qs).take(WINDOW_SAMPLES) {
            *s = 0.0;
        }
        let bounds = chunk_boundaries(&buf, TARGET_SAMPLE_RATE);
        assert_eq!(bounds.first(), Some(&0));
        assert_eq!(bounds.last(), Some(&total));
        let first_cut = bounds[1];
        // Cut should sit within the planted quiet window (± a step).
        let step = (WINDOW_SAMPLES / 10).max(1);
        assert!(
            (first_cut as i64 - quiet_center as i64).abs() <= (WINDOW_SAMPLES + step) as i64,
            "cut {first_cut} not near quiet center {quiet_center}"
        );
    }

    #[test]
    fn boundaries_are_monotonic_and_cover_the_buffer() {
        let total = 95 * TARGET_SAMPLE_RATE as usize; // ~95 s → 4 chunks
        let buf = vec![0.3_f32; total];
        let bounds = chunk_boundaries(&buf, TARGET_SAMPLE_RATE);
        assert!(bounds.windows(2).all(|w| w[1] > w[0]), "monotonic");
        assert_eq!(bounds[0], 0);
        assert_eq!(*bounds.last().unwrap(), total);
        // Every chunk is ≤ 30 s.
        assert!(bounds.windows(2).all(|w| w[1] - w[0] <= CHUNK_SAMPLES));
    }
}
