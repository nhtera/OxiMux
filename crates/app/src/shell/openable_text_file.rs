//! Filter for "is this path something the text editor can usefully open?"
//!
//! Cheap, extension-based heuristic. No filesystem read — rejecting a
//! `.DS_Store` click should not cost an I/O round-trip. Used by the
//! file-tree click handler before constructing an `EditorView`.

use std::path::Path;

/// Returns `true` when `path` is plausibly a text file the editor can
/// render. Conservative: false-negatives (rejecting a text file) are
/// the expensive failure mode, false-positives (accepting an unusual
/// binary) gracefully degrade to the `EditorView` UTF-8 fallback.
pub fn is_openable_text_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // System metadata + hidden dotfiles that are almost always binary.
    if name == ".DS_Store" || name.starts_with(".Spotlight") || name == ".localized" {
        return false;
    }
    let Some(ext) = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return true; // no extension — assume text (Makefile, LICENSE, etc.)
    };
    !matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "tiff"
            | "tif"
            | "pdf"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "xz"
            | "bz2"
            | "7z"
            | "rar"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "a"
            | "o"
            | "obj"
            | "rlib"
            | "mp3"
            | "mp4"
            | "mov"
            | "avi"
            | "mkv"
            | "webm"
            | "ogg"
            | "wav"
            | "flac"
            | "ttf"
            | "otf"
            | "woff"
            | "woff2"
            | "eot"
            | "db"
            | "sqlite"
            | "sqlite3"
            | "class"
            | "jar"
            | "wasm"
    )
}
