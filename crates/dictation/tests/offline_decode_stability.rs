//! End-to-end offline-decode stability check across many languages.
//!
//! This is an on-demand smoke test (marked `#[ignore]`) because it needs the
//! real downloaded Whisper model (hundreds of MB) and speech fixtures — neither
//! belongs in the default `cargo test` run. It drives the SAME pipeline the app
//! uses (resample → optional Silero VAD trim → silence gate → chunked decode),
//! so a green run means the production dictation path is sound end to end.
//!
//! Run it with:
//! ```text
//! OXIMUX_STT_MODEL_DIR="$HOME/Library/Application Support/dev.nhtera.oximux/speech-models" \
//! OXIMUX_STT_FIXTURES=/path/to/stt_fixtures \
//! cargo test -p oximux-dictation --test offline_decode_stability -- --ignored --nocapture
//! ```
//!
//! Model dir defaults to the macOS app-support location; fixtures dir has no
//! default (the test skips with a notice if unset/absent).
//!
//! What "stable" means here, asserted per fixture:
//! - **no panic / no process abort** across the whole matrix,
//! - **determinism**: decoding the same buffer twice yields identical text,
//! - **speech → non-empty** transcript (auto-detect handles every language),
//! - **silence → empty** transcript (the peak gate suppresses hallucination),
//! - **VAD-safe**: trimming never turns real speech into an empty transcript.

use std::path::{Path, PathBuf};

use oximux_dictation::engine::{is_silent, Engine, EngineKind, ModelPaths};
use oximux_dictation::resample;

/// A speech fixture: `<stem>.wav` plus the language and a lowercased substring
/// we expect to appear in the transcript (soft-checked — printed, not asserted,
/// since synthesized-voice ASR is imperfect).
struct Fixture {
    stem: &'static str,
    lang: &'static str,
    /// Lowercased keyword(s); any one appearing counts as a content hit.
    expect_any: &'static [&'static str],
}

const FIXTURES: &[Fixture] = &[
    Fixture { stem: "en", lang: "English",    expect_any: &["fox", "quick", "lazy", "dog"] },
    Fixture { stem: "vi", lang: "Vietnamese", expect_any: &["giọng nói", "tiếng việt", "nhận dạng"] },
    Fixture { stem: "fr", lang: "French",     expect_any: &["reconnaissance", "vocale", "français"] },
    Fixture { stem: "es", lang: "Spanish",    expect_any: &["reconocimiento", "voz", "español"] },
    Fixture { stem: "de", lang: "German",     expect_any: &["spracherkennung", "deutsch", "teste"] },
    Fixture { stem: "zh", lang: "Chinese",    expect_any: &["语音", "识别", "测试"] },
    Fixture { stem: "ja", lang: "Japanese",   expect_any: &["音声", "認識", "日本語"] },
];

fn model_dir() -> PathBuf {
    if let Ok(p) = std::env::var("OXIMUX_STT_MODEL_DIR") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/dev.nhtera.oximux/speech-models")
}

fn fixtures_dir() -> Option<PathBuf> {
    std::env::var("OXIMUX_STT_FIXTURES").ok().map(PathBuf::from)
}

/// Build ModelPaths for the whisper-base model (int8 graphs) if present.
fn whisper_base_paths(model_root: &Path, language: Option<String>) -> Option<ModelPaths> {
    let dir = model_root.join("whisper-base");
    let encoder = dir.join("base-encoder.int8.onnx");
    let decoder = dir.join("base-decoder.int8.onnx");
    let tokens = dir.join("base-tokens.txt");
    if !encoder.exists() || !decoder.exists() || !tokens.exists() {
        return None;
    }
    Some(ModelPaths {
        id: "whisper-base".to_string(),
        dir,
        kind: EngineKind::Whisper { language },
        encoder,
        decoder,
        joiner: None,
        tokens,
        model: None,
    })
}

/// Read a WAV (i16 or f32, any rate) into 16 kHz mono f32 — the same shape the
/// capture path hands the engine. Mirrors resample::downmix + resample_linear.
fn load_wav_16k_mono(path: &Path) -> (Vec<f32>, u32) {
    let mut reader = hound::WavReader::open(path).expect("open wav fixture");
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 / max)
                .collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    let mono = resample::downmix_to_mono(&raw, spec.channels);
    let out = resample::resample_linear(&mono, spec.sample_rate, resample::TARGET_SAMPLE_RATE);
    (out, spec.sample_rate)
}

/// Trim silence with a freshly-built Silero VAD (matches the production
/// single-use lifecycle), with the same safety net the controller uses.
fn vad_trim(model_root: &Path, samples: Vec<f32>) -> Vec<f32> {
    use oximux_dictation::vad::{self, Vad};
    let path = match vad::ensure_downloaded(model_root) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  [vad] model unavailable ({e}); skipping trim");
            return samples;
        }
    };
    let mut vad = match Vad::load(&path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("  [vad] load failed ({e}); skipping trim");
            return samples;
        }
    };
    let trimmed = vad.keep_speech(&samples);
    if trimmed.is_empty() && !is_silent(&samples) {
        eprintln!("  [vad] found no speech in non-silent audio; using untrimmed");
        return samples;
    }
    trimmed
}

#[test]
#[ignore = "needs downloaded whisper model + speech fixtures; run manually with --ignored"]
fn offline_decode_is_stable_across_languages() {
    let model_root = model_dir();
    let Some(fx_dir) = fixtures_dir() else {
        eprintln!(
            "SKIP: set OXIMUX_STT_FIXTURES to the speech-fixtures dir to run this test.\n\
             (generate with `say -v <voice> -o <lang>.wav --data-format=LEI16@22050 \"...\"`)"
        );
        return;
    };
    let Some(paths) = whisper_base_paths(&model_root, None) else {
        eprintln!(
            "SKIP: whisper-base model not found under {}. Download it in the app first.",
            model_root.display()
        );
        return;
    };

    println!("\n=== STT stability matrix (model: whisper-base, auto-detect) ===");
    println!("model root: {}", model_root.display());
    println!("fixtures:   {}\n", fx_dir.display());

    let mut engine = Engine::load(&paths).expect("load whisper-base engine");

    let mut failures: Vec<String> = Vec::new();
    let mut content_hits = 0usize;

    for fx in FIXTURES {
        let wav = fx_dir.join(format!("{}.wav", fx.stem));
        if !wav.exists() {
            eprintln!("[{}] MISSING fixture {}", fx.lang, wav.display());
            continue;
        }
        let (samples16k, src_rate) = load_wav_16k_mono(&wav);
        let dur = samples16k.len() as f32 / resample::TARGET_SAMPLE_RATE as f32;

        // Full production path: VAD trim → silence gate → chunked decode.
        let trimmed = vad_trim(&model_root, samples16k.clone());
        let kept = trimmed.len() as f32 / resample::TARGET_SAMPLE_RATE as f32;

        // Determinism: decode the SAME trimmed buffer twice.
        let t0 = std::time::Instant::now();
        let text1 = engine.decode_recording(&trimmed);
        let decode_ms = t0.elapsed().as_millis();
        let text2 = engine.decode_recording(&trimmed);

        // Also decode the untrimmed buffer, to confirm VAD didn't corrupt speech.
        let text_raw = engine.decode_recording(&samples16k);

        let low = text1.to_lowercase();
        let hit = fx.expect_any.iter().any(|k| low.contains(k));
        if hit {
            content_hits += 1;
        }

        println!(
            "[{lang}] {src}Hz→16k  {dur:.2}s→{kept:.2}s (vad)  decode {decode_ms}ms  content:{content}",
            lang = fx.lang,
            src = src_rate,
            content = if hit { "HIT" } else { "miss" },
        );
        println!("    trimmed : {text1:?}");
        if text_raw.trim() != text1.trim() {
            println!("    untrim'd: {text_raw:?}");
        }

        // Hard stability assertions.
        if text1 != text2 {
            failures.push(format!(
                "[{}] NON-DETERMINISTIC: {text1:?} != {text2:?}",
                fx.lang
            ));
        }
        if text1.trim().is_empty() {
            failures.push(format!("[{}] EMPTY transcript on speech audio", fx.lang));
        }
        if text_raw.trim().is_empty() {
            failures.push(format!(
                "[{}] EMPTY transcript on UNTRIMMED speech audio",
                fx.lang
            ));
        }
    }

    // Silence fixture → the peak gate must yield an empty decode (no hallucination).
    let silence = fx_dir.join("silence.wav");
    if silence.exists() {
        let (s16, _) = load_wav_16k_mono(&silence);
        let silent = is_silent(&s16);
        let text = if silent { String::new() } else { engine.decode_recording(&s16) };
        println!("\n[silence] is_silent={silent}  transcript:{text:?}");
        if !silent && !text.trim().is_empty() {
            failures.push(format!("[silence] produced hallucinated text: {text:?}"));
        }
    }

    // Silence-trim proof: a clip padded with ~5 s of dead air must (a) get that
    // silence dropped by VAD, yet (b) keep every spoken word (padding intact).
    let padded = fx_dir.join("padded_en.wav");
    if padded.exists() {
        let (s16, _) = load_wav_16k_mono(&padded);
        let full = s16.len() as f32 / resample::TARGET_SAMPLE_RATE as f32;
        let trimmed = vad_trim(&model_root, s16);
        let kept = trimmed.len() as f32 / resample::TARGET_SAMPLE_RATE as f32;
        let text = engine.decode_recording(&trimmed).to_lowercase();
        println!("\n[padded_en] {full:.2}s→{kept:.2}s (vad)  transcript:{text:?}");
        // Expect a real cut (dead air gone) but well short of removing speech.
        if kept >= full - 2.0 {
            failures.push(format!(
                "[padded_en] VAD did not trim silence: {full:.2}s→{kept:.2}s"
            ));
        }
        for word in ["quick", "brown", "fox", "lazy", "dog"] {
            if !text.contains(word) {
                failures.push(format!("[padded_en] VAD clipped word '{word}': {text:?}"));
            }
        }
    }

    // Pinned-language path (the Vietnamese-priority default): reload with language
    // forced to `vi` and confirm the Vietnamese fixture still decodes non-empty.
    let vi_wav = fx_dir.join("vi.wav");
    if let (true, Some(vi_paths)) = (
        vi_wav.exists(),
        whisper_base_paths(&model_root, Some("vi".to_string())),
    ) {
        let mut vi_engine = Engine::load(&vi_paths).expect("load whisper-base pinned-vi");
        let (s16, _) = load_wav_16k_mono(&vi_wav);
        let trimmed = vad_trim(&model_root, s16);
        let text = vi_engine.decode_recording(&trimmed);
        println!("\n[vi pinned] transcript: {text:?}");
        if text.trim().is_empty() {
            failures.push("[vi pinned] EMPTY transcript with language pinned to vi".into());
        }
    }

    println!(
        "\n=== summary: {}/{} content hits, {} hard failures ===\n",
        content_hits,
        FIXTURES.len(),
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "STT stability failures:\n{}",
        failures.join("\n")
    );
}
