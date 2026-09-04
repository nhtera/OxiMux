//! Filter for "is this path something the editor should try to open?"
//!
//! Used by the file-tree click handler before constructing an `EditorView`.
//! The editor itself now picks a rendering mode (text / image / binary
//! placeholder) at open time via the byte sniffer in `oximux_editor::binary`,
//! so almost any path is fair game. The guard here only refuses paths that
//! are pathologically heavy to open in an editor tab (archives, executables,
//! databases) — opening a 200 MB sqlite or a packed `.dylib` provides no
//! useful preview and would just pin a tab on a giant blob.
//!
//! Cheap, extension-based heuristic. No filesystem read.
//!
//! Failure modes:
//! - false-negative (refusing a path that would have been useful): user
//!   sees a silent no-op on click. Survivable.
//! - false-positive (accepting a path that produces a giant useless tab):
//!   the editor view's binary placeholder still renders cleanly, just with
//!   the cost of having allocated a tab.
//!
//! The list intentionally excludes images (PNG/JPG/SVG/etc.) and
//! filesystem metadata (`.DS_Store`) so those open with the previewer /
//! "Binary file — cannot display" placeholder respectively — matching the
//! always-open behavior of comparable editors.

use std::path::Path;

/// Returns `true` when the editor should accept a click on `path`. The
/// editor decides text-vs-image-vs-binary internally — this filter only
/// rejects formats where opening provides no informational value (archives,
/// executables, databases, fonts, A/V media).
pub fn is_openable_text_file(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|s| s.to_str())
        .map(str::to_ascii_lowercase)
    else {
        // No extension — accept. Covers Makefile / LICENSE / Dockerfile / .DS_Store.
        // Filesystem-metadata files (.DS_Store) fall through and open as a
        // "Binary file — cannot display" placeholder, which matches the
        // user-visible behavior in comparable editors.
        return true;
    };
    !matches!(
        ext.as_str(),
        // Archives — content is opaque without unpacking.
        "zip" | "tar" | "gz" | "tgz" | "xz" | "bz2" | "7z" | "rar"
        // Executables / shared libs / object files.
        | "exe" | "dll" | "so" | "dylib" | "a" | "o" | "obj" | "rlib"
        | "class" | "jar" | "wasm"
        // Audio / video — would either fail to decode or freeze the UI.
        | "mp3" | "mp4" | "mov" | "avi" | "mkv" | "webm" | "ogg" | "wav" | "flac"
        // Fonts.
        | "ttf" | "otf" | "woff" | "woff2" | "eot"
        // Databases.
        | "db" | "sqlite" | "sqlite3"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn refuses_known_archive_formats() {
        assert!(!is_openable_text_file(&PathBuf::from("a.zip")));
        assert!(!is_openable_text_file(&PathBuf::from("a.tar.gz")));
        assert!(!is_openable_text_file(&PathBuf::from("a.7z")));
    }

    #[test]
    fn refuses_executables_and_libs() {
        assert!(!is_openable_text_file(&PathBuf::from("a.exe")));
        assert!(!is_openable_text_file(&PathBuf::from("libfoo.dylib")));
        assert!(!is_openable_text_file(&PathBuf::from("module.wasm")));
    }

    #[test]
    fn accepts_image_formats() {
        // Editor opens as Image preview.
        assert!(is_openable_text_file(&PathBuf::from("logo.png")));
        assert!(is_openable_text_file(&PathBuf::from("photo.jpg")));
        assert!(is_openable_text_file(&PathBuf::from("icon.svg")));
    }

    #[test]
    fn accepts_pdf() {
        // Editor opens as the PDF preview (rasterized page + navigation).
        assert!(is_openable_text_file(&PathBuf::from("spec.pdf")));
        assert!(is_openable_text_file(&PathBuf::from("SPEC.PDF")));
    }

    #[test]
    fn accepts_filesystem_metadata() {
        // Editor opens as Binary placeholder.
        assert!(is_openable_text_file(&PathBuf::from(".DS_Store")));
    }

    #[test]
    fn accepts_source_files() {
        assert!(is_openable_text_file(&PathBuf::from("main.rs")));
        assert!(is_openable_text_file(&PathBuf::from("Makefile")));
        assert!(is_openable_text_file(&PathBuf::from("README.md")));
    }
}
