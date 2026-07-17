//! oximux-dictation
//!
//! Local, offline voice dictation: cpal microphone capture → linear resample to
//! 16 kHz → sherpa-onnx offline decode (Whisper for Vietnamese, Parakeet for
//! English). The crate exposes a channel-based [`DictationController`] handle
//! (mirroring the `oximux-pty` convention) plus a model catalog + download
//! manager. It has **no GPUI dependency** — the app drains [`DictationEvent`]s
//! and drives the mic button; every cpal/sherpa type stays inside here.
//!
//! Threading: one worker thread owns the warm engine + session state machine; a
//! capture thread owns the cpal stream. Neither blocks the GPUI main thread, and
//! `Drop` signals rather than joins (see [`controller`]).

pub mod capture;
pub mod controller;
pub mod custom_words;
pub mod engine;
pub mod feedback;
pub mod model_catalog;
pub mod model_manager;
pub mod resample;
pub mod text_filter;
pub mod vad;

pub use capture::{has_input_device, list_input_devices};
pub use controller::{DictationController, DictationEvent, MAX_RECORDING_SECS};
pub use engine::{Engine, EngineKind, ModelPaths};
pub use model_catalog::{
    Family, ModelSpec, catalog, recommended_model_id, spec_for, DEFAULT_MODEL_ID,
};
pub use model_manager::{ModelEvent, ModelManager, ModelStatus, Readiness};
pub use resample::TARGET_SAMPLE_RATE;
