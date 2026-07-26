//! Host-side voice transcription for the remote-control dispatcher.
//!
//! Implements [`AudioTranscriber`] by routing a clip the phone recorded through
//! the *same* speech pipeline the desktop composer drives — [`Engine`], the
//! resampler, the silence gate, the VAD trim, and the filler/custom-word
//! post-processing. It shares the composer's [`ModelManager`] (a model
//! downloaded in Settings › Voice is usable from the phone the moment it lands)
//! and reads `dictation.toml` fresh on every call, so switching the model or
//! language on the desktop takes effect on the next phone dictation with no
//! restart.
//!
//! The engine is not re-implemented here; only the two things the desktop's own
//! path gets from its capture thread — a decoded sample buffer and a warm
//! engine cache — are reconstructed. The heavy ONNX decode runs on
//! `spawn_blocking` so a long clip never stalls the connection's async task.
//!
//! No audio is retained past the decode call: the clip is parsed, decoded, and
//! dropped when this function returns. Do not add logging of the sample buffer
//! or the recording bytes — the desktop's own dictation keeps nothing either,
//! and this must match that.

use std::path::Path;
use std::sync::{Arc, Mutex};

use oximux_dictation::engine::is_silent;
use oximux_dictation::resample::{TARGET_SAMPLE_RATE, resample_linear};
use oximux_dictation::{Engine, ModelManager};
use oximux_remote_host::transcribe::{AudioTranscriber, TranscribeError};
use oximux_settings::DictationSettings;

/// The desktop's speech engine, exposed to the remote host.
///
/// Holds its own warm-engine cache (behind a `Mutex`, shared into the blocking
/// decode via `Arc`) distinct from the composer's — the composer's lives on its
/// own capture worker and is not reachable off that thread. Two warm engines is
/// the cost of decoupling the remote path from the UI thread; each is dropped
/// when its owner is.
pub struct HostTranscriber {
    manager: Arc<ModelManager>,
    /// Warm recognizer, rebuilt when the configured model changes. `Arc<Mutex>`
    /// so the blocking decode closure can own a clone.
    engine: Arc<Mutex<Option<Engine>>>,
    /// `<data>/dev.nhtera.oximux/speech-models` — its parent is the settings
    /// dir (`dictation.toml`), and the VAD model downloads alongside the speech
    /// models here.
    models_dir: std::path::PathBuf,
}

impl HostTranscriber {
    pub fn new(manager: Arc<ModelManager>, models_dir: std::path::PathBuf) -> Self {
        Self { manager, engine: Arc::new(Mutex::new(None)), models_dir }
    }
}

#[async_trait::async_trait]
impl AudioTranscriber for HostTranscriber {
    async fn transcribe(&self, wav: &[u8], sample_rate: u32) -> Result<String, TranscribeError> {
        let wav = wav.to_vec();
        let manager = Arc::clone(&self.manager);
        let engine = Arc::clone(&self.engine);
        let models_dir = self.models_dir.clone();
        // The ONNX decode is CPU-heavy and synchronous; keep it off the async
        // runtime so a 2-minute clip does not park a runtime worker.
        tokio::task::spawn_blocking(move || {
            decode(&manager, &engine, &models_dir, &wav, sample_rate)
        })
        .await
        // A join error is a panic in the decode — surfaced as a generic failure,
        // never leaking the panic message to the wire.
        .map_err(|_| TranscribeError::Failed)?
    }
}

/// The full host decode: bytes → text, run on the blocking pool.
fn decode(
    manager: &ModelManager,
    engine: &Mutex<Option<Engine>>,
    models_dir: &Path,
    wav: &[u8],
    fallback_rate: u32,
) -> Result<String, TranscribeError> {
    let (samples, rate) = decode_audio(wav, fallback_rate)?;
    // Match the engine's 16 kHz expectation; a clip already at the target rate
    // (the phone's contract) skips the resample entirely.
    let s16 = if rate == 0 || rate == TARGET_SAMPLE_RATE {
        samples
    } else {
        resample_linear(&samples, rate, TARGET_SAMPLE_RATE)
    };

    // Read the live settings so the phone honours a model/language the user
    // switched on the desktop, exactly as the composer does at record time.
    let settings = load_settings(models_dir);
    let paths = manager
        .resolve_paths(&settings.model_id, settings.language_param())
        .ok_or(TranscribeError::NoModel)?;

    // Trim to speech spans when the user has VAD on, mirroring the desktop path.
    // Best-effort: a missing/unloadable VAD model degrades to the untrimmed
    // buffer, and the silence gate below still guards a fully-silent clip.
    let s16 = if settings.vad_enabled { vad_trim(models_dir, s16) } else { s16 };
    if is_silent(&s16) {
        // A silent clip is a normal, empty result — the composer inserts nothing.
        return Ok(String::new());
    }

    let mut guard = engine.lock().unwrap();
    if guard.as_ref().map(Engine::model_id) != Some(paths.id.as_str()) {
        // Detail (a model-file path under the user's data dir) is dropped: the
        // wire error must not name host paths.
        let loaded = Engine::load(&paths).map_err(|e| {
            tracing::warn!(error = %e, "remote dictation: engine load failed");
            TranscribeError::Failed
        })?;
        *guard = Some(loaded);
    }
    let text = guard
        .as_mut()
        .expect("engine was just loaded")
        .decode_recording(&s16);
    Ok(post_process(&settings, text))
}

/// Parse a recorded clip into mono 16-bit-derived f32 samples plus its rate.
///
/// Accepts a WAV (`RIFF…WAVE`) — the phone's contract — and reads the true rate
/// and channel count from its header, downmixing to mono. Falls back to reading
/// a headerless payload as raw mono PCM16 at `fallback_rate`, so a client that
/// strips its own header still works. Anything else — a truncated file, a
/// non-PCM encoding, an odd byte count — is a `BadAudio` the client sees as a
/// `BadRequest`.
fn decode_audio(bytes: &[u8], fallback_rate: u32) -> Result<(Vec<f32>, u32), TranscribeError> {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return parse_wav(bytes);
    }
    // Headerless raw PCM16 mono.
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(TranscribeError::BadAudio);
    }
    let samples = pcm16_to_f32_mono(bytes, 1);
    Ok((samples, fallback_rate))
}

/// Minimal WAV reader: walk the RIFF chunks for `fmt ` (format) and `data`
/// (samples). Only uncompressed PCM16 is supported — the one encoding the phone
/// is asked to produce; anything else is `BadAudio`.
fn parse_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32), TranscribeError> {
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits: u16 = 0;
    let mut data: Option<&[u8]> = None;

    // Chunks begin after the 12-byte RIFF/WAVE header.
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
            as usize;
        let body_start = pos + 8;
        let body_end = body_start.checked_add(size).ok_or(TranscribeError::BadAudio)?;
        if body_end > bytes.len() {
            return Err(TranscribeError::BadAudio);
        }
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err(TranscribeError::BadAudio);
                }
                let audio_format = u16::from_le_bytes([body[0], body[1]]);
                // 1 = PCM. WAVE_FORMAT_EXTENSIBLE (0xFFFE) can also wrap PCM, but
                // the phone emits plain PCM; reject the rest rather than guess.
                if audio_format != 1 {
                    return Err(TranscribeError::BadAudio);
                }
                channels = u16::from_le_bytes([body[2], body[3]]);
                sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                bits = u16::from_le_bytes([body[14], body[15]]);
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word-aligned: an odd body carries a pad byte.
        pos = body_end + (size & 1);
    }

    let data = data.ok_or(TranscribeError::BadAudio)?;
    if bits != 16 || channels == 0 || sample_rate == 0 {
        return Err(TranscribeError::BadAudio);
    }
    if !data.len().is_multiple_of(2) {
        return Err(TranscribeError::BadAudio);
    }
    Ok((pcm16_to_f32_mono(data, channels), sample_rate))
}

/// Interleaved little-endian PCM16 → mono f32 in `-1.0..=1.0`, averaging
/// channels. A trailing partial frame (bytes not a whole number of frames) is
/// dropped rather than misread.
fn pcm16_to_f32_mono(data: &[u8], channels: u16) -> Vec<f32> {
    let channels = channels.max(1) as usize;
    let frame_bytes = 2 * channels;
    let frames = data.len() / frame_bytes;
    let mut out = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * frame_bytes;
        let mut acc = 0.0f32;
        for c in 0..channels {
            let i = base + c * 2;
            let s = i16::from_le_bytes([data[i], data[i + 1]]);
            acc += s as f32 / 32768.0;
        }
        out.push(acc / channels as f32);
    }
    out
}

/// Read `dictation.toml` from the app data dir (the models dir's parent). Any
/// failure — no parent, unreadable, unparseable — degrades to defaults, matching
/// the app-side loader. Mirrors `app_settings::dictation_settings::load`, keyed
/// off the models dir this transcriber already holds.
fn load_settings(models_dir: &Path) -> DictationSettings {
    let Some(data_dir) = models_dir.parent() else {
        return DictationSettings::default();
    };
    let path = data_dir.join(DictationSettings::FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => DictationSettings::from_toml_str(&text)
            .map(DictationSettings::sanitized)
            .unwrap_or_default(),
        Err(_) => DictationSettings::default(),
    }
}

/// Trim `samples` to speech spans with a fresh Silero VAD, downloading the model
/// on first use. Best-effort: any failure returns the untrimmed buffer, and a
/// VAD miss on non-silent audio is treated as a miss (return the original) so a
/// whole utterance is never silently dropped.
fn vad_trim(models_dir: &Path, samples: Vec<f32>) -> Vec<f32> {
    let vad = oximux_dictation::vad::ensure_downloaded(models_dir)
        .and_then(|p| oximux_dictation::vad::Vad::load(&p));
    let mut vad = match vad {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "remote dictation: VAD unavailable; using untrimmed audio");
            return samples;
        }
    };
    let trimmed = vad.keep_speech(&samples);
    if trimmed.is_empty() && !is_silent(&samples) {
        return samples;
    }
    trimmed
}

/// Clean a transcript exactly as the desktop composer does: fix casing for the
/// uppercase-only models first, filter fillers/hallucinations, then correct
/// toward the user's custom-word dictionary.
fn post_process(settings: &DictationSettings, text: String) -> String {
    let text = match oximux_dictation::spec_for(&settings.model_id) {
        Some(spec) if spec.uppercase_output => oximux_dictation::text_filter::sentence_case(&text),
        _ => text,
    };
    let filtered = oximux_dictation::text_filter::filter(
        &text,
        &settings.language,
        settings.filler_filter_enabled,
    );
    oximux_dictation::custom_words::apply(
        &filtered,
        &settings.custom_words,
        settings.word_correction_threshold,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canonical 16 kHz mono PCM16 WAV round-trips to the sample count and rate
    /// the header declares.
    #[test]
    fn parses_a_mono_pcm16_wav() {
        let samples: Vec<i16> = vec![0, 1000, -1000, 32767, -32768];
        let wav = build_wav(16_000, 1, &samples);
        let (out, rate) = decode_audio(&wav, 0).expect("valid wav");
        assert_eq!(rate, 16_000);
        assert_eq!(out.len(), samples.len());
        assert!((out[3] - 32767.0 / 32768.0).abs() < 1e-6, "peak sample preserved");
    }

    /// A stereo clip is downmixed to mono by averaging the two channels.
    #[test]
    fn downmixes_stereo_to_mono() {
        // Two frames: L/R = (1000, -1000) then (2000, 0) → averages 0, 1000.
        let interleaved: Vec<i16> = vec![1000, -1000, 2000, 0];
        let wav = build_wav(16_000, 2, &interleaved);
        let (out, _) = decode_audio(&wav, 0).expect("valid wav");
        assert_eq!(out.len(), 2, "two frames collapse to two mono samples");
        assert!(out[0].abs() < 1e-6, "opposite channels cancel");
        assert!((out[1] - 1000.0 / 32768.0).abs() < 1e-6, "second frame averages to 1000");
    }

    /// A headerless even-length payload is read as raw mono PCM16 at the declared
    /// fallback rate.
    #[test]
    fn reads_headerless_raw_pcm_at_the_fallback_rate() {
        let raw: Vec<u8> = vec![0x00, 0x10, 0x00, 0xF0]; // two i16 samples
        let (out, rate) = decode_audio(&raw, 16_000).expect("raw pcm");
        assert_eq!(rate, 16_000);
        assert_eq!(out.len(), 2);
    }

    /// Garbage that is neither a WAV nor an even-length PCM buffer is rejected.
    #[test]
    fn rejects_a_malformed_clip() {
        assert!(matches!(decode_audio(&[1, 2, 3], 16_000), Err(TranscribeError::BadAudio)));
        assert!(matches!(decode_audio(&[], 16_000), Err(TranscribeError::BadAudio)));
    }

    /// A non-PCM WAV (compressed) is refused rather than misread as PCM.
    #[test]
    fn rejects_a_non_pcm_wav() {
        let samples: Vec<i16> = vec![1, 2, 3];
        let mut wav = build_wav(16_000, 1, &samples);
        // Flip the audio-format field (offset 20) from PCM (1) to something else.
        wav[20] = 3;
        assert!(matches!(decode_audio(&wav, 0), Err(TranscribeError::BadAudio)));
    }

    /// Build a minimal canonical PCM16 WAV for the tests.
    fn build_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let bits = 16u16;
        let block_align = channels * bits / 8;
        let byte_rate = sample_rate * block_align as u32;
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&sample_rate.to_le_bytes());
        w.extend_from_slice(&byte_rate.to_le_bytes());
        w.extend_from_slice(&block_align.to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }
}
