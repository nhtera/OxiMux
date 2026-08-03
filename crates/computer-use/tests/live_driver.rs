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

use oximux_computer_use::{daemon, discovery, verify, Error};
// Only the declared-invocation check reaches for it, and that is gated off on
// Windows until the safety pair lands.
#[cfg(any(not(windows), feature = "windows-screen-control"))]
use oximux_computer_use::mcp;

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

    // `verify` is the signature gate specifically, so it can only ever vouch on
    // that basis — a user-pinned verdict coming back from here would mean the
    // publisher check had been swapped out for the weaker Windows anchor.
    match &driver.basis {
        verify::TrustBasis::Signature {
            identifier,
            team_id,
            notarized,
        } => {
            assert_eq!(identifier, verify::EXPECTED_IDENTIFIER);
            assert_eq!(team_id, verify::EXPECTED_TEAM_ID);
            assert!(
                notarized,
                "expected a stapled notarization ticket on a Developer ID build"
            );
        }
        other => panic!("verify must vouch by signature, got {other:?}"),
    }
    assert!(
        driver.version >= verify::MIN_VERSION,
        "installed {} is below the {} floor",
        driver.version,
        verify::MIN_VERSION
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

/// Skipped on Windows for now: `mcp::server_spec` does not exist there until
/// the safety pair lands, so there is no declared invocation to compare.
///
/// Worth restating rather than deleting once it does. Phase 0 measured
/// `cua-driver manifest -p` on Windows recommending a bare `["mcp"]` — no
/// `--host-bundle-id` — so this is precisely the drift check that would catch
/// Phase 6 getting the Windows argv wrong.
#[cfg(any(not(windows), feature = "windows-screen-control"))]
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
fn every_tool_the_driver_registers_is_classified() {
    let Some(path) = installed() else { return };

    let out =
        oximux_computer_use::exec::run_bounded(&path, &["list-tools"], Duration::from_secs(10))
            .expect("list-tools must run");
    assert!(out.success(), "list-tools failed: {}", out.stderr);

    // The classifier fails closed, so an unlisted tool is *refused* rather than
    // waved through — safe, but it silently breaks a working feature after a
    // driver update, and the deny list cannot cover a tool it has never heard
    // of. This turns that into a visible failure at upgrade time.
    let names: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
        .collect();

    // Without this the test passes vacuously if the output format changes and
    // the parse yields nothing — which is exactly the upgrade this guards.
    assert!(
        names.len() > 30,
        "parsed only {} tool names; the `name: description` format likely changed",
        names.len()
    );
    assert!(names.contains(&"click"), "parse missed a known tool: {names:?}");

    let unclassified: Vec<String> = names
        .into_iter()
        .filter(|name| {
            matches!(
                oximux_computer_use::tools::classify(name),
                oximux_computer_use::tools::ToolClass::Forbidden(
                    oximux_computer_use::tools::Refusal::Unrecognised
                )
            )
        })
        .map(str::to_string)
        .collect();

    assert!(
        unclassified.is_empty(),
        "the driver registers tools this build has never classified — add them to \
         `tools::TOOLS` with a deliberate class: {unclassified:?}"
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
