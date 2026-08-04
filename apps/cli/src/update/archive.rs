//! Unpacking a release archive.
//!
//! Release archives are **flat** `.tar.gz` on every platform, Windows
//! included: one archive format means one extraction path, and Windows has
//! shipped bsdtar (`tar.exe`) since Windows 10 1803, which is well below the
//! ConPTY floor this project already requires.
//!
//! Extraction is by *allow-list*, not by walking what the archive says. The
//! two names are constants in this file and the destination is always
//! `into.join(<our constant>)` — an entry's own path is only ever compared,
//! never joined — so no archive can write outside the staging directory
//! regardless of what its headers claim.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use super::{UpdateError, cli_name, relay_name};

/// Where the two binaries landed.
#[derive(Debug)]
pub struct Unpacked {
    pub cli: PathBuf,
    pub relay: PathBuf,
}

pub fn extract(archive: &[u8], into: &Path) -> Result<Unpacked, UpdateError> {
    std::fs::create_dir_all(into).map_err(|err| UpdateError::Staging {
        detail: format!("could not create {}: {err}", into.display()),
    })?;

    let wanted = [cli_name(), relay_name()];
    let mut found: Vec<String> = Vec::new();

    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|err| UpdateError::Archive { detail: format!("unreadable archive: {err}") })?;

    for entry in entries {
        let mut entry = entry
            .map_err(|err| UpdateError::Archive { detail: format!("unreadable entry: {err}") })?;
        let path = entry
            .path()
            .map_err(|err| UpdateError::Archive { detail: format!("unreadable path: {err}") })?
            .to_string_lossy()
            .to_string();
        let Some(name) = wanted.iter().find(|w| **w == path) else {
            continue;
        };
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|err| UpdateError::Archive {
            detail: format!("could not read {name} out of the archive: {err}"),
        })?;
        std::fs::write(into.join(name), &bytes).map_err(|err| UpdateError::Staging {
            detail: format!("could not write {name}: {err}"),
        })?;
        found.push(name.clone());
    }

    for name in &wanted {
        if !found.contains(name) {
            return Err(UpdateError::Archive {
                detail: format!("the release archive does not contain {name}"),
            });
        }
    }
    Ok(Unpacked { cli: into.join(cli_name()), relay: into.join(relay_name()) })
}

#[cfg(test)]
pub(super) fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        // The name goes into the raw header rather than through `set_path`,
        // which refuses `..` and absolute paths — precisely the entries the
        // extractor has to be proven against. An attacker writing a hostile
        // archive is not constrained by our tar library's guard rails, so a
        // test that relied on them would be proving the wrong thing.
        {
            let gnu = header.as_gnu_mut().expect("a gnu header");
            let raw = name.as_bytes();
            assert!(raw.len() < gnu.name.len(), "{name:?} does not fit a tar header");
            gnu.name[..raw.len()].copy_from_slice(raw);
        }
        header.set_cksum();
        builder.append(&header, *bytes).expect("append");
    }
    let tar = builder.into_inner().expect("finish tar");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    std::io::Write::write_all(&mut encoder, &tar).expect("gzip");
    encoder.finish().expect("finish gzip")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_both_binaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive =
            tar_gz(&[(&cli_name(), b"new cli"), (&relay_name(), b"new relay")]);

        let unpacked = extract(&archive, dir.path()).expect("extracts");
        assert_eq!(std::fs::read(&unpacked.cli).expect("cli"), b"new cli");
        assert_eq!(std::fs::read(&unpacked.relay).expect("relay"), b"new relay");
    }

    /// Both or neither: a CLI without its relay is exactly the version split
    /// the paired swap exists to prevent, so it must fail before anything is
    /// staged rather than half-succeed.
    #[test]
    fn an_archive_missing_the_relay_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = tar_gz(&[(&cli_name(), b"new cli")]);
        let err = extract(&archive, dir.path()).expect_err("must refuse");
        assert!(err.to_string().contains(&relay_name()), "{err}");
    }

    /// The traversal case. `../../../etc/oximux` is not one of the two names
    /// we compare against, so it is skipped — and because the destination is
    /// always `into.join(<our own constant>)`, even a name that *did* match
    /// could not land outside the staging directory.
    #[test]
    fn entries_that_are_not_the_two_binaries_are_never_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let escape = format!("../{}", cli_name());
        let archive = tar_gz(&[
            (escape.as_str(), b"evil"),
            ("/tmp/oximux-absolute", b"evil"),
            ("payload.sh", b"evil"),
            (&cli_name(), b"new cli"),
            (&relay_name(), b"new relay"),
        ]);

        extract(&archive, dir.path()).expect("extracts");

        assert_eq!(std::fs::read(dir.path().join(cli_name())).expect("cli"), b"new cli");
        assert!(!dir.path().join("payload.sh").exists());
        assert!(
            !dir.path().parent().expect("parent").join(cli_name()).exists(),
            "nothing may be written outside the staging directory"
        );
    }

    #[test]
    fn bytes_that_are_not_an_archive_are_refused_rather_than_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = extract(b"not a tarball at all", dir.path()).expect_err("must refuse");
        assert!(matches!(err, UpdateError::Archive { .. }), "{err}");
    }
}
