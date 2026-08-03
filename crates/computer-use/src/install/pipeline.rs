//! The blocking install pipeline: resolve → download → gate → place.
//!
//! Network I/O is sync `ureq` with explicit connect/read timeouts — a stalled
//! peer surfaces as an error instead of pinning the thread past a cancel (the
//! cancel flag is only checkable between reads). Runs on the dedicated thread
//! `spawn_install` creates; never on a UI thread.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use super::release_feed::{self, DriverRelease, RELEASES_URL};
use super::{place, InstallError, InstallStage};
use crate::verify::{self, VerifiedDriver};
use crate::exec;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-`read()` ceiling, not whole-transfer: a healthy slow link keeps making
/// progress, a dead one errors out within this window.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const TAR_TIMEOUT: Duration = Duration::from_secs(120);

/// GitHub's API refuses requests with no User-Agent.
const USER_AGENT: &str = "OxiMux-driver-install";

pub(super) const APP_NAME: &str = "CuaDriver.app";
const APP_BINARY: &str = "Contents/MacOS/cua-driver";

pub(super) fn run(
    cancel: &AtomicBool,
    emit: &dyn Fn(InstallStage),
) -> Result<VerifiedDriver, InstallError> {
    emit(InstallStage::Resolving);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .build();

    let release = release_feed::parse_latest(&fetch_text(&agent, RELEASES_URL)?)?;
    guard_downgrade(release.version)?;
    check_cancel(cancel)?;

    let workdir = tempfile::tempdir().map_err(|err| InstallError::Install {
        detail: format!("could not create a staging dir: {err}"),
    })?;
    let checksums = fetch_text(&agent, &release.checksums.browser_download_url)?;
    let tarball = workdir.path().join(&release.tarball.name);
    let digest = download(&agent, &release, &tarball, cancel, emit)?;

    let expected = release_feed::expected_sha256(&checksums, &release.tarball.name).ok_or_else(
        || InstallError::Feed {
            detail: format!("checksums.txt has no entry for {}", release.tarball.name),
        },
    )?;
    if digest != expected {
        return Err(InstallError::ChecksumMismatch {
            asset: release.tarball.name.clone(),
        });
    }

    extract(&tarball, workdir.path())?;
    let staged_app = find_app(workdir.path())?;

    emit(InstallStage::Verifying);
    // Identity gates first (codesign, no execution beyond verify.rs's bounded
    // `--version`), then Gatekeeper's own bundle-level notarization verdict.
    let staged = verify::verify(&staged_app.join(APP_BINARY))?;
    verify::verify_notarized_bundle(&staged_app)?;
    // The binary's own version is exact where the feed tag was a claim.
    guard_downgrade(staged.version)?;
    check_cancel(cancel)?;

    emit(InstallStage::Installing);
    let swap = place::swap_in(&staged_app)?;

    // Prove what landed is what was gated; only then discard the previous
    // install. On failure, put the old driver back — never leave neither.
    match verify::verify(&swap.target.join(APP_BINARY)) {
        Ok(driver) => {
            swap.commit();
            Ok(driver)
        }
        Err(err) => {
            swap.roll_back();
            Err(err.into())
        }
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), InstallError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(InstallError::Cancelled);
    }
    Ok(())
}

/// The version of the driver already installed, if one can be trusted enough to
/// read a version off.
#[cfg(not(windows))]
fn installed_version() -> Option<crate::Version> {
    match crate::prepare() {
        Ok(driver) => Some(driver.version),
        // A too-old install still has a readable version; upgrading over it is
        // the whole point. Any other state (missing, broken signature) has no
        // trustworthy version to compare against.
        Err(crate::Error::DriverTooOld { found, .. }) => Some(found),
        Err(_) => None,
    }
}

/// Windows has no in-app installer, so there is never an OxiMux-managed install
/// to downgrade.
///
/// This module downloads and stages a notarized macOS `.app`; none of that has a
/// Windows meaning. Under the user-anchored trust model the user installs the
/// driver by whatever route they trust and approves the bytes
/// (see [`crate::trust`]), which is also why `prepare` cannot be called here —
/// on Windows it requires a trust store, and this module has no business
/// deciding whose approval to consult.
#[cfg(windows)]
fn installed_version() -> Option<crate::Version> {
    None
}

/// A release older than what is already installed is refused even when validly
/// signed — the feed is publish-date ordered and identity gates alone cannot
/// stop a re-published old build.
fn guard_downgrade(candidate: crate::Version) -> Result<(), InstallError> {
    match installed_version() {
        Some(installed) if candidate < installed => Err(InstallError::Downgrade {
            staged: candidate,
            installed,
        }),
        _ => Ok(()),
    }
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

/// Stream the tarball to disk, hashing as it lands so the checksum needs no
/// second pass. Cancel is honored per chunk.
fn download(
    agent: &ureq::Agent,
    release: &DriverRelease,
    dest: &Path,
    cancel: &AtomicBool,
    emit: &dyn Fn(InstallStage),
) -> Result<String, InstallError> {
    let response = get(agent, &release.tarball.browser_download_url)?;
    let total = (release.tarball.size > 0).then_some(release.tarball.size).or_else(|| {
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
                    release.tarball.name
                ),
            });
        }
        let n = reader.read(&mut buf).map_err(|err| InstallError::Network {
            detail: format!("downloading {}: {err}", release.tarball.name),
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

/// `/usr/bin/tar` by absolute path — no PATH lookup for anything that touches
/// the downloaded artifact.
fn extract(tarball: &Path, into: &Path) -> Result<(), InstallError> {
    let out = exec::run_bounded(
        Path::new("/usr/bin/tar"),
        &[
            "-xzf",
            &tarball.display().to_string(),
            "-C",
            &into.display().to_string(),
        ],
        TAR_TIMEOUT,
    )?;
    if out.success() {
        return Ok(());
    }
    Err(InstallError::Install {
        detail: format!("tar failed: {}", out.stderr.lines().next().unwrap_or("")),
    })
}

/// Locate `CuaDriver.app` in the extracted tree without hardcoding the staging
/// dir name (upstream layout may drift). Two levels is generous for a tarball
/// whose contract is `<staging-dir>/CuaDriver.app`.
fn find_app(root: &Path) -> Result<PathBuf, InstallError> {
    fn scan(dir: &Path, depth: u8, seen: &mut Vec<String>) -> Option<PathBuf> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|name| name == APP_NAME) {
                return Some(path);
            }
            seen.push(entry.file_name().to_string_lossy().into_owned());
            if depth > 0
                && let Some(found) = scan(&path, depth - 1, seen)
            {
                return Some(found);
            }
        }
        None
    }

    let mut seen = Vec::new();
    scan(root, 2, &mut seen).ok_or_else(|| InstallError::NoAppInTarball {
        listing: seen.join(", "),
    })
}
