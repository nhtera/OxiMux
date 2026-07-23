//! The speech-to-text seam: turning a recorded clip into text with the
//! desktop's own dictation engine, expressed without depending on it.
//!
//! Like [`SessionLauncher`](crate::SessionLauncher) and
//! [`RewindService`](crate::RewindService), this exists because the work is not
//! something `SessionRegistry` can do — it loads an ONNX graph and runs a CPU
//! decode — and `agent-core` was deliberately kept free of that. So the
//! dispatcher talks to this trait and the app supplies the implementation,
//! routing a remote dictation through the exact engine a local one uses.
//!
//! **Why the desktop's engine is reused rather than re-derived on the phone.**
//! Same reason the chat fold, diff rendering, and forge calls live desktop-side:
//! the decode investment (model catalog, resample, silence gating, filler
//! filtering) already exists there, and a second implementation on the phone
//! would double the app's size for offline capability nothing else in the
//! remote-control surface needs. The clip crosses the wire; the text comes back.

/// Why a transcription could not happen.
///
/// Curated to carry no host paths, like [`RewindError`](crate::RewindError):
/// the underlying failures name model files under the user's data dir, which the
/// wire must never leak.
#[derive(Debug, thiserror::Error)]
pub enum TranscribeError {
    /// The clip could not be read as audio — not a WAV, not PCM16, or empty.
    ///
    /// The client's fault (a malformed payload), so the dispatcher maps it to a
    /// `BadRequest` rather than an `Internal`: retrying the same bytes will not
    /// help, but recording again will.
    #[error("the recording could not be read as audio")]
    BadAudio,
    /// No dictation model is downloaded and ready on the desktop.
    ///
    /// A host-configuration state the user fixes in the desktop's Voice
    /// settings, not something the phone can resolve — surfaced so the phone can
    /// say so instead of reporting a bare failure.
    #[error("the desktop has no dictation model ready")]
    NoModel,
    /// The engine failed to load or decode. Detail is logged host-side.
    #[error("the transcription did not complete")]
    Failed,
}

/// Decoding one recorded clip to text with the desktop's speech engine.
///
/// **Mutates nothing** — no session, no files, no persisted audio. The clip is
/// decoded and dropped; the transcript is the only output. Gated on the
/// authenticated-connection requirement alone (any paired device may dictate),
/// not on write scope, because there is no state for a narrowed device to reach.
#[async_trait::async_trait]
pub trait AudioTranscriber: Send + Sync {
    /// Decode `wav` (a WAV clip; 16 kHz mono PCM16 is the phone's contract, but
    /// the implementation reads the real rate from the header and resamples when
    /// it differs) to text. `sample_rate` is the phone's declared capture rate,
    /// a fallback used only for a headerless raw-PCM payload.
    ///
    /// An empty transcript is `Ok("")`, not an error: a silent clip, or one that
    /// held only filler the engine dropped, decoded fine — it simply has no
    /// words. The heavy ONNX decode should run off the async runtime (the
    /// implementation is expected to `spawn_blocking`) so a long clip does not
    /// stall the connection.
    async fn transcribe(&self, wav: &[u8], sample_rate: u32) -> Result<String, TranscribeError>;
}
