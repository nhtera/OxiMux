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
    /// Transducer models only.
    pub joiner: Option<&'static str>,
    pub tokens: &'static str,
}

impl ModelSpec {
    /// The files that must exist after extraction for the model to be Ready.
    pub fn required_files(&self) -> Vec<&'static str> {
        let mut v = vec![self.encoder, self.decoder, self.tokens];
        if let Some(j) = self.joiner {
            v.push(j);
        }
        v
    }
}

/// The default model id — Vietnamese priority (whisper-small covers `vi`).
pub const DEFAULT_MODEL_ID: &str = "whisper-small";

/// The full catalog, in display order (default first).
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
    },
    // Best English quality (both reference apps default to this). 465 MB. No vi.
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3-int8",
        label: "Parakeet TDT 0.6B v3",
        family: Family::Transducer,
        langs: "25 European languages · best English",
        size_mb: 465,
        archive_url:
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        archive_sha256: None,
        encoder: "encoder.int8.onnx",
        decoder: "decoder.int8.onnx",
        joiner: Some("joiner.int8.onnx"),
        tokens: "tokens.txt",
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
    fn transducer_has_joiner_whisper_does_not() {
        for m in catalog() {
            match m.family {
                Family::Whisper => assert!(m.joiner.is_none(), "{} whisper has no joiner", m.id),
                Family::Transducer => assert!(m.joiner.is_some(), "{} needs joiner", m.id),
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
