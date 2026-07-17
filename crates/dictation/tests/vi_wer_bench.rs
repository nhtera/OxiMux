//! Vietnamese WER benchmark for the CURRENT (sherpa) stack.
//!
//! On-demand (`#[ignore]`d): needs downloaded models + a real-speech eval set,
//! neither of which belongs in `cargo test`. Exists to give the phase-06 GPU/GGUF
//! evaluation an apples-to-apples baseline measured through the same pipeline the
//! app uses (resample → VAD trim → silence gate → chunked decode) and the same
//! WER math as the GGUF harness.
//!
//! ```text
//! OXIMUX_STT_MODEL_DIR=… OXIMUX_VI_EVAL=… \
//!   cargo test -p oximux-dictation --test vi_wer_bench -- --ignored --nocapture
//! ```
//! `OXIMUX_VI_EVAL` holds `manifest.json` ([{file,text}]) + `wav16/*.wav` at 16 kHz mono.

use std::path::{Path, PathBuf};

use oximux_dictation::engine::{Engine, EngineKind, ModelPaths};
use oximux_dictation::resample;

/// Vietnamese-safe normalization: lowercase, drop punctuation, collapse spaces.
/// Diacritics are MEANINGFUL in Vietnamese and are deliberately preserved.
fn norm(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn load_16k(path: &Path) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let spec = r.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().map(|s| s.unwrap() as f32 / max).collect()
        }
        hound::SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    let mono = resample::downmix_to_mono(&raw, spec.channels);
    resample::resample_linear(&mono, spec.sample_rate, resample::TARGET_SAMPLE_RATE)
}

fn model_root() -> PathBuf {
    std::env::var("OXIMUX_STT_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join("Library/Application Support/dev.nhtera.oximux/speech-models")
        })
}

/// Build ModelPaths for a catalog model by id, or None if not downloaded.
fn paths_for(root: &Path, id: &str, language: Option<String>) -> Option<ModelPaths> {
    let spec = oximux_dictation::spec_for(id)?;
    let dir = root.join(id);
    let kind = match spec.family {
        oximux_dictation::Family::Whisper => EngineKind::Whisper { language },
        oximux_dictation::Family::Zipformer => EngineKind::Zipformer,
        oximux_dictation::Family::Transducer => EngineKind::Transducer,
        oximux_dictation::Family::SenseVoice => EngineKind::SenseVoice,
    };
    let p = ModelPaths {
        id: id.to_string(),
        dir: dir.clone(),
        kind,
        encoder: dir.join(spec.encoder),
        decoder: dir.join(spec.decoder),
        joiner: spec.joiner.map(|j| dir.join(j)),
        tokens: dir.join(spec.tokens),
        model: spec.model.map(|m| dir.join(m)),
    };
    // Only bench what's actually on disk.
    spec.required_files()
        .iter()
        .all(|f| dir.join(f).exists())
        .then_some(p)
}

#[derive(serde::Deserialize)]
struct Item {
    file: String,
    text: String,
}

#[test]
#[ignore = "needs downloaded models + a real-speech eval set; run manually with --ignored"]
fn vietnamese_wer_of_current_stack() {
    let Ok(eval) = std::env::var("OXIMUX_VI_EVAL") else {
        eprintln!("SKIP: set OXIMUX_VI_EVAL to the eval-set dir (manifest.json + wav16/)");
        return;
    };
    let eval = PathBuf::from(eval);
    let items: Vec<Item> = serde_json::from_reader(
        std::fs::File::open(eval.join("manifest.json")).expect("manifest.json"),
    )
    .expect("parse manifest");
    let root = model_root();

    println!("\n=== Vietnamese WER — current (sherpa, CPU) ===");
    println!("eval: {} clips from {}\n", items.len(), eval.display());

    // Pin whisper to `vi`; zipformer-vi is Vietnamese-only and ignores language.
    for (id, lang) in [
        ("whisper-small", Some("vi".to_string())),
        ("whisper-base", Some("vi".to_string())),
        ("zipformer-vi", None),
    ] {
        let Some(p) = paths_for(&root, id, lang.clone()) else {
            println!("[{id}] not downloaded — skipped");
            continue;
        };
        let t = std::time::Instant::now();
        let mut engine = match Engine::load(&p) {
            Ok(e) => e,
            Err(e) => {
                println!("[{id}] load failed: {e}");
                continue;
            }
        };
        let load_ms = t.elapsed().as_millis();

        let (mut errs, mut words, mut audio_s, mut dec_ms) = (0usize, 0usize, 0f32, 0u128);
        for it in &items {
            let name = it.file.rsplit('/').next().unwrap();
            let pcm = load_16k(&eval.join("wav16").join(name));
            audio_s += pcm.len() as f32 / resample::TARGET_SAMPLE_RATE as f32;
            let t = std::time::Instant::now();
            let hyp = engine.decode_recording(&pcm);
            dec_ms += t.elapsed().as_millis();
            let (r, h) = (norm(&it.text), norm(&hyp));
            errs += edit_distance(&r, &h);
            words += r.len();
        }
        let wer = errs as f32 / words.max(1) as f32 * 100.0;
        println!(
            "[{id}] load {load_ms} ms | {audio_s:.1}s audio | decode {dec_ms} ms | x{:.1} realtime",
            audio_s * 1000.0 / dec_ms.max(1) as f32
        );
        println!("    WER = {wer:.2}%  ({errs} errors / {words} words)\n");
    }
}
