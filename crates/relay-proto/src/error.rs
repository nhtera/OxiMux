use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    // u64 (not u32) so a hostile payload length larger than 4 GiB is
    // reported truthfully. The wire header itself is u32, but the
    // encode-side check operates on `payload.len(): usize` which can
    // exceed u32 on 64-bit hosts.
    #[error("frame too large: {0} bytes (max {MAX})", MAX = crate::frame::MAX_FRAME_SIZE)]
    FrameTooLarge(u64),
    #[error("invalid frame kind: 0x{0:02x}")]
    InvalidFrameKind(u8),
    #[error("bincode encode failed: {0}")]
    Encode(String),
    #[error("bincode decode failed: {0}")]
    Decode(String),
    #[error("trailing bytes after frame payload: {0} bytes left")]
    TrailingBytes(usize),
}

// Application-level errors carried by `Response::Err`. Distinct from
// `ProtoError` (codec faults) — these reflect daemon decisions that the
// client must interpret semantically (e.g. fall back to fresh spawn on
// PtyNotFound, abort + warn on VersionMismatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrCode {
    Unknown,
    PtyNotFound,
    AuthFailed,
    VersionMismatch,
    RateLimited,
    Internal,
}
