//! Static catalog of the dictation models we offer.
//!
//! All archives come from the k2-fsa sherpa-onnx `asr-models` release, extract
//! with `--strip-components=1` so the inner files land directly in the model
//! dir, and run at 16 kHz. The URLs + byte sizes were HEAD-verified against the
//! live release on 2026-07-16.
//!
//! ⚠️ `archive_sha256` is `None` for every entry: GitHub does not publish a
//! checksum for release assets, and fabricating one would break real downloads.
//! The manager verifies only when a hash is present and otherwise relies on the
//! post-extract file-existence gate. Pin real hashes here once a model has been
//! downloaded and hashed on a trusted machine.

/// Recognizer family for a catalog entry. Kept as a small flag rather than the
/// runtime [`crate::EngineKind`] because the whisper language is chosen at
/// session time from settings, not baked into the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Whisper,
    Transducer,
    /// Icefall zipformer offline transducer (e.g. dedicated Vietnamese models).
    /// Same encoder/decoder/joiner/tokens layout as `Transducer`.
    Zipformer,
    /// SenseVoice — a single-file multilingual model (`model` + `tokens`).
    SenseVoice,
}

/// One downloadable model. File names are relative to the extracted model dir
/// (post `--strip-components=1`).
#[derive(Debug, Clone)]
pub struct ModelSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub family: Family,
    /// Human blurb about language coverage, shown in the Voice pane.
    pub langs: &'static str,
    /// Download size in MB (rounded), for the pane's "Download (NN MB)" label.
    pub size_mb: u32,
    pub archive_url: &'static str,
    /// SHA-256 of the archive, verified when present. `None` = unpinned.
    pub archive_sha256: Option<&'static str>,
    pub encoder: &'static str,
    pub decoder: &'static str,
    /// Transducer / zipformer models only.
    pub joiner: Option<&'static str>,
    pub tokens: &'static str,
    /// Single-file model families (SenseVoice). `None` for the encoder/decoder
    /// families, which use `encoder`/`decoder`(/`joiner`).
    pub model: Option<&'static str>,
}

impl ModelSpec {
    /// The files that must exist after extraction for the model to be Ready.
    pub fn required_files(&self) -> Vec<&'static str> {
        match self.family {
            // Single-file: only the model graph + tokens.
            Family::SenseVoice => {
                let mut v = vec![self.tokens];
                if let Some(m) = self.model {
                    v.push(m);
                }
                v
            }
            _ => {
                let mut v = vec![self.encoder, self.decoder, self.tokens];
                if let Some(j) = self.joiner {
                    v.push(j);
                }
                v
            }
        }
    }
}

/// The default model id — Vietnamese priority (whisper-small covers `vi`).
pub const DEFAULT_MODEL_ID: &str = "whisper-small";

/// The full catalog, in display order (default first). URLs + sizes are
/// HEAD/range-verified against the live k2-fsa `asr-models` release; in-archive
/// file names were read from the actual `tar.bz2` listings, not guessed.
pub const CATALOG: &[ModelSpec] = &[
    // Default: multilingual incl. Vietnamese. 610 MB (only the fp32 archive is
    // published; the `.int8` archive 404s). Uses the int8 graphs bundled inside.
    ModelSpec {
        id: "whisper-small",
        label: "Whisper Small",
        family: Family::Whisper,
        langs: "Multilingual · Vietnamese",
        size_mb: 610,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-small.tar.bz2",
        archive_sha256: None,
        encoder: "small-encoder.int8.onnx",
        decoder: "small-decoder.int8.onnx",
        joiner: None,
        tokens: "small-tokens.txt",
        model: None,
    },
    // Dedicated Vietnamese zipformer — ~1/10th the size of whisper-small, better
    // vi accuracy, but Vietnamese-only (no code-switching). 57 MB.
    ModelSpec {
        id: "zipformer-vi",
        label: "Zipformer Vietnamese",
        family: Family::Zipformer,
        langs: "Vietnamese · dedicated",
        size_mb: 57,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-zipformer-vi-int8-2025-04-20.tar.bz2",
        archive_sha256: None,
        encoder: "encoder-epoch-12-avg-8.int8.onnx",
        decoder: "decoder-epoch-12-avg-8.onnx",
        joiner: Some("joiner-epoch-12-avg-8.int8.onnx"),
        tokens: "tokens.txt",
        model: None,
    },
    // Fills the tiny↔small gap; same Vietnamese coverage as small. 198 MB.
    ModelSpec {
        id: "whisper-base",
        label: "Whisper Base",
        family: Family::Whisper,
        langs: "Multilingual · Vietnamese",
        size_mb: 198,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2",
        archive_sha256: None,
        encoder: "base-encoder.int8.onnx",
        decoder: "base-decoder.int8.onnx",
        joiner: None,
        tokens: "base-tokens.txt",
        model: None,
    },
    // Fast starter: multilingual but weak Vietnamese. 111 MB.
    ModelSpec {
        id: "whisper-tiny",
        label: "Whisper Tiny",
        family: Family::Whisper,
        langs: "Multilingual · fast, weak Vietnamese",
        size_mb: 111,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-tiny.tar.bz2",
        archive_sha256: None,
        encoder: "tiny-encoder.int8.onnx",
        decoder: "tiny-decoder.int8.onnx",
        joiner: None,
        tokens: "tiny-tokens.txt",
        model: None,
    },
    // 25 European languages incl. best-in-class multilingual. 465 MB. No vi.
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3-int8",
        label: "Parakeet TDT 0.6B v3",
        family: Family::Transducer,
        langs: "25 European languages · multilingual",
        size_mb: 465,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        archive_sha256: None,
        encoder: "encoder.int8.onnx",
        decoder: "decoder.int8.onnx",
        joiner: Some("joiner.int8.onnx"),
        tokens: "tokens.txt",
        model: None,
    },
    // English-only, topped the OpenASR leaderboard — best English WER. 460 MB.
    ModelSpec {
        id: "parakeet-tdt-0.6b-v2-int8",
        label: "Parakeet TDT 0.6B v2",
        family: Family::Transducer,
        langs: "English only · best English WER",
        size_mb: 460,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        archive_sha256: None,
        encoder: "encoder.int8.onnx",
        decoder: "decoder.int8.onnx",
        joiner: Some("joiner.int8.onnx"),
        tokens: "tokens.txt",
        model: None,
    },
    // SenseVoice — single-file multilingual (zh/en/ja/ko/yue), fast. 158 MB. No vi.
    ModelSpec {
        id: "sense-voice-zh-en-ja-ko-yue-int8",
        label: "SenseVoice Small",
        family: Family::SenseVoice,
        langs: "Chinese · English · Japanese · Korean · Cantonese",
        size_mb: 158,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2",
        archive_sha256: None,
        encoder: "",
        decoder: "",
        joiner: None,
        tokens: "tokens.txt",
        model: Some("model.int8.onnx"),
    },
    // Lighter Vietnamese zipformer for low-resource machines. 25 MB.
    ModelSpec {
        id: "zipformer-vi-30m",
        label: "Zipformer Vietnamese 30M",
        family: Family::Zipformer,
        langs: "Vietnamese · lite",
        size_mb: 25,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-zipformer-vi-30M-int8-2026-02-09.tar.bz2",
        archive_sha256: None,
        encoder: "encoder.int8.onnx",
        decoder: "decoder.onnx",
        joiner: Some("joiner.int8.onnx"),
        tokens: "tokens.txt",
        model: None,
    },
];

/// The whole catalog.
pub fn catalog() -> &'static [ModelSpec] {
    CATALOG
}

/// The spec for `id`, if it exists.
pub fn spec_for(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_in_catalog_and_whisper() {
        let d = spec_for(DEFAULT_MODEL_ID).expect("default model in catalog");
        assert_eq!(d.family, Family::Whisper, "default supports Vietnamese");
    }

    #[test]
    fn family_file_shape_is_consistent() {
        for m in catalog() {
            match m.family {
                Family::Whisper => assert!(m.joiner.is_none(), "{} whisper has no joiner", m.id),
                // Both transducer flavors need encoder/decoder/joiner.
                Family::Transducer | Family::Zipformer => {
                    assert!(m.joiner.is_some(), "{} needs a joiner", m.id);
                    assert!(m.model.is_none(), "{} is not single-file", m.id);
                }
                // Single-file: a `model`, no joiner.
                Family::SenseVoice => {
                    assert!(m.model.is_some(), "{} needs a model file", m.id);
                    assert!(m.joiner.is_none(), "{} sense-voice has no joiner", m.id);
                    assert!(m.required_files().contains(&m.tokens), "{} needs tokens", m.id);
                }
            }
        }
    }

    #[test]
    fn required_files_include_joiner_only_for_transducer() {
        let whisper = spec_for("whisper-small").unwrap();
        assert_eq!(whisper.required_files().len(), 3);
        let para = spec_for("parakeet-tdt-0.6b-v3-int8").unwrap();
        assert_eq!(para.required_files().len(), 4);
    }

    #[test]
    fn all_urls_are_from_k2fsa_asr_models() {
        for m in catalog() {
            assert!(
                m.archive_url.starts_with(
                    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/"
                ),
                "{} url off-source",
                m.id
            );
            assert!(m.archive_url.ends_with(".tar.bz2"));
        }
    }
}
