//! Live checks against trycua's real release feed. Ignored by default —
//! network-dependent and rate-limited — run manually when validating the
//! installer against upstream:
//!
//! ```sh
//! cargo test -p oximux-computer-use --test install_live_feed -- --ignored
//! ```

use oximux_computer_use::install::{self, release_feed};

/// The full pipeline against the real release feed: downloads, verifies
/// gates, and installs the actual driver. Run deliberately — it changes the
/// machine:
///
/// ```sh
/// cargo test -p oximux-computer-use --test install_live_feed -- --ignored the_live_install
/// ```
///
/// Runs on both platforms, because the point of one pipeline is that one test
/// covers it. What differs is the gate, and the test asserts that difference
/// rather than hiding it: macOS must come back publisher-signed and notarized,
/// Windows must ask for approval — which this test grants against a throwaway
/// trust store, never the user's real one — and come back user-pinned.
#[test]
#[ignore = "downloads and installs the real driver"]
fn the_live_install_pipeline_installs_a_verified_driver() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use oximux_computer_use::install::{self, InstallEvent};

    let pins = tempfile::tempdir().expect("tempdir");
    let cancel = Arc::new(AtomicBool::new(false));
    let (events, join) =
        install::spawn_install(cancel, anchor(&pins)).expect("no install already running");

    // Drain so the channel never backs up; the join result is authoritative.
    let mut stages = 0u32;
    let mut asked_for_approval = false;
    for event in events {
        match event {
            InstallEvent::Stage(_) => stages += 1,
            InstallEvent::NeedsApproval { sha256, .. } => {
                assert_eq!(sha256.len(), 64, "a sha256 is 64 hex digits");
                asked_for_approval = true;
                install::approve();
            }
            _ => {}
        }
    }
    let driver = join
        .join()
        .expect("install thread must not panic")
        .expect("pipeline must succeed against the live feed");

    assert!(stages >= 3, "expected resolve/download/verify/install stages");
    assert!(
        driver.path.exists(),
        "verified driver must exist at {}",
        driver.path.display()
    );
    assert_basis(&driver, asked_for_approval);

    // And the normal discovery+gate path must now agree.
    assert_eq!(resolve(&pins).version, driver.version);
}

/// The trust anchor the install runs against: nothing on macOS, a throwaway
/// store on Windows. A test must never pin bytes into the user's real one.
#[cfg(windows)]
fn anchor(pins: &tempfile::TempDir) -> oximux_computer_use::install::Anchor {
    oximux_computer_use::TrustStore::at(pins.path().join("pins.json"))
}

#[cfg(not(windows))]
fn anchor(_pins: &tempfile::TempDir) -> oximux_computer_use::install::Anchor {
    oximux_computer_use::install::Anchor
}

/// The gate that actually ran, asserted per platform.
#[cfg(windows)]
fn assert_basis(driver: &oximux_computer_use::VerifiedDriver, asked_for_approval: bool) {
    use oximux_computer_use::TrustBasis;
    assert!(
        asked_for_approval,
        "Windows has no publisher to check — the install must ask"
    );
    assert!(
        matches!(driver.basis, TrustBasis::UserPinned { .. }),
        "installed driver must be user-pinned, got {:?}",
        driver.basis
    );
}

#[cfg(not(windows))]
fn assert_basis(driver: &oximux_computer_use::VerifiedDriver, asked_for_approval: bool) {
    use oximux_computer_use::TrustBasis;
    assert!(
        !asked_for_approval,
        "macOS gates on the signature and must never ask a person"
    );
    assert!(
        matches!(driver.basis, TrustBasis::Signature { notarized: true, .. }),
        "installed driver must carry a ticket, got {:?}",
        driver.basis
    );
}

#[cfg(windows)]
fn resolve(pins: &tempfile::TempDir) -> oximux_computer_use::VerifiedDriver {
    oximux_computer_use::prepare(&anchor(pins)).expect("prepare must find the new install")
}

#[cfg(not(windows))]
fn resolve(_pins: &tempfile::TempDir) -> oximux_computer_use::VerifiedDriver {
    oximux_computer_use::prepare().expect("prepare must find the new install")
}

#[test]
#[ignore = "hits the live GitHub API"]
fn the_live_feed_yields_a_driver_release_with_both_assets() {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(30))
        .user_agent("OxiMux-live-feed-test")
        .build();
    let body = agent
        .get(release_feed::RELEASES_URL)
        .call()
        .expect("release feed reachable")
        .into_string()
        .expect("feed body");

    let release = release_feed::parse_latest(&body, install::platform::asset_name)
        .expect("a published driver release");
    assert!(
        release.version >= oximux_computer_use::verify::MIN_VERSION,
        "latest driver {} older than integration floor {}",
        release.version,
        oximux_computer_use::verify::MIN_VERSION
    );
    assert!(release.archive.browser_download_url.starts_with("https://"));
    assert!(release.checksums.name == "checksums.txt");
}
