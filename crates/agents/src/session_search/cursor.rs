//! Opaque "load more" cursors: base64url JSON `{o, k}` — the offset of the
//! next page and a hash of the request it belongs to. A cursor from a
//! different request is rejected. The index may change between pages (an
//! agent is mid-conversation); the next page is computed against the index as
//! it is then, and the caller drops sessions it already shows.

use std::hash::{DefaultHasher, Hash, Hasher};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub o: usize,
    pub k: u64,
}

/// Unparseable, or made for a different request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidCursor;

impl Cursor {
    pub fn encode(&self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    /// Decode `raw` and check it belongs to the request with `key`.
    pub fn verify(raw: &str, key: u64) -> Result<Cursor, InvalidCursor> {
        let bytes = URL_SAFE_NO_PAD.decode(raw).map_err(|_| InvalidCursor)?;
        let c: Cursor = serde_json::from_slice(&bytes).map_err(|_| InvalidCursor)?;
        (c.k == key).then_some(c).ok_or(InvalidCursor)
    }
}

/// Stable within a process (cursors never outlive it).
pub fn request_key(parts: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_mismatches() {
        let c = Cursor { o: 20, k: 42 };
        let raw = c.encode();
        assert_eq!(Cursor::verify(&raw, 42), Ok(c));
        assert_eq!(Cursor::verify(&raw, 43), Err(InvalidCursor));
        assert_eq!(Cursor::verify("not base64!", 42), Err(InvalidCursor));
    }
}
