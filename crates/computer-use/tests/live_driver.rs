//! Checks that run against a real installed `cua-driver`.
//!
//! Every test here skips cleanly when no driver is installed, because that is
//! the normal state on CI and for contributors who never enable screen
//! control. The unit tests cover the parsing and policy logic; these exist to
//! catch the thing unit tests structurally cannot — upstream changing the CLI
//! contract this crate is written against.
//!
//! Nothing here starts the daemon. `status` is read-only, and `mcp` is only
//! *described*, never spawned: spawning it would auto-launch the daemon as a
//! side effect of running the test suite.

use std::time::Duration;

use oximux_computer_use::{daemon, discovery, mcp, verify, Error};

/// Locate the driver, or return `None` so the caller can skip.
fn installed() -> Option<std::path::PathBuf> {
    match discovery::locate() {
        Ok(path) => Some(path),
        Err(Error::NotFound { .. }) => {
            eprintln!("skipping: no cua-driver installed");
            None
        }
        Err(err) => panic!("unexpected discovery failure: {err}"),
    }
}

#[test]
fn a_real_driver_passes_every_gate() {
    let Some(path) = installed() else { return };

    let driver = verify::verify(&path).expect("installed driver must pass verification");

    assert_eq!(driver.identifier, verify::EXPECTED_IDENTIFIER);
    assert_eq!(driver.team_id, verify::EXPECTED_TEAM_ID);
    assert!(
        driver.version >= verify::MIN_VERSION,
        "installed {} is below the {} floor",
        driver.version,
        verify::MIN_VERSION
    );
    assert!(
        driver.notarized,
        "expected a stapled notarization ticket on a Developer ID build"
    );
    assert_eq!(driver.sha256.len(), 64, "sha256 must be a full hex digest");
}

#[test]
fn discovery_prefers_the_signed_app_bundle_over_the_path_symlink() {
    let Some(path) = installed() else { return };
    // The installer puts a symlink on PATH pointing into the bundle. Landing on
    // the bundle is what makes the signature check meaningful.
    assert!(
        !path.is_symlink(),
        "locate() returned an unresolved symlink: {}",
        path.display()
    );
}

#[test]
fn an_unrelated_signed_binary_is_rejected() {
    // Proves the gate is provenance, not merely "is it signed". /bin/echo is
    // Apple-signed and passes integrity verification, but is not this
    // publisher's binary.
    if !std::path::Path::new("/bin/echo").exists() {
        return;
    }
    let err = verify::verify(std::path::Path::new("/bin/echo"))
        .expect_err("an Apple binary must not pass as the driver");
    assert!(
        matches!(
            err,
            Error::UnexpectedIdentifier { .. } | Error::UnexpectedTeamId { .. }
        ),
        "expected an identity rejection, got {err:?}"
    );
}

#[test]
fn status_is_readable_and_bounded() {
    let Some(path) = installed() else { return };

    let started = std::time::Instant::now();
    let state = daemon::status(&path, daemon::STATUS_TIMEOUT).expect("status must be readable");
    assert!(
        started.elapsed() < daemon::STATUS_TIMEOUT,
        "status outran its own ceiling"
    );
    // Either verdict is fine — the daemon is started on demand by the driver,
    // not by OxiMux. What must not happen is an unparsed verdict.
    assert!(
        !matches!(state, daemon::DaemonState::Unknown { .. }),
        "could not parse daemon status: {state:?}"
    );
}

#[test]
fn the_declared_server_matches_the_drivers_own_recommended_invocation() {
    let Some(path) = installed() else { return };

    // `cua-driver mcp-config` prints the invocation upstream recommends. If a
    // future release changes it, this catches the drift here rather than as a
    // silently broken tool surface inside an agent session.
    let out = oximux_computer_use::exec::run_bounded(
        &path,
        &["mcp-config"],
        Duration::from_secs(10),
    )
    .expect("mcp-config must run");
    assert!(out.success(), "mcp-config failed: {}", out.stderr);

    let recommended: serde_json::Value =
        serde_json::from_str(&out.stdout).expect("mcp-config must emit JSON");
    let entry = recommended["mcpServers"]
        .as_object()
        .and_then(|servers| servers.values().next())
        .expect("one recommended server");

    let spec = mcp::server_spec(&path);
    assert_eq!(
        entry["args"][0], spec.args[0],
        "upstream no longer recommends `{}` as the subcommand",
        spec.args[0]
    );
    assert_eq!(
        entry["command"].as_str().map(std::path::PathBuf::from),
        Some(path.clone()),
        "upstream points at a different executable than discovery found"
    );
}

#[test]
fn the_driver_still_offers_the_tools_the_integration_assumes() {
    let Some(path) = installed() else { return };

    let out =
        oximux_computer_use::exec::run_bounded(&path, &["list-tools"], Duration::from_secs(10))
            .expect("list-tools must run");
    assert!(out.success(), "list-tools failed: {}", out.stderr);

    // The session primitives this crate's cleanup depends on, plus the input
    // actions a later policy gate will have to recognise.
    for tool in [
        "start_session",
        "end_session",
        "click",
        "type_text",
        "press_key",
    ] {
        assert!(
            out.stdout.contains(&format!("{tool}:")),
            "driver no longer registers `{tool}`"
        );
    }
}
