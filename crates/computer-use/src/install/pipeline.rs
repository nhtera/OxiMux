//! The blocking install pipeline: resolve → download → gate → place.
//!
//! Every step here is the same on every platform. The four that are not live in
//! [`super::platform`], behind identical signatures — which is what keeps this
//! file free of `cfg` islands and keeps the two platforms from drifting into
//! two different installers.
//!
//! Network I/O is sync `ureq` with explicit connect/read timeouts — a stalled
//! peer surfaces as an error instead of pinning the thread past a cancel (the
//! cancel flag is only checkable between reads). Runs on the dedicated thread
//! `spawn_install` creates; never on a UI thread.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::platform::{self, Anchor, Gate};
use super::release_feed::{self, DriverRelease, RELEASES_URL};
use super::{Decision, InstallError, InstallEvent, InstallStage};
use crate::exec;
use crate::verify::VerifiedDriver;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-`read()` ceiling, not whole-transfer: a healthy slow link keeps making
/// progress, a dead one errors out within this window.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub's API refuses requests with no User-Agent.
const USER_AGENT: &str = "OxiMux-driver-install";

/// Ceiling for the post-install `telemetry disable` run. Generous: it is the
/// driver's first execution from its new home, which on Windows means Defender
/// may be reading 28 MB before `main` starts.
const TELEMETRY_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn run(
    cancel: &AtomicBool,
    anchor: &Anchor,
    emit: &dyn Fn(InstallEvent),
) -> Result<VerifiedDriver, InstallError> {
    let stage = |stage: InstallStage| emit(InstallEvent::Stage(stage));

    stage(InstallStage::Resolving);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();

    let release = release_feed::parse_latest(
        &fetch_text(&agent, RELEASES_URL)?,
        platform::asset_name,
    )?;
    guard_downgrade(Some(release.version))?;
    check_cancel(cancel)?;

    let workdir = tempfile::tempdir().map_err(|err| InstallError::Install {
        detail: format!("could not create a staging dir: {err}"),
    })?;
    let checksums = fetch_text(&agent, &release.checksums.browser_download_url)?;
    let archive = workdir.path().join(&release.archive.name);
    let digest = download(&agent, &release, &archive, cancel, &stage)?;

    let expected = release_feed::expected_sha256(&checksums, &release.archive.name).ok_or_else(
        || InstallError::Feed {
            detail: format!("checksums.txt has no entry for {}", release.archive.name),
        },
    )?;
    if digest != expected {
        return Err(InstallError::ChecksumMismatch {
            asset: release.archive.name.clone(),
        });
    }

    let staged = platform::extract(&archive, workdir.path(), release.version)?;

    stage(InstallStage::Verifying);
    // Gate before place, on every platform. Nothing lands where `discovery`
    // will find it until the gate has answered — on macOS that answer comes
    // from Apple's signature, on Windows from a person. See `platform`.
    match platform::gate(&staged, anchor)? {
        // The binary's own version is exact where the feed tag was a claim.
        Gate::Passed(driver) => guard_downgrade(Some(driver.version))?,
        Gate::NeedsApproval {
            sha256,
            version,
            bytes,
        } => {
            stage(InstallStage::AwaitingApproval);
            emit(InstallEvent::NeedsApproval {
                sha256,
                version,
                bytes,
            });
            // No exact-version re-check on this path: reading the version means
            // executing the binary, and nothing may execute what has not been
            // approved. The claimed version was already guarded above.
            match super::await_decision(cancel)? {
                Decision::Approved => platform::record_approval(&staged, anchor)?,
                // The user answering "no" is not a failure.
                Decision::Declined => return Err(InstallError::Cancelled),
            }
        }
    }
    check_cancel(cancel)?;

    stage(InstallStage::Installing);
    let placement = platform::place(&staged)?;

    // Prove what landed is what was gated; only then discard the previous
    // install. On failure, put the old driver back — never leave neither.
    match platform::verify_placed(&placement.binary(), anchor) {
        Ok(driver) => {
            placement.commit();
            disable_telemetry(&driver.path);
            Ok(driver)
        }
        Err(err) => {
            placement.roll_back();
            Err(err.into())
        }
    }
}

/// Turn off the driver's own product telemetry.
///
/// Upstream ships it **on** by default, and merely running the binary writes a
/// persistent id under `~/.cua-driver`. A user who installs the driver
/// themselves has at least chosen that; a user who clicks Install in OxiMux has
/// not, and enrolling them silently in a third party's analytics is not
/// OxiMux's call to make on their behalf. The pane says this happens.
///
/// Best effort on purpose: an installed, verified driver that refused this call
/// is still an installed, verified driver, and failing the install over it
/// would trade a working feature for a preference. Runs only after the gate has
/// passed and the binary is placed — before that, executing it is exactly what
/// [`crate::trust`] forbids.
fn disable_telemetry(binary: &Path) {
    match exec::run_bounded(binary, &["telemetry", "disable"], TELEMETRY_TIMEOUT) {
        Ok(output) if output.success() => {}
        Ok(output) => tracing::warn!(
            detail = output.stderr.lines().next().unwrap_or(""),
            "driver install: could not turn off the driver's telemetry"
        ),
        Err(err) => tracing::warn!(
            %err,
            "driver install: could not turn off the driver's telemetry"
        ),
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), InstallError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(InstallError::Cancelled);
    }
    Ok(())
}

/// A release older than what is already installed is refused even when validly
/// signed — the feed is publish-date ordered and identity gates alone cannot
/// stop a re-published old build.
///
/// `candidate` is an `Option` because one platform cannot always produce an
/// exact version to compare: on Windows the only version available before
/// approval is the one the feed claimed, since reading the real one means
/// running an unapproved binary. A `None` skips rather than inventing a
/// comparison.
fn guard_downgrade(candidate: Option<crate::Version>) -> Result<(), InstallError> {
    let (Some(candidate), Some(installed)) = (candidate, platform::installed_version()) else {
        return Ok(());
    };
    if candidate < installed {
        return Err(InstallError::Downgrade {
            staged: candidate,
            installed,
        });
    }
    Ok(())
}

/// Small text bodies (the release feed, checksums.txt). Capped: neither is
/// legitimately more than a few hundred KB.
fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String, InstallError> {
    let mut body = String::new();
    get(agent, url)?
        .into_reader()
        .take(4 * 1024 * 1024)
        .read_to_string(&mut body)
        .map_err(|err| InstallError::Network {
            detail: format!("reading {url}: {err}"),
        })?;
    Ok(body)
}

/// Stream the archive to disk, hashing as it lands so the checksum needs no
/// second pass. Cancel is honored per chunk.
fn download(
    agent: &ureq::Agent,
    release: &DriverRelease,
    dest: &Path,
    cancel: &AtomicBool,
    emit: &dyn Fn(InstallStage),
) -> Result<String, InstallError> {
    let response = get(agent, &release.archive.browser_download_url)?;
    let total = (release.archive.size > 0).then_some(release.archive.size).or_else(|| {
        response
            .header("Content-Length")
            .and_then(|len| len.parse().ok())
    });

    // Byte ceiling: the read timeout bounds duration, this bounds volume — a
    // response that keeps streaming past any plausible driver size must not
    // fill the disk. Double the declared size allows for header/size drift.
    let ceiling = total
        .map(|bytes| bytes.saturating_mul(2))
        .unwrap_or(1024 * 1024 * 1024);

    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|err| InstallError::Install {
        detail: format!("creating {}: {err}", dest.display()),
    })?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        check_cancel(cancel)?;
        if received > ceiling {
            return Err(InstallError::Network {
                detail: format!(
                    "{} exceeded its expected size ({received} bytes)",
                    release.archive.name
                ),
            });
        }
        let n = reader.read(&mut buf).map_err(|err| InstallError::Network {
            detail: format!("downloading {}: {err}", release.archive.name),
        })?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|err| InstallError::Install {
            detail: format!("writing {}: {err}", dest.display()),
        })?;
        hasher.update(&buf[..n]);
        received += n as u64;
        emit(InstallStage::Downloading { received, total });
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn get(agent: &ureq::Agent, url: &str) -> Result<ureq::Response, InstallError> {
    match agent.get(url).call() {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(403 | 429, _)) => Err(InstallError::RateLimited),
        Err(err) => Err(InstallError::Network {
            detail: err.to_string(),
        }),
    }
}
