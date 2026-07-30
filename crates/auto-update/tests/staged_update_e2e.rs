//! End-to-end proof of the update lifecycle against *real* signed bundles,
//! a real DMG, a real mount, and a real HTTP feed.
//!
//! Everything the unit tests mock out is genuine here: `codesign` signs the
//! fixtures, `hdiutil` builds and mounts the image, and the pipeline runs
//! unmodified. What is faked is only the environment — a scratch directory
//! standing in for `/Applications`, and a one-shot HTTP server standing in
//! for the GitHub release feed.
//!
//! # Why it is `#[ignore]`d
//!
//! It needs a code-signing identity in the login keychain, which CI does not
//! have. Run it locally with:
//!
//! ```text
//! cargo test -p oximux-auto-update --test staged_update_e2e -- --ignored --nocapture
//! ```
//!
//! `spctl` is skipped via the debug-only knob: these fixtures are signed but
//! not notarized, and notarization is a network round-trip to Apple measured
//! in minutes. Every other gate runs for real.

#![cfg(target_os = "macos")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use oximux_auto_update::staging::{apply_pending, PendingUpdate, SwapOutcome};
use oximux_auto_update::{
    CheckTrigger, InstalledApp, SignaturePolicy, UpdateStatus, UpdaterConfig, Version,
};

/// First identity `codesign` will accept. Both fixtures are signed with it,
/// so the updater's pin (taken from the "installed" bundle) matches the
/// "update" — exactly the real-world relationship between two releases.
fn signing_identity() -> Option<String> {
    let out = Command::new("/usr/bin/security")
        .args(["find-identity", "-v", "-p", "codesigning"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| line.split('"').nth(1).map(str::to_string))
}

/// A minimal but structurally real `.app`: an executable in `Contents/MacOS`
/// and an `Info.plist` carrying the version the updater reads.
fn make_bundle(path: &Path, identifier: &str, version: &str, identity: &str) {
    std::fs::create_dir_all(path.join("Contents/MacOS")).expect("mkdir");
    std::fs::copy("/bin/echo", path.join("Contents/MacOS/OxiMux")).expect("copy exe");
    std::fs::write(
        path.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <plist version=\"1.0\"><dict>\
             <key>CFBundleIdentifier</key><string>{identifier}</string>\
             <key>CFBundleExecutable</key><string>OxiMux</string>\
             <key>CFBundleShortVersionString</key><string>{version}</string>\
             </dict></plist>"
        ),
    )
    .expect("plist");

    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", identity, "--identifier", identifier])
        .arg(path)
        .status()
        .expect("codesign runs");
    assert!(status.success(), "codesign failed for {}", path.display());
}

/// Pack `app` into a real read-only DMG, the same artifact shape the release
/// workflow publishes.
fn make_dmg(app: &Path, out: &Path) {
    let staging = out.with_extension("srcdir");
    std::fs::create_dir_all(&staging).expect("mkdir");
    let status = Command::new("/usr/bin/ditto")
        .arg(app)
        .arg(staging.join("OxiMux.app"))
        .status()
        .expect("ditto runs");
    assert!(status.success(), "ditto into dmg staging failed");

    let status = Command::new("/usr/bin/hdiutil")
        .args(["create", "-quiet", "-fs", "HFS+", "-format", "UDZO", "-srcfolder"])
        .arg(&staging)
        .arg("-volname")
        .arg("OxiMux")
        .arg(out)
        .status()
        .expect("hdiutil runs");
    assert!(status.success(), "hdiutil create failed");
    let _ = std::fs::remove_dir_all(&staging);
}

/// A throwaway HTTP server that answers exactly two paths: the release feed
/// and the DMG. Runs until the test drops it.
struct FeedServer {
    port: u16,
    _thread: std::thread::JoinHandle<()>,
}

impl FeedServer {
    fn start(dmg: PathBuf, version: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let body = std::fs::read(&dmg).expect("read dmg");
        let asset = format!("OxiMux-{version}-macos-arm64.dmg");
        let feed = format!(
            "{{\"tag_name\":\"v{version}\",\"body\":\"E2E release\",\"assets\":[\
               {{\"name\":\"{asset}\",\
                 \"browser_download_url\":\"http://127.0.0.1:{port}/{asset}\",\
                 \"size\":{}}}]}}",
            body.len()
        );

        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let payload: &[u8] = if req.contains(".dmg") {
                    &body
                } else {
                    feed.as_bytes()
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    )
                    .as_bytes(),
                );
                let _ = stream.write_all(payload);
                let _ = stream.flush();
            }
        });
        Self {
            port,
            _thread: thread,
        }
    }

    fn feed_url(&self) -> String {
        format!("http://127.0.0.1:{}/releases/latest", self.port)
    }
}

fn bundle_version(app: &Path) -> String {
    let out = Command::new("/usr/bin/defaults")
        .arg("read")
        .arg(app.join("Contents/Info"))
        .arg("CFBundleShortVersionString")
        .output()
        .expect("defaults runs");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[ignore = "needs a codesigning identity; run locally with --ignored"]
fn an_update_stages_without_touching_the_running_bundle_then_swaps_at_quit() {
    let Some(identity) = signing_identity() else {
        panic!("no codesigning identity available — this test cannot prove anything without one");
    };
    // SAFETY: single-threaded test setup; no other thread reads env yet.
    unsafe { std::env::set_var("OXIMUX_UPDATE_SKIP_SPCTL", "1") };

    let root = tempfile::tempdir().expect("tempdir");
    let apps = root.path().join("Applications");
    std::fs::create_dir_all(&apps).expect("mkdir");
    let cache = root.path().join("cache");
    let manifest = root.path().join("data/pending-update.json");
    let sentinel = root.path().join("data/update-pending-verify");
    const ID: &str = "dev.nhtera.oximux.e2e";

    // The installed app, and the newer one a release would publish.
    let installed = apps.join("OxiMux.app");
    make_bundle(&installed, ID, "0.1.3", &identity);
    let update_src = root.path().join("build/OxiMux.app");
    make_bundle(&update_src, ID, "0.2.0", &identity);
    let dmg = root.path().join("OxiMux-0.2.0-macos-arm64.dmg");
    make_dmg(&update_src, &dmg);

    let server = FeedServer::start(dmg, "0.2.0".into());
    unsafe { std::env::set_var("OXIMUX_UPDATE_FEED_URL", server.feed_url()) };

    let pin = oximux_macos_trust::read_signature(&installed).expect("installed bundle is signed");
    assert!(
        pin.pinnable(),
        "a signed fixture must yield a pinnable identity, got {pin:?}"
    );
    let config = UpdaterConfig {
        current_version: Version::new(0, 1, 3),
        app: InstalledApp {
            bundle_root: installed.clone(),
            pin: SignaturePolicy {
                identifier: pin.identifier.clone(),
                team_id: pin.team_id.clone(),
            },
        },
        cache_dir: cache.clone(),
        manifest_path: manifest.clone(),
    };

    // --- Background pass: download, mount, stage, verify. ---
    let (rx, handle) = oximux_auto_update::spawn_check(
        config.clone(),
        CheckTrigger::Manual,
        Arc::new(AtomicBool::new(false)),
    )
    .expect("check starts");
    let terminal = handle.join().expect("pipeline thread");
    let seen: Vec<UpdateStatus> = rx.try_iter().collect();

    match &terminal {
        UpdateStatus::Ready { version, .. } => assert_eq!(version, "0.2.0"),
        other => panic!("expected Ready, got {other:?} (statuses seen: {seen:?})"),
    }

    // THE invariant this whole design exists for: nothing was swapped while
    // the app is "running". A subprocess resolved from the bundle path right
    // now still gets the old version.
    assert_eq!(
        bundle_version(&installed),
        "0.1.3",
        "the installed bundle must still be the running version until quit"
    );

    let pending = PendingUpdate::load(&manifest).expect("manifest written");
    assert_eq!(pending.version, "0.2.0");
    assert!(pending.staged_path.is_dir(), "staged bundle must exist");
    assert_eq!(bundle_version(&pending.staged_path), "0.2.0");

    // No DMG or mountpoint left behind.
    let downloads = cache.join("updates");
    let leftover_dmgs: Vec<_> = std::fs::read_dir(&downloads)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "dmg"))
        .collect();
    assert!(
        leftover_dmgs.is_empty(),
        "downloaded DMG must be cleaned up, found {leftover_dmgs:?}"
    );

    // A second check while one is not running is fine; the guard is about
    // concurrency, and the staged update short-circuits the network.
    let (rx2, handle2) = oximux_auto_update::spawn_check(
        config.clone(),
        CheckTrigger::Background,
        Arc::new(AtomicBool::new(false)),
    )
    .expect("second check starts");
    drop(rx2);
    assert!(
        matches!(handle2.join().expect("thread"), UpdateStatus::Ready { .. }),
        "a staged update must be re-offered without re-downloading"
    );

    // --- Quit pass: the swap. ---
    assert_eq!(
        apply_pending(&config, Some(&sentinel)),
        SwapOutcome::Applied
    );
    assert_eq!(
        bundle_version(&installed),
        "0.2.0",
        "the installed bundle must be the new version after the quit swap"
    );
    assert!(!manifest.exists(), "manifest must be cleared after applying");
    assert!(!sentinel.exists(), "sentinel must be cleared after applying");
    assert!(
        !pending.staged_path.exists(),
        "the swapped-out bundle must be removed"
    );

    // Nothing staged: applying again is a no-op rather than an error.
    assert_eq!(apply_pending(&config, Some(&sentinel)), SwapOutcome::Nothing);

    unsafe {
        std::env::remove_var("OXIMUX_UPDATE_SKIP_SPCTL");
        std::env::remove_var("OXIMUX_UPDATE_FEED_URL");
    }
}

#[test]
#[ignore = "needs a codesigning identity; run locally with --ignored"]
fn a_staged_bundle_tampered_before_quit_is_refused_and_the_old_version_survives() {
    let Some(identity) = signing_identity() else {
        panic!("no codesigning identity available — this test cannot prove anything without one");
    };

    let root = tempfile::tempdir().expect("tempdir");
    let apps = root.path().join("Applications");
    std::fs::create_dir_all(&apps).expect("mkdir");
    let manifest = root.path().join("data/pending-update.json");
    let sentinel = root.path().join("data/update-pending-verify");
    const ID: &str = "dev.nhtera.oximux.e2e";

    let installed = apps.join("OxiMux.app");
    make_bundle(&installed, ID, "0.1.3", &identity);

    // A staged update that passed verification when it was staged...
    let staged = apps.join(".OxiMux.update-tamper.app");
    make_bundle(&staged, ID, "0.2.0", &identity);
    let pin = oximux_macos_trust::read_signature(&installed).expect("signed");
    let config = UpdaterConfig {
        current_version: Version::new(0, 1, 3),
        app: InstalledApp {
            bundle_root: installed.clone(),
            pin: SignaturePolicy {
                identifier: pin.identifier.clone(),
                team_id: pin.team_id.clone(),
            },
        },
        cache_dir: root.path().join("cache"),
        manifest_path: manifest.clone(),
    };
    PendingUpdate {
        staged_path: staged.clone(),
        version: "0.2.0".into(),
        notes: String::new(),
    }
    .store(&manifest)
    .expect("manifest");

    // ...and was then modified while it sat waiting for the user to quit.
    std::fs::write(staged.join("Contents/MacOS/OxiMux"), b"#!/bin/sh\necho pwned\n")
        .expect("tamper");

    assert_eq!(
        apply_pending(&config, Some(&sentinel)),
        SwapOutcome::Refused,
        "a tampered staged bundle must never be swapped in"
    );
    assert_eq!(
        bundle_version(&installed),
        "0.1.3",
        "the installed version must be untouched"
    );
    assert!(!staged.exists(), "the rejected bundle must be discarded");
    assert!(!manifest.exists(), "the manifest must be cleared");
    assert!(!sentinel.exists(), "the sentinel must not be left behind");
}
