//! Live checks against trycua's real release feed. Ignored by default —
//! network-dependent and rate-limited — run manually when validating the
//! installer against upstream:
//!
//! ```sh
//! cargo test -p oximux-computer-use --test install_live_feed -- --ignored
//! ```

use oximux_computer_use::install::release_feed;

/// The full pipeline against the real release feed: downloads, verifies
/// (publisher + notarization), and installs the actual driver to an install
/// root. Run deliberately — it changes the machine:
///
/// ```sh
/// cargo test -p oximux-computer-use --test install_live_feed -- --ignored the_live_install
/// ```
///
/// macOS only, and that is the design rather than a porting gap: the pipeline
/// downloads a notarized `.app` and gates it on a Developer ID signature, none
/// of which the unsigned Windows artifacts have. On Windows the user installs
/// the driver themselves and approves the bytes — see `oximux_computer_use::trust`.
#[cfg(not(windows))]
#[test]
#[ignore = "downloads and installs the real CuaDriver.app"]
fn the_live_install_pipeline_installs_a_verified_driver() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use oximux_computer_use::install::{self, InstallEvent};
    use oximux_computer_use::TrustBasis;

    let cancel = Arc::new(AtomicBool::new(false));
    let (events, join) = install::spawn_install(cancel).expect("no install already running");

    // Drain so the channel never backs up; the join result is authoritative.
    let mut stages = 0u32;
    for event in events {
        if matches!(event, InstallEvent::Stage(_)) {
            stages += 1;
        }
    }
    let driver = join
        .join()
        .expect("install thread must not panic")
        .expect("pipeline must succeed against the live feed");

    assert!(stages >= 3, "expected resolve/download/verify/install stages");
    assert!(
        matches!(driver.basis, TrustBasis::Signature { notarized: true, .. }),
        "installed driver must carry a ticket, got {:?}",
        driver.basis
    );
    assert!(
        driver.path.exists(),
        "verified driver must exist at {}",
        driver.path.display()
    );

    // And the normal discovery+gate path must now agree.
    let resolved = oximux_computer_use::prepare().expect("prepare must find the new install");
    assert_eq!(resolved.version, driver.version);
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

    let release = release_feed::parse_latest(&body).expect("a published driver release");
    assert!(
        release.version >= oximux_computer_use::verify::MIN_VERSION,
        "latest driver {} older than integration floor {}",
        release.version,
        oximux_computer_use::verify::MIN_VERSION
    );
    assert!(release.tarball.browser_download_url.starts_with("https://"));
    assert!(release.checksums.name == "checksums.txt");
}
