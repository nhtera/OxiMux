//! Unpacking the desktop app's release zip.
//!
//! The CLI extracts two known file names out of a `.tar.gz` and can therefore
//! ignore everything an archive claims about its own layout. An app payload
//! cannot: it is a whole install directory whose contents legitimately vary
//! between releases — `scripts/bundle-windows.ps1` globs the native libraries
//! rather than listing them, precisely so a renamed DLL rides along without a
//! hand edit.
//!
//! So this one *does* walk the archive, and the traversal defence has to be
//! real rather than structural:
//!
//! * `enclosed_name` refuses absolute paths, drive letters, and any `..` hop —
//!   an entry that fails it is not skipped but fatal, because a release zip
//!   containing one is not a release zip.
//! * Every entry must sit under the single `OxiMux/` root the bundle script
//!   produces. Anything else means the archive is not the shape this expects,
//!   and guessing at a different one is how a payload lands in the wrong place.
//! * Nothing is executed, and no entry mode is honoured — Windows has no
//!   execute bit and this never runs on unix outside its own tests.

use std::io::Cursor;
use std::path::{Component, Path, PathBuf};

use crate::release::ReleaseError;

/// The directory `Compress-Archive -Path dist/OxiMux` puts at the top of the
/// zip. Extracting `dist/OxiMux/*` instead would scatter loose files into
/// whatever folder someone unzipped into, so the bundle script deliberately
/// archives the directory — and this deliberately depends on that.
const PAYLOAD_ROOT: &str = "OxiMux";

/// The one file whose absence means the archive is not an app payload at all.
/// Checked after extraction so the error names what was actually in there.
const REQUIRED: &str = "oximux.exe";

/// Uncompressed ceiling. The real payload is a couple of hundred megabytes,
/// dominated by `onnxruntime.dll`; a gigabyte is far above any release and far
/// below what a decompression bomb wants.
const EXTRACT_CEILING: u64 = 1024 * 1024 * 1024;

/// Extract the payload into `into`, returning the files written, relative to
/// it — the exact list the swap will replace.
///
/// `into` must already exist and should be empty; a [`crate::release::Staging`]
/// directory is what every caller passes.
pub fn extract(archive: &[u8], into: &Path) -> Result<Vec<PathBuf>, ReleaseError> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(|err| {
        ReleaseError::Archive { detail: format!("the update is not a readable zip: {err}") }
    })?;

    let mut written = Vec::new();
    let mut listing = Vec::new();
    let mut total: u64 = 0;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|err| ReleaseError::Archive {
            detail: format!("could not read archive entry {index}: {err}"),
        })?;
        let raw = entry.name().to_string();
        let Some(enclosed) = entry.enclosed_name() else {
            return Err(ReleaseError::Archive {
                detail: format!("the update archive contains an unsafe path: {raw:?}"),
            });
        };
        let Some(relative) = strip_payload_root(&enclosed) else {
            return Err(ReleaseError::Archive {
                detail: format!(
                    "the update archive contains {raw:?}, which is outside its {PAYLOAD_ROOT}/ root"
                ),
            });
        };
        // The root entry itself.
        if relative.as_os_str().is_empty() {
            continue;
        }

        let dest = into.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|err| ReleaseError::Staging {
                detail: format!("could not create {}: {err}", dest.display()),
            })?;
            continue;
        }
        if !entry.is_file() {
            // A symlink or a device entry. Neither belongs in this payload,
            // and a symlink in particular is how an extractor is talked into
            // writing outside the directory it was told to write into.
            return Err(ReleaseError::Archive {
                detail: format!("the update archive contains a non-file entry: {raw:?}"),
            });
        }

        total = total.saturating_add(entry.size());
        if total > EXTRACT_CEILING {
            return Err(ReleaseError::Archive {
                detail: format!(
                    "the update unpacks to more than {} MB — refusing to continue",
                    EXTRACT_CEILING / (1024 * 1024)
                ),
            });
        }

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ReleaseError::Staging {
                detail: format!("could not create {}: {err}", parent.display()),
            })?;
        }
        let mut out = std::fs::File::create(&dest).map_err(|err| ReleaseError::Staging {
            detail: format!("could not write {}: {err}", dest.display()),
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|err| ReleaseError::Staging {
            detail: format!("could not extract {raw}: {err}"),
        })?;

        listing.push(relative.to_string_lossy().to_string());
        written.push(relative);
    }

    if !into.join(REQUIRED).is_file() {
        listing.sort();
        return Err(ReleaseError::Archive {
            detail: format!(
                "the update archive carries no {REQUIRED} (it has: {})",
                if listing.is_empty() { "nothing".to_string() } else { listing.join(", ") }
            ),
        });
    }
    Ok(written)
}

/// `OxiMux/foo/bar.dll` → `foo/bar.dll`; anything not under that root →
/// `None`. The comparison is case-insensitive because the archive is built and
/// consumed on Windows, where `oximux/` and `OxiMux/` name one directory.
fn strip_payload_root(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return None;
    };
    if !first.to_str()?.eq_ignore_ascii_case(PAYLOAD_ROOT) {
        return None;
    }
    Some(components.as_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a zip in memory. `entries` are full archive paths, so a test can
    /// write whatever shape it needs to attack.
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in entries {
                writer.start_file(*name, options).expect("start entry");
                writer.write_all(bytes).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        buffer
    }

    fn payload() -> Vec<u8> {
        zip_of(&[
            ("OxiMux/oximux.exe", b"app"),
            ("OxiMux/oximux-relay.exe", b"relay"),
            ("OxiMux/onnxruntime.dll", b"native"),
        ])
    }

    #[test]
    fn the_payload_lands_flat_with_its_root_stripped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut written = extract(&payload(), dir.path()).expect("extracts");
        written.sort();

        assert_eq!(
            written,
            vec![
                PathBuf::from("onnxruntime.dll"),
                PathBuf::from("oximux-relay.exe"),
                PathBuf::from("oximux.exe"),
            ]
        );
        assert_eq!(std::fs::read(dir.path().join("oximux.exe")).expect("read"), b"app");
        assert!(!dir.path().join("OxiMux").exists(), "the root must be stripped, not kept");
    }

    /// Nested paths are real — a future payload may ship a `resources/`
    /// folder — so the guard has to be about escaping, not about depth.
    #[test]
    fn a_nested_file_keeps_its_relative_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = zip_of(&[("OxiMux/oximux.exe", b"app"), ("OxiMux/res/icon.ico", b"icon")]);

        extract(&archive, dir.path()).expect("extracts");
        assert_eq!(std::fs::read(dir.path().join("res/icon.ico")).expect("read"), b"icon");
    }

    /// The attack this file exists to stop. A skip would be worse than a
    /// refusal: the rest of the payload would install and look fine.
    #[test]
    fn an_entry_escaping_the_payload_root_is_fatal_not_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in [
            "OxiMux/../../evil.exe",
            "../evil.exe",
            "Other/oximux.exe",
            "evil.exe",
            "/etc/passwd",
        ] {
            let archive = zip_of(&[("OxiMux/oximux.exe", b"app"), (bad, b"pwned")]);
            let err = extract(&archive, dir.path()).expect_err("must refuse {bad}");
            assert!(
                matches!(err, ReleaseError::Archive { .. }),
                "{bad:?} produced {err:?}"
            );
        }
    }

    /// An archive that verified its signature and its digest but carries the
    /// wrong contents is still not installable, and the error has to say what
    /// was in there — "no oximux.exe" alone sends nobody anywhere.
    #[test]
    fn an_archive_without_the_app_names_what_it_did_contain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = zip_of(&[("OxiMux/readme.txt", b"hi")]);

        let err = extract(&archive, dir.path()).expect_err("must refuse");
        let rendered = err.to_string();
        assert!(rendered.contains("oximux.exe"), "{rendered}");
        assert!(rendered.contains("readme.txt"), "{rendered}");
    }

    #[test]
    fn the_root_directory_matches_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = zip_of(&[("oximux/oximux.exe", b"app")]);
        extract(&archive, dir.path()).expect("extracts");
        assert!(dir.path().join("oximux.exe").is_file());
    }
}
