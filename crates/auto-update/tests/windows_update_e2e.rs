//! The Windows updater end to end: a signed release goes in, an install
//! directory comes out replaced — and every refusal along the way leaves the
//! installed files exactly as they were.
//!
//! What these assert is *ordering*, which is the part of a trust chain that
//! silently rots. A gate that runs after the thing it was meant to guard is
//! still a gate, still passes its own unit test, and protects nothing. So each
//! refusal test checks not only that the update failed but that the step it was
//! meant to prevent never happened: no payload downloaded, no file touched.
//!
//! The staging and the swap are deliberately exercised as two calls with a
//! gap between them, because that is what actually happens — the check runs in
//! the background, the swap runs when the user quits, and hours can pass.

#![cfg(target_os = "windows")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use oximux_auto_update::release::download::{self, Fetcher};
use oximux_auto_update::release::testkit::MinisignKeypair;
use oximux_auto_update::release::{ReleaseError, verify};
use oximux_auto_update::windows::{install::InstalledApp, pipeline, staging};
use oximux_auto_update::{CheckTrigger, UpdateStatus, UpdaterConfig, Version};

const TARGET: &str = "x86_64-pc-windows-msvc";
const RUNNING: &str = "0.1.15";

/// Serves canned bytes and records every URL asked for.
#[derive(Default)]
struct MapFetcher {
    files: HashMap<String, Vec<u8>>,
    asked: RefCell<Vec<String>>,
}

impl MapFetcher {
    fn asked_for_a_payload(&self) -> bool {
        self.asked.borrow().iter().any(|url| url.ends_with(".zip"))
    }
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str, ceiling: u64) -> Result<Vec<u8>, ReleaseError> {
        self.asked.borrow_mut().push(url.to_string());
        let body = self.files.get(url).cloned().ok_or(ReleaseError::NoRelease)?;
        if body.len() as u64 > ceiling {
            return Err(ReleaseError::Oversize { ceiling });
        }
        Ok(body)
    }
}

/// The app payload as `scripts/bundle-windows.ps1` builds it: the whole
/// `OxiMux\` directory, not its contents.
fn payload_zip(version: &str) -> Vec<u8> {
    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut buffer));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, contents) in [
            ("OxiMux/oximux.exe", format!("app {version}")),
            ("OxiMux/oximux-relay.exe", format!("relay {version}")),
            ("OxiMux/onnxruntime.dll", format!("native {version}")),
        ] {
            writer.start_file(name, options).expect("start entry");
            writer.write_all(contents.as_bytes()).expect("write entry");
        }
        writer.finish().expect("finish zip");
    }
    buffer
}

/// A signed release: manifest, signature, and the payload the manifest names.
struct Release {
    fetcher: MapFetcher,
    public_key: String,
}

impl Release {
    fn build(version: &str, sign_with: Option<&MinisignKeypair>, corrupt_payload: bool) -> Self {
        let keys = MinisignKeypair::generate();
        let payload = payload_zip(version);
        let honest_digest = verify::sha256_hex(&payload);
        let served = if corrupt_payload { b"tampered bytes".to_vec() } else { payload };

        let name = format!("OxiMux-{version}-windows-x64.zip");
        let raw = format!(
            r#"{{"schemaVersion":1,"version":"{version}","channel":"stable","targets":{{}},"apps":{{"{TARGET}":{{"archive":"{name}","size":{},"sha256":"{honest_digest}"}}}}}}"#,
            served.len().max(1)
        );
        let signature = sign_with.unwrap_or(&keys).sign(raw.as_bytes());

        let mut files = HashMap::new();
        files.insert(download::manifest_url(), raw.into_bytes());
        files.insert(download::signature_url(), signature.into_bytes());
        files.insert(download::asset_url(&format!("v{version}"), &name), served);
        Self {
            fetcher: MapFetcher { files, asked: RefCell::default() },
            public_key: keys.public_key_base64(),
        }
    }

    fn signed(version: &str) -> Self {
        Self::build(version, None, false)
    }
}

/// An install directory with the running version's files in it, plus the
/// config the pipeline resolves at boot.
struct Fixture {
    _root: tempfile::TempDir,
    install: PathBuf,
    config: UpdaterConfig,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let install = root.path().join("OxiMux");
    std::fs::create_dir(&install).expect("install dir");
    for name in ["oximux.exe", "oximux-relay.exe", "onnxruntime.dll"] {
        std::fs::write(install.join(name), format!("{name} {RUNNING}")).expect("installed file");
    }
    let config = UpdaterConfig {
        current_version: Version::parse(RUNNING).expect("running version parses"),
        app: InstalledApp { install_dir: install.clone() },
        cache_dir: root.path().join("cache"),
        manifest_path: root.path().join("pending-update.json"),
    };
    Fixture { _root: root, install, config }
}

fn check(fx: &Fixture, release: &Release) -> Result<UpdateStatus, oximux_auto_update::UpdateError> {
    pipeline::run_with(
        &release.fetcher,
        Some(&release.public_key),
        &fx.config,
        CheckTrigger::Manual,
        &Arc::new(AtomicBool::new(false)),
        &|_| {},
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("readable")
}

fn install_is_untouched(fx: &Fixture) {
    for name in ["oximux.exe", "oximux-relay.exe", "onnxruntime.dll"] {
        assert_eq!(
            read(&fx.install.join(name)),
            format!("{name} {RUNNING}"),
            "{name} must still be the running version"
        );
    }
}

/// The whole point, in one test: check, quit, and come back replaced.
#[test]
fn a_signed_release_stages_then_replaces_the_whole_install() {
    let fx = fixture();
    let release = Release::signed("0.2.0");

    let status = check(&fx, &release).expect("stages");
    assert!(
        matches!(&status, UpdateStatus::Ready { version, .. } if version == "0.2.0"),
        "{status:?}"
    );
    // Staging alone must change nothing — the running app is still mapping
    // every one of these files.
    install_is_untouched(&fx);

    let outcome = oximux_auto_update::apply_pending_update(&fx.config, None);
    assert_eq!(outcome, staging::SwapOutcome::Applied);
    assert_eq!(read(&fx.install.join("oximux.exe")), "app 0.2.0");
    assert_eq!(read(&fx.install.join("oximux-relay.exe")), "relay 0.2.0");
    assert_eq!(read(&fx.install.join("onnxruntime.dll")), "native 0.2.0");
}

/// The criterion a checksum alone cannot meet. The manifest is internally
/// consistent — its sha256 really is the payload's — and it is still refused,
/// because it is not signed by the key this build trusts.
#[test]
fn a_manifest_signed_by_another_key_is_refused_before_anything_is_downloaded() {
    let fx = fixture();
    let attacker = MinisignKeypair::generate();
    let release = Release::build("9.9.9", Some(&attacker), false);

    check(&fx, &release).expect_err("must refuse");

    assert!(!release.fetcher.asked_for_a_payload(), "the payload must never be requested");
    assert!(!fx.config.manifest_path.exists(), "and nothing may be recorded as pending");
    install_is_untouched(&fx);
}

/// Signature good, bytes swapped underneath it. The digest gate catches it,
/// and must catch it before anything is extracted.
#[test]
fn a_payload_that_does_not_match_the_signed_digest_is_never_extracted() {
    let fx = fixture();
    let release = Release::build("0.2.0", None, true);

    check(&fx, &release).expect_err("must refuse");

    assert!(!fx.config.manifest_path.exists());
    assert!(staged_dirs(&fx).is_empty(), "no staging directory may survive");
    install_is_untouched(&fx);
}

/// A correctly signed *old* release must not walk an installation backwards
/// onto a fixed bug. Signed replay is the attack; the version gate is the
/// answer, and it has to run before the download.
#[test]
fn a_correctly_signed_older_release_is_refused_without_downloading_it() {
    let fx = fixture();
    let release = Release::signed("0.1.2");

    let status = check(&fx, &release).expect("a downgrade is not an error, it is up-to-date");
    assert_eq!(status, UpdateStatus::UpToDate);
    assert!(!release.fetcher.asked_for_a_payload(), "a downgrade must not even be downloaded");
    install_is_untouched(&fx);
}

#[test]
fn the_running_version_is_not_an_update() {
    let fx = fixture();
    let release = Release::signed(RUNNING);
    assert_eq!(check(&fx, &release).expect("checks"), UpdateStatus::UpToDate);
}

/// Without a trust root there is nothing to verify against, so the only safe
/// answer is to refuse — never to fall back to the manifest's own checksum,
/// which came from the same place as the artifact it describes.
#[test]
fn a_build_with_no_release_key_never_reaches_the_network() {
    let fx = fixture();
    let release = Release::signed("0.2.0");

    pipeline::run_with(
        &release.fetcher,
        None,
        &fx.config,
        CheckTrigger::Manual,
        &Arc::new(AtomicBool::new(false)),
        &|_| {},
    )
    .expect_err("must refuse");

    assert!(release.fetcher.asked.borrow().is_empty(), "not one request may be made");
}

/// The expensive mistake: a user who leaves the app open for a day and gets
/// two background checks must not download the same payload twice.
#[test]
fn a_second_check_finds_the_payload_already_staged() {
    let fx = fixture();
    let release = Release::signed("0.2.0");
    check(&fx, &release).expect("stages");
    let after_first = release.fetcher.asked.borrow().len();

    let status = check(&fx, &release).expect("checks again");
    assert!(matches!(status, UpdateStatus::Ready { .. }), "{status:?}");
    let asked: Vec<String> = release.fetcher.asked.borrow().clone();
    assert!(
        !asked[after_first..].iter().any(|url| url.ends_with(".zip")),
        "the payload must not be fetched twice: {asked:?}"
    );
}

/// A newer release supersedes a staged older one, and the older one's staging
/// directory has to go with it — otherwise the parent accumulates one per
/// release, at a few hundred megabytes each.
#[test]
fn a_newer_release_replaces_a_staged_one_rather_than_stacking_beside_it() {
    let fx = fixture();
    check(&fx, &Release::signed("0.2.0")).expect("stages");
    assert_eq!(staged_dirs(&fx).len(), 1);

    let status = check(&fx, &Release::signed("0.3.0")).expect("stages the newer one");
    assert!(
        matches!(&status, UpdateStatus::Ready { version, .. } if version == "0.3.0"),
        "{status:?}"
    );
    assert_eq!(staged_dirs(&fx).len(), 1, "exactly one staging directory at a time");

    oximux_auto_update::apply_pending_update(&fx.config, None);
    assert_eq!(read(&fx.install.join("oximux.exe")), "app 0.3.0");
}

/// The swap leaves backups it cannot delete, because this process is running
/// out of the files it just replaced. Finishing that is the next launch's job,
/// and until it happens the install directory carries a `.old-` copy of every
/// file — which is exactly what it must *not* carry two updates later.
#[test]
fn the_next_launch_clears_the_backups_the_swap_could_not_delete() {
    let fx = fixture();
    check(&fx, &Release::signed("0.2.0")).expect("stages");
    oximux_auto_update::apply_pending_update(&fx.config, None);

    // Simulate what Windows does to a mapped image: put a backup back and
    // prove the boot sweep, not the swap, is what finally removes it.
    let backup = fx.install.join("onnxruntime.dll.old-deadbeef");
    std::fs::write(&backup, "old native").expect("write backup");

    oximux_auto_update::boot_housekeeping(&fx.config, None);

    assert!(!backup.exists(), "the backup is swept at the next launch");
    assert_eq!(read(&fx.install.join("onnxruntime.dll")), "native 0.2.0");
    assert!(staged_dirs(&fx).is_empty(), "and the spent staging directory with it");
}

/// Staging directories are hidden siblings of the install directory.
fn staged_dirs(fx: &Fixture) -> Vec<PathBuf> {
    let parent = fx.install.parent().expect("install has a parent");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(".OxiMux.update-"))
        })
        .collect()
}
