//! How a scrollback checkpoint's bytes are stored on disk.
//!
//! The daemon writes `scrollback.bin` gzip-compressed (level 1: fast, and
//! terminal output shrinks well over 5×); every reader sniffs the gzip magic
//! and takes anything else as raw, so checkpoints written before compression
//! still restore. One definition for both ends, so they cannot drift. Pure:
//! the callers read and write the files.

use std::io::{Read, Write};

/// The two bytes every gzip stream starts with.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// The bytes to store for raw scrollback `raw`.
pub fn encode(raw: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(raw.len() / 4), flate2::Compression::new(1));
    // Writing to a Vec cannot fail.
    let _ = enc.write_all(raw);
    enc.finish().unwrap_or_default()
}

/// The raw scrollback in stored bytes `stored`: gunzipped when compressed,
/// as-is otherwise. A corrupt compressed file yields nothing rather than
/// compressed bytes replayed as terminal output.
pub fn decode(stored: &[u8]) -> Vec<u8> {
    if !stored.starts_with(&GZIP_MAGIC) {
        return stored.to_vec();
    }
    let mut raw = Vec::with_capacity(stored.len() * 4);
    match flate2::read::GzDecoder::new(stored).read_to_end(&mut raw) {
        Ok(_) => raw,
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_shrinks_terminal_output() {
        let raw: Vec<u8> = (0..20_000).flat_map(|i| format!("[{i}] compiling crate ok\r\n").into_bytes()).collect();
        let stored = encode(&raw);
        assert!(stored.starts_with(&GZIP_MAGIC));
        assert!(stored.len() * 5 < raw.len(), "{} vs {}", stored.len(), raw.len());
        assert_eq!(decode(&stored), raw);
    }

    #[test]
    fn a_raw_checkpoint_from_before_compression_still_reads() {
        assert_eq!(decode(b"$ make test\r\nok\r\n"), b"$ make test\r\nok\r\n");
        assert_eq!(decode(b""), b"");
    }

    #[test]
    fn a_corrupt_compressed_checkpoint_reads_as_empty() {
        assert!(decode(&[0x1f, 0x8b, 0x08, 0x00, 0xde, 0xad]).is_empty());
    }
}
