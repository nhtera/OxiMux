//! The Windows recipe: unsigned bytes, a person for a gate, a junction for a
//! swap.
//!
//! # Why OxiMux downloads this itself
//!
//! Upstream ships `install.ps1`, and it verifies nothing — no checksum, no
//! signature — while a per-asset `checksums.txt` sits unused in the same
//! release (re-confirmed against v0.21.0 on 2026-08-24). Owning the download
//! buys a hash check, a known version, and gate-before-place ordering. It does
//! not buy identity: the artifacts are unsigned and carry no build provenance,
//! so the anchor stays the user's own approval. See [`crate::trust`] for
//! exactly what that does and does not establish.
//!
//! # The layout is upstream's, on purpose
//!
//! Releases land in their own immutable directory and two junctions point at
//! them. That is not cosmetic parity — Windows refuses to overwrite a running
//! `.exe`, so retargeting a junction is the only upgrade that works while the
//! driver is live. Matching upstream's paths also means the two installers
//! coexist: OxiMux can install over theirs, and theirs over OxiMux's.

use std::fs;
use std::path::{Path, PathBuf};

use super::super::InstallError;
use super::{junction, missing_from_archive, Anchor, Gate, Staged};
use crate::discovery;
use crate::trust::{self, Trust};
use crate::verify::{self, VerifiedDriver};
use crate::version::Version;

/// The driver itself — the binary whose bytes are the identity.
pub(crate) const DRIVER_EXE: &str = "cua-driver.exe";
/// Custom cursor themes.
const THEME_EXE: &str = "cua-cursor-theme.exe";
/// The reserved uiAccess worker. Present since 0.2.8, launched by nothing
/// today; placed anyway so a future driver release finds its sibling.
const UIA_EXE: &str = "cua-driver-uia.exe";

/// What gets placed. The archive also carries `cua_driver_sdk.dll`, a Node
/// addon and a C header — SDK payload for embedders, which upstream's own
/// installer does not place either.
const PLACED: [&str; 3] = [DRIVER_EXE, THEME_EXE, UIA_EXE];

/// The release from which a missing `cua-cursor-theme.exe` is a broken archive
/// rather than an old one.
const THEME_REQUIRED_FROM: Version = Version::new(0, 12, 7);

/// Ceiling on total extracted bytes. The real archive unpacks to ~75 MB; this
/// bounds a hostile one that declares far less than it writes.
const EXTRACT_CEILING: u64 = 512 * 1024 * 1024;

/// Per-version directories kept after a successful install. Upstream keeps
/// five; two is a rollback and the one that replaced it. OxiMux is not a
/// version manager.
const KEEP_RELEASES: usize = 2;

/// Rust's target triple for this host, which is also the suffix upstream puts
/// on a release directory name.
fn target_triple() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "aarch64-pc-windows-msvc",
        _ => "x86_64-pc-windows-msvc",
    }
}

/// Package home. Honors upstream's override so a user who moved their install
/// keeps one install rather than gaining a second.
fn home() -> PathBuf {
    if let Some(raw) = std::env::var_os("CUA_DRIVER_RS_HOME") {
        return PathBuf::from(raw);
    }
    PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join(".cua-driver")
}

fn releases_dir() -> PathBuf {
    home().join("packages").join("releases")
}

/// The inner junction, retargeted on every upgrade.
fn current_link() -> PathBuf {
    home().join("packages").join("current")
}

/// Extract the three executables into a staging directory.
///
/// In-process rather than `Expand-Archive`: an extraction path that shells out
/// cannot be tested against a hostile archive, and this one is fed bytes from
/// the internet whose publisher nobody has checked.
pub(in crate::install) fn extract(
    archive: &Path,
    into: &Path,
    claimed: Version,
) -> Result<Staged, InstallError> {
    let root = into.join("driver");
    fs::create_dir_all(&root).map_err(|err| InstallError::Install {
        detail: format!("creating {}: {err}", root.display()),
    })?;

    let file = fs::File::open(archive).map_err(|err| InstallError::Install {
        detail: format!("opening {}: {err}", archive.display()),
    })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| InstallError::Install {
        detail: format!("{} is not a readable zip: {err}", archive.display()),
    })?;

    let mut extracted: u64 = 0;
    let mut listing = Vec::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|err| InstallError::Install {
            detail: format!("reading archive entry {index}: {err}"),
        })?;
        // `enclosed_name` is the traversal guard: it returns `None` for an
        // absolute path, a drive letter, or anything climbing out with `..`.
        let Some(name) = entry
            .enclosed_name()
            .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        else {
            continue;
        };
        listing.push(name.clone());
        if !entry.is_file() || !PLACED.contains(&name.as_str()) {
            continue;
        }

        extracted = extracted.saturating_add(entry.size());
        if extracted > EXTRACT_CEILING {
            return Err(InstallError::Install {
                detail: format!(
                    "archive unpacks to more than {} MB — refusing to continue",
                    EXTRACT_CEILING / (1024 * 1024)
                ),
            });
        }

        let dest = root.join(&name);
        let mut out = fs::File::create(&dest).map_err(|err| InstallError::Install {
            detail: format!("creating {}: {err}", dest.display()),
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|err| InstallError::Install {
            detail: format!("extracting {name}: {err}"),
        })?;
    }

    let binary = root.join(DRIVER_EXE);
    if !binary.is_file() {
        return Err(missing_from_archive(DRIVER_EXE, listing));
    }
    if claimed >= THEME_REQUIRED_FROM && !root.join(THEME_EXE).is_file() {
        return Err(missing_from_archive(THEME_EXE, listing));
    }

    Ok(Staged {
        root,
        binary,
        claimed,
    })
}

/// There is no publisher to ask, so the gate asks the user — unless these exact
/// bytes are already pinned.
///
/// The already-pinned shortcut is not a weakening: the pin *is* the gate, and
/// re-asking about bytes the user already approved trains people to click
/// through the one prompt that matters. Executing them to read a version is
/// legitimate for the same reason — they are approved.
pub(in crate::install) fn gate(staged: &Staged, anchor: &Anchor) -> Result<Gate, InstallError> {
    if let Trust::Approved { .. } = anchor.evaluate(&staged.binary)? {
        return Ok(Gate::Passed(verify::verify_pinned(&staged.binary, anchor)?));
    }

    let bytes = fs::metadata(&staged.binary)
        .map(|meta| meta.len())
        .unwrap_or_default();
    Ok(Gate::NeedsApproval {
        sha256: trust::sha256_of(&staged.binary)?,
        // The *claimed* version: reading the real one means running the binary,
        // and nobody has approved it yet. `verify_placed` reports the exact
        // version once execution is legitimate.
        version: staged.claimed,
        bytes,
    })
}

/// Pin the bytes the user just approved — the staged ones, before they are
/// placed. Placement copies them byte for byte, so the pin still matches.
pub(in crate::install) fn record_approval(
    staged: &Staged,
    anchor: &Anchor,
) -> Result<(), InstallError> {
    anchor.approve(&staged.binary)?;
    Ok(())
}

/// A completed junction retarget awaiting the post-place verdict.
#[derive(Debug)]
pub(in crate::install) struct Placement {
    binary: PathBuf,
    current: PathBuf,
    /// Where `current` pointed before, if anywhere.
    previous: Option<PathBuf>,
    visible: PathBuf,
    /// Whether this install created the outer junction (so rollback knows
    /// whether removing it restores the previous state or destroys it).
    visible_created: bool,
    version_dir: PathBuf,
    version_dir_created: bool,
}

impl Placement {
    pub(in crate::install) fn binary(&self) -> PathBuf {
        self.binary.clone()
    }

    pub(in crate::install) fn commit(self) {
        collect_old_releases(&self.version_dir, self.previous.as_deref())
            .into_iter()
            .for_each(|stale| {
                // A directory whose executable is running cannot be removed.
                // That is fine: it is disk, not correctness, and the next
                // install will find it gone.
                let _ = fs::remove_dir_all(stale);
            });
    }

    pub(in crate::install) fn roll_back(self) {
        match &self.previous {
            Some(previous) => {
                let _ = junction::set_target(&self.current, previous);
            }
            // Removing a junction removes the link, never its target.
            None => {
                let _ = fs::remove_dir(&self.current);
            }
        }
        if self.visible_created {
            let _ = fs::remove_dir(&self.visible);
        }
        if self.version_dir_created {
            let _ = fs::remove_dir_all(&self.version_dir);
        }
    }
}

/// Copy the staged executables into their own release directory, then retarget
/// the junctions at it.
pub(in crate::install) fn place(staged: &Staged) -> Result<Placement, InstallError> {
    let version_dir = releases_dir().join(format!("{}-{}", staged.claimed, target_triple()));
    let version_dir_created = !version_dir.exists();

    // What this release still owes the directory. A per-version directory is
    // immutable once complete, so a repeat install is normally a no-op — but
    // "complete" has to mean every file, not just the driver.
    //
    // A directory holding only `cua-driver.exe` is a state that really happens:
    // the executable is the one file a running daemon locks, so a half-finished
    // removal deletes its siblings and leaves it behind. Keying the skip on that
    // one file then made the gap permanent — every later install saw the driver
    // present and copied nothing. Observed on a real machine, not imagined.
    let missing: Vec<&str> = PLACED
        .iter()
        .copied()
        .filter(|name| staged.root.join(name).is_file() && !version_dir.join(name).is_file())
        .collect();

    if !missing.is_empty() {
        fs::create_dir_all(&version_dir).map_err(|err| InstallError::Install {
            detail: format!("creating {}: {err}", version_dir.display()),
        })?;
        for name in missing {
            fs::copy(staged.root.join(name), version_dir.join(name)).map_err(|err| {
                InstallError::Install {
                    detail: format!("installing {name}: {err}"),
                }
            })?;
        }
    }

    let current = current_link();
    let previous = fs::canonicalize(&current).ok();
    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent).map_err(|err| InstallError::Install {
            detail: format!("creating {}: {err}", parent.display()),
        })?;
    }
    // The commit point: one kernel call, and the link is never absent.
    junction::set_target(&current, &version_dir).map_err(|err| InstallError::Install {
        detail: format!("pointing {} at this release: {err}", current.display()),
    })?;

    let visible = discovery::windows_bin_dir().ok_or_else(|| InstallError::Install {
        detail: "neither %LOCALAPPDATA% nor CUA_DRIVER_RS_INSTALL_DIR is set".into(),
    })?;
    let visible_created = !visible.exists();
    if let Some(parent) = visible.parent() {
        fs::create_dir_all(parent).map_err(|err| InstallError::Install {
            detail: format!("creating {}: {err}", parent.display()),
        })?;
    }
    junction::set_target(&visible, &current).map_err(|err| InstallError::Install {
        detail: format!("pointing {} at the install: {err}", visible.display()),
    })?;

    Ok(Placement {
        binary: visible.join(DRIVER_EXE),
        current,
        previous,
        visible,
        visible_created,
        version_dir,
        version_dir_created,
    })
}

/// Prove that what landed is what the user approved.
pub(in crate::install) fn verify_placed(
    binary: &Path,
    anchor: &Anchor,
) -> Result<VerifiedDriver, crate::Error> {
    verify::verify_pinned(binary, anchor)
}

/// The installed version, read off the release directory the `current` junction
/// resolves to.
///
/// Deliberately not `prepare()`: that would execute the installed driver, and
/// the downgrade guard has no business running a binary — least of all one this
/// install is about to replace. The directory name is upstream's own
/// `<version>-<target>` convention, so it is as authoritative as the install.
pub(in crate::install) fn installed_version() -> Option<Version> {
    let resolved = fs::canonicalize(current_link()).ok()?;
    let name = resolved.file_name()?.to_str()?;
    Version::parse(name.split('-').next()?)
}

/// Release directories to remove after a successful install: everything except
/// the newest [`KEEP_RELEASES`], the one just installed, and the one that was
/// live before it.
fn collect_old_releases(keep: &Path, previous: Option<&Path>) -> Vec<PathBuf> {
    let mut releases: Vec<(Version, PathBuf)> = fs::read_dir(releases_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let path = entry.path();
            let version = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split('-').next().and_then(Version::parse))?;
            Some((version, path))
        })
        .collect();
    releases.sort_by_key(|(version, _)| std::cmp::Reverse(*version));

    releases
        .into_iter()
        .skip(KEEP_RELEASES)
        .map(|(_, path)| path)
        .filter(|path| path != keep)
        .filter(|path| {
            previous.is_none_or(|previous| {
                fs::canonicalize(path).ok().as_deref() != Some(previous)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a zip in memory with the entries a test needs, stored uncompressed
    /// (the reader accepts both; the writer needs no compressor this way).
    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            writer.start_file(*name, options).expect("start entry");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish zip");
        buffer.into_inner()
    }

    fn archive_of(entries: &[(&str, &[u8])], dir: &Path) -> PathBuf {
        let path = dir.join("driver.zip");
        fs::write(&path, zip_with(entries)).expect("write archive");
        path
    }

    #[test]
    fn only_the_placed_executables_are_extracted() {
        let staging = tempfile::tempdir().expect("tempdir");
        let archive = archive_of(
            &[
                (DRIVER_EXE, b"driver"),
                (THEME_EXE, b"theme"),
                (UIA_EXE, b"uia"),
                ("cua_driver_sdk.dll", b"sdk"),
                ("cua_driver_abi.h", b"header"),
            ],
            staging.path(),
        );

        let staged = extract(&archive, staging.path(), Version::new(0, 21, 0)).expect("extract");

        assert_eq!(fs::read(&staged.binary).expect("driver"), b"driver");
        assert!(staged.root.join(THEME_EXE).is_file());
        assert!(staged.root.join(UIA_EXE).is_file());
        assert!(
            !staged.root.join("cua_driver_sdk.dll").exists(),
            "SDK payload is not part of an install"
        );
        assert!(!staged.root.join("cua_driver_abi.h").exists());
    }

    /// The prefixed archive variant: upstream also publishes a zip whose
    /// entries all sit under `cua-driver-rs-<version>-<arch>/`. Extraction must
    /// not care which one it was handed.
    #[test]
    fn a_prefixed_archive_extracts_the_same_way() {
        let staging = tempfile::tempdir().expect("tempdir");
        let archive = archive_of(
            &[
                ("cua-driver-rs-0.21.0-windows-x86_64/cua-driver.exe", b"d"),
                (
                    "cua-driver-rs-0.21.0-windows-x86_64/cua-cursor-theme.exe",
                    b"t",
                ),
            ],
            staging.path(),
        );

        let staged = extract(&archive, staging.path(), Version::new(0, 21, 0)).expect("extract");
        assert!(staged.binary.is_file());
    }

    #[test]
    fn an_archive_without_the_driver_names_what_it_held() {
        let staging = tempfile::tempdir().expect("tempdir");
        let archive = archive_of(&[("cua_driver_sdk.dll", b"sdk")], staging.path());

        match extract(&archive, staging.path(), Version::new(0, 21, 0)) {
            Err(InstallError::ArchiveIncomplete { listing }) => {
                assert!(listing.contains(DRIVER_EXE), "must name what was expected");
                assert!(listing.contains("cua_driver_sdk.dll"), "and what was there");
            }
            other => panic!("expected the missing-payload error, got {other:?}"),
        }
    }

    /// A release new enough to require cursor themes must not install without
    /// them; an older one may.
    #[test]
    fn a_missing_theme_binary_is_fatal_only_from_the_release_that_requires_it() {
        let staging = tempfile::tempdir().expect("tempdir");
        let archive = archive_of(&[(DRIVER_EXE, b"driver")], staging.path());

        assert!(extract(&archive, staging.path(), Version::new(0, 21, 0)).is_err());
        assert!(extract(&archive, staging.path(), Version::new(0, 12, 6)).is_ok());
    }

    /// Entries that try to climb out of the staging directory are dropped by
    /// `enclosed_name`, and nothing is written outside it.
    #[test]
    fn a_traversing_entry_cannot_escape_the_staging_directory() {
        let staging = tempfile::tempdir().expect("tempdir");
        let archive = archive_of(
            &[
                ("../../cua-driver.exe", b"escapee"),
                (DRIVER_EXE, b"driver"),
            ],
            staging.path(),
        );

        let staged = extract(&archive, staging.path(), Version::new(0, 12, 6)).expect("extract");
        assert_eq!(
            fs::read(&staged.binary).expect("driver"),
            b"driver",
            "the real entry must win, not the traversing one"
        );
        assert!(!staging.path().join("..").join("cua-driver.exe").exists());
    }

    /// Both junctions, the retarget upgrade, and the version read back off the
    /// release directory — the whole placement contract in one pass, against a
    /// redirected install root so no test touches a real one.
    #[test]
    fn placing_a_release_wires_both_junctions_and_upgrades_by_retarget() {
        let root = tempfile::tempdir().expect("tempdir");
        redirected(root.path(), || {
            let first = place(&fake_staged(root.path(), Version::new(0, 20, 0), b"one"))
                .expect("first install");
            let visible = discovery::windows_bin_dir().expect("redirected bin dir");

            assert_eq!(
                fs::read(visible.join(DRIVER_EXE)).expect("read through both junctions"),
                b"one"
            );
            assert_eq!(installed_version(), Some(Version::new(0, 20, 0)));
            first.commit();

            // The upgrade: a new release directory, one junction retarget, and
            // nothing overwrote the executable that was already there.
            let second = place(&fake_staged(root.path(), Version::new(0, 21, 0), b"two"))
                .expect("upgrade");
            assert_eq!(fs::read(visible.join(DRIVER_EXE)).expect("read"), b"two");
            assert_eq!(installed_version(), Some(Version::new(0, 21, 0)));
            second.commit();
        });
    }

    /// A release directory left holding only `cua-driver.exe` — what a partial
    /// removal leaves behind, because a running daemon locks that one file — must
    /// be completed by the next install rather than accepted as done.
    #[test]
    fn placing_over_an_incomplete_release_restores_its_missing_files() {
        let root = tempfile::tempdir().expect("tempdir");
        redirected(root.path(), || {
            let staged = fake_staged(root.path(), Version::new(0, 21, 0), b"one");
            place(&staged).expect("first install").commit();

            // Simulate the half-removal: siblings gone, the locked driver left.
            let version_dir = releases_dir().join(format!("0.21.0-{}", target_triple()));
            fs::remove_file(version_dir.join(THEME_EXE)).expect("remove theme");
            assert!(version_dir.join(DRIVER_EXE).is_file());

            place(&staged).expect("second install").commit();
            assert!(
                version_dir.join(THEME_EXE).is_file(),
                "the missing sibling must be replaced, not skipped"
            );
        });
    }

    /// A failed post-place verification must leave the previous release live.
    #[test]
    fn rolling_back_an_upgrade_restores_the_previous_release() {
        let root = tempfile::tempdir().expect("tempdir");
        redirected(root.path(), || {
            place(&fake_staged(root.path(), Version::new(0, 20, 0), b"one"))
                .expect("first install")
                .commit();
            let visible = discovery::windows_bin_dir().expect("redirected bin dir");

            place(&fake_staged(root.path(), Version::new(0, 21, 0), b"two"))
                .expect("upgrade")
                .roll_back();

            assert_eq!(
                fs::read(visible.join(DRIVER_EXE)).expect("read"),
                b"one",
                "the previous release must be live again"
            );
            assert_eq!(installed_version(), Some(Version::new(0, 20, 0)));
        });
    }

    /// A real directory where the visible junction belongs is someone else's —
    /// very possibly upstream's own install — and placement must refuse it
    /// rather than replace it.
    #[test]
    fn placement_refuses_a_real_directory_at_the_bin_path() {
        let root = tempfile::tempdir().expect("tempdir");
        redirected(root.path(), || {
            let visible = discovery::windows_bin_dir().expect("redirected bin dir");
            fs::create_dir_all(&visible).expect("mkdir");
            fs::write(visible.join("someone-elses-file"), b"").expect("write");

            let err = place(&fake_staged(root.path(), Version::new(0, 21, 0), b"one"))
                .expect_err("must refuse");
            assert!(err.to_string().contains("bin"), "{err}");
            assert!(visible.join("someone-elses-file").exists());
        });
    }

    /// A staged driver whose bytes are `contents`, in its own directory so
    /// repeated calls in one test do not overwrite each other.
    fn fake_staged(root: &Path, claimed: Version, contents: &[u8]) -> Staged {
        let staged = root.join(format!("staged-{claimed}"));
        fs::create_dir_all(&staged).expect("mkdir");
        fs::write(staged.join(DRIVER_EXE), contents).expect("driver");
        fs::write(staged.join(THEME_EXE), b"theme").expect("theme");
        Staged {
            binary: staged.join(DRIVER_EXE),
            root: staged,
            claimed,
        }
    }

    /// Point the package home and the visible bin dir inside `root` for the
    /// duration of `body`.
    ///
    /// Serialized on a mutex: these are process-wide environment variables, and
    /// two placement tests running at once would install over each other.
    fn redirected(root: &Path, body: impl FnOnce()) {
        static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let home = root.join("home");
        let bin = root.join("visible").join("bin");
        let previous = [
            std::env::var_os("CUA_DRIVER_RS_HOME"),
            std::env::var_os("CUA_DRIVER_RS_INSTALL_DIR"),
        ];
        // SAFETY: serialized by the mutex above; nothing else in this crate
        // reads these variables off another thread.
        unsafe {
            std::env::set_var("CUA_DRIVER_RS_HOME", &home);
            std::env::set_var("CUA_DRIVER_RS_INSTALL_DIR", &bin);
        }

        body();

        unsafe {
            for (key, value) in ["CUA_DRIVER_RS_HOME", "CUA_DRIVER_RS_INSTALL_DIR"]
                .iter()
                .zip(previous)
            {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn the_release_directory_name_carries_the_version() {
        // The parse `installed_version` performs, against the name `place`
        // writes — the two must not drift.
        let name = format!("{}-{}", Version::new(0, 21, 0), target_triple());
        assert_eq!(
            name.split('-').next().and_then(Version::parse),
            Some(Version::new(0, 21, 0))
        );
    }
}
