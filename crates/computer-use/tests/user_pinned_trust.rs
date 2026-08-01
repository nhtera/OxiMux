//! The Windows trust anchor, end to end, against a real executable.
//!
//! The unit tests in `trust` cover the store in isolation. This exercises the
//! gate the way `prepare` does — [`verify::verify_pinned`] against a real file
//! on this machine — because the property that matters most is an *ordering*
//! one: the trust verdict has to come before anything executes the candidate,
//! and a store tested on its own cannot show that.
//!
//! Runs everywhere. The anchor is Windows' because Windows has no signature to
//! use, but nothing in it is Windows-specific, and a macOS host running these is
//! the only thing that would catch the gate being wired up per-platform.

use std::path::{Path, PathBuf};

use oximux_computer_use::trust::TrustStore;
use oximux_computer_use::{verify, Error};

/// A real, harmless executable, copied under the driver's name.
///
/// Real rather than a written-out stub because the point is to reach the
/// `--version` spawn: a file that cannot execute would fail the last step for
/// the wrong reason and the test would still pass.
fn stage_driver(dir: &Path) -> PathBuf {
    #[cfg(windows)]
    let source = r"C:\Windows\System32\HOSTNAME.EXE";
    #[cfg(not(windows))]
    let source = "/bin/echo";

    let staged = dir.join(format!("cua-driver{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(source, &staged).expect("copy a real executable into place");
    staged
}

fn store_in(dir: &Path) -> TrustStore {
    TrustStore::for_app_data_dir(dir)
}

#[test]
fn an_unapproved_binary_is_refused_before_it_is_ever_executed() {
    // The load-bearing ordering. `NotApproved` specifically — as opposed to
    // `UnreadableVersion`, `Timeout` or `Spawn` — is what proves `read_version`
    // was never reached, because every one of those errors can only be produced
    // by having already run the binary.
    let dir = tempfile::tempdir().expect("tempdir");
    let driver = stage_driver(dir.path());

    let err = verify::verify_pinned(&driver, &store_in(dir.path())).expect_err("must refuse");

    match err {
        Error::NotApproved { path, sha256 } => {
            assert_eq!(path, driver);
            assert_eq!(sha256.len(), 64, "the prompt needs a full digest to show");
        }
        other => panic!("expected NotApproved before execution, got {other:?}"),
    }
}

#[test]
fn approving_lets_the_gate_run_the_binary_it_now_trusts() {
    // The mirror of the test above: once approved, the gate proceeds past trust
    // and does execute. Asserted as "not a trust error" rather than as one exact
    // variant, because what a stand-in binary prints for `--version` is not this
    // module's business — only that the gate got that far.
    let dir = tempfile::tempdir().expect("tempdir");
    let driver = stage_driver(dir.path());
    let store = store_in(dir.path());

    store.approve(&driver).expect("approve");

    let err = verify::verify_pinned(&driver, &store)
        .expect_err("a stand-in binary reports no usable version");
    assert!(
        !matches!(
            err,
            Error::NotApproved { .. } | Error::TrustSuperseded { .. }
        ),
        "trust must have passed; got {err:?}"
    );
}

#[test]
fn rewriting_an_approved_binary_shuts_the_gate_again() {
    // The threat this whole anchor exists for: something replaces the approved
    // binary in place, months later, and nothing signed anything.
    let dir = tempfile::tempdir().expect("tempdir");
    let driver = stage_driver(dir.path());
    let store = store_in(dir.path());

    let pin = store.approve(&driver).expect("approve");
    std::fs::write(&driver, b"MZ a different binary entirely").expect("rewrite");

    match verify::verify_pinned(&driver, &store).expect_err("must refuse") {
        Error::TrustSuperseded {
            approved, found, ..
        } => {
            assert_eq!(approved, pin.sha256);
            assert_ne!(found, approved, "a changed binary must hash differently");
        }
        other => panic!("expected TrustSuperseded, got {other:?}"),
    }
}

#[test]
fn re_approving_after_an_update_reopens_the_gate() {
    // The routine case, not an edge one: upstream ships roughly six releases a
    // week and the driver rewrites itself in place, so this path runs far more
    // often than the first approval does. If re-approval did not work cleanly
    // the anchor would be abandoned within a week of shipping.
    let dir = tempfile::tempdir().expect("tempdir");
    let driver = stage_driver(dir.path());
    let store = store_in(dir.path());

    store.approve(&driver).expect("first approval");
    std::fs::write(&driver, b"MZ pretend this is v2").expect("update in place");
    assert!(matches!(
        verify::verify_pinned(&driver, &store),
        Err(Error::TrustSuperseded { .. })
    ));

    // Re-staging a real executable stands in for the user re-approving whatever
    // the update left behind.
    let updated = stage_driver(dir.path());
    store.approve(&updated).expect("re-approval");
    assert!(
        !matches!(
            verify::verify_pinned(&updated, &store),
            Err(Error::NotApproved { .. } | Error::TrustSuperseded { .. })
        ),
        "re-approval must clear the trust gate"
    );
}

#[test]
fn a_driver_that_was_never_installed_is_not_a_trust_failure() {
    // "Not installed" and "not approved" reach the user as different problems
    // with different fixes, and the gate must not turn the first into the
    // second by hashing a file that is not there.
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("cua-driver-absent.exe");

    let err = verify::verify_pinned(&absent, &store_in(dir.path())).expect_err("must fail");
    assert!(matches!(err, Error::Spawn { .. }), "got {err:?}");
}
