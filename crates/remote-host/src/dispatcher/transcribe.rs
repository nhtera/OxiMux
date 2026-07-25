//! The voice-transcription handler — decode one recorded clip to text.
//!
//! Unlike the schedule and session RPCs this names no session and changes no
//! state: it is a composer utility, so it gates on the authenticated connection
//! alone (checked in [`serve`](super::serve) before this is reached), not on
//! device tier or `is_allowed_for`. Any paired device may dictate, exactly as
//! any of them may type.
//!
//! Base64 decoding and the size guard live here (wire concerns); the audio
//! parsing, resample, and ONNX decode live behind the
//! [`AudioTranscriber`](crate::transcribe::AudioTranscriber) seam, in the app.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::transcribe::TranscribeError;

/// Hard cap on a decoded clip. The phone's contract is 16 kHz mono PCM16 capped
/// at 120 s (the desktop's own `MAX_RECORDING_SECS`) — 3.84 MB of samples plus a
/// WAV header. This ceiling leaves generous room for that plus a higher capture
/// rate, while staying well under the transport's 16 MiB frame limit, so an
/// oversized clip is refused here with a clear reason rather than failing as a
/// bare transport read error.
const MAX_AUDIO_BYTES: usize = 6 * 1024 * 1024;

impl Dispatcher {
    /// Decode a base64 WAV clip to text with the desktop's speech engine.
    ///
    /// Authorization (an authenticated, still-authorized device) is checked by
    /// the serve loop before this runs; there is no further tier gate because
    /// nothing is mutated.
    pub(super) async fn transcribe_audio(&self, audio_base64: &str, sample_rate: u32) -> Response {
        let Some(transcriber) = self.transcriber.as_ref() else {
            // Same answer a device lacking scope gets: whether this desktop can
            // transcribe is not something an unauthorized client should probe.
            return Response::Error(RpcError::Unauthorized);
        };
        let bytes = match STANDARD.decode(audio_base64) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Response::Error(RpcError::BadRequest("the audio was not valid base64".into()));
            }
        };
        if bytes.len() > MAX_AUDIO_BYTES {
            return Response::Error(RpcError::BadRequest("the recording is too long".into()));
        }
        match transcriber.transcribe(&bytes, sample_rate).await {
            Ok(text) => Response::Transcript(text),
            Err(e) => {
                // The engine's own errors name model files under the user's data
                // dir; `TranscribeError`'s messages are curated free of that, so
                // forwarding them is safe. The kind decides the code: a bad clip
                // is the client's to fix (retry helps nothing), the rest are host
                // state.
                tracing::warn!(error = %e, "remote transcription failed");
                match e {
                    TranscribeError::BadAudio => {
                        Response::Error(RpcError::BadRequest(e.to_string()))
                    }
                    TranscribeError::NoModel | TranscribeError::Failed => {
                        Response::Error(RpcError::Internal(e.to_string()))
                    }
                }
            }
        }
    }
}
