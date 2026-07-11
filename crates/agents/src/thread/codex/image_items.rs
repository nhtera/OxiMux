//! Codex image-item → inline [`ChatImage`] extraction.
//!
//! Codex's `imageView`/`imageGeneration` items carry a FILE PATH (`path` /
//! `savedPath`), unlike Claude, which inlines base64 on the wire. Reading an
//! agent-supplied path is security-sensitive: the path is canonicalized and
//! rejected unless it stays within the session `cwd`, so a manipulated session
//! can't point `imageView` at `~/.ssh/config` or another project's `.env` and
//! render its bytes into the chat. A rejected/unreadable/non-image path yields
//! `None` — a missing image is a cosmetic gap, never an error.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::super::entry::ChatImage;

/// Extract the inline image a completed `imageView`/`imageGeneration` item points
/// at, as a single-element `Vec<ChatImage>`. `None` when the item carries no
/// path, the path escapes `cwd`, the file is unreadable/empty, or the extension
/// isn't a known image type.
pub fn image_result(item: &Value, cwd: &Path) -> Option<Vec<ChatImage>> {
    let raw = item
        .get("path")
        .or_else(|| item.get("savedPath"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let path = contained_path(raw, cwd)?;
    let media_type = media_type_for(&path)?;
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(vec![ChatImage { media_type, data: STANDARD.encode(bytes) }])
}

/// Canonicalize `raw` (absolute, or relative to `cwd`) and return it ONLY if it
/// stays within the canonicalized `cwd`. Canonicalization also proves the file
/// exists. Rejects any path that escapes the session directory — the guard
/// against rendering arbitrary on-disk files an agent names.
fn contained_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let base = cwd.canonicalize().ok()?;
    let candidate = Path::new(raw);
    let abs = if candidate.is_absolute() { candidate.to_path_buf() } else { base.join(candidate) };
    let canonical = abs.canonicalize().ok()?;
    canonical.starts_with(&base).then_some(canonical)
}

/// Image MIME type from the file extension. `None` for a non-image or
/// extensionless path — those are not rendered.
fn media_type_for(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_contained_image_and_encodes_base64() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("shot.png");
        std::fs::write(&img, b"\x89PNG\r\n\x1a\nfakebytes").unwrap();
        let item = json!({"type":"imageView","id":"i","path": img.to_str().unwrap()});
        let out = image_result(&item, dir.path()).expect("image extracted");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].media_type, "image/png");
        assert_eq!(STANDARD.decode(&out[0].data).unwrap(), b"\x89PNG\r\n\x1a\nfakebytes");
    }

    #[test]
    fn rejects_path_escaping_cwd() {
        // A file OUTSIDE the session cwd must not be read (the containment guard).
        let cwd = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.png");
        std::fs::write(&secret, b"\x89PNGsecret").unwrap();
        // Absolute escape.
        let item = json!({"type":"imageView","id":"i","savedPath": secret.to_str().unwrap()});
        assert!(image_result(&item, cwd.path()).is_none(), "absolute escape rejected");
        // Relative traversal escape.
        let rel = json!({"type":"imageView","id":"i","path": "../secret.png"});
        assert!(image_result(&rel, cwd.path()).is_none(), "relative traversal rejected");
    }

    #[test]
    fn non_image_extension_and_missing_path_yield_none() {
        let dir = tempfile::tempdir().unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, b"plain").unwrap();
        assert!(image_result(&json!({"path": txt.to_str().unwrap()}), dir.path()).is_none());
        assert!(image_result(&json!({"type":"imageGeneration","id":"g"}), dir.path()).is_none());
    }
}
