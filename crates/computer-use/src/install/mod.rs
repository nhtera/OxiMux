//! One-click driver install: download → verify-by-publisher → crash-safe swap.
//!
//! # Trust boundary
//!
//! The asset URL comes only from the typed `api.github.com` response (release
//! downloads then 302-redirect to GitHub's asset CDN — host pinning is not the
//! control here, the post-download gates are). The download is programmatic,
//! so it carries no quarantine xattr and Gatekeeper never inspects it; the
//! gates in [`crate::verify`] — codesign integrity, identifier, Team ID,
//! minimum version, and the bundle-level notarization assessment — are the
//! only gates, run against the **staged** bundle before anything touches an
//! install root. `checksums.txt` is transport integrity, not authentication.
//!
//! # What install deliberately does not do
//!
//! It never stops or restarts the driver daemon — that is a machine-wide
//! singleton other MCP clients share (see [`crate::daemon`]). The swap uses
//! atomic renames, so a running daemon keeps its old inode until it next
//! respawns; the UI surfaces that after an upgrade.
//!
//! # Concurrency and progress
//!
//! One install per process, enforced here (settings pane and onboarding both
//! call in). Progress is pushed over an [`mpsc`] channel *and* readable via
//! [`status`] — a pane reopened mid-install pulls the current stage instead of
//! replaying events.

pub mod release_feed;

mod pipeline;
mod place;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use crate::verify::VerifiedDriver;
use crate::version::Version;

/// Where the pipeline currently is. `Downloading.total` is the asset size the
/// feed reported (absent if it didn't).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStage {
    Resolving,
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Installing,
}

/// Pushed to the channel as the pipeline advances. Terminal outcomes also come
/// back typed through the join handle; `Failed` carries the display string so
/// a pure event consumer needs no join. `Cancelled` is its own terminal event —
/// the user asked for it, and rendering it as a failure would turn every
/// Cancel click into a red banner.
#[derive(Debug, Clone)]
pub enum InstallEvent {
    Stage(InstallStage),
    Done(VerifiedDriver),
    Cancelled,
    Failed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("an install is already running")]
    AlreadyRunning,

    #[error("release feed error: {detail}")]
    Feed { detail: String },

    #[error("no published driver release in the feed window")]
    NoDriverRelease,

    #[error("release {tag} is missing asset `{name}`")]
    MissingAsset { name: String, tag: String },

    #[error("network error: {detail}")]
    Network { detail: String },

    #[error("GitHub API rate limit hit — try again later, or install manually")]
    RateLimited,

    #[error("checksum mismatch for {asset} — download corrupted or tampered; nothing was installed")]
    ChecksumMismatch { asset: String },

    #[error("no CuaDriver.app inside the release tarball (found: {listing})")]
    NoAppInTarball { listing: String },

    #[error("release {staged} is older than the installed {installed} — refusing to downgrade")]
    Downgrade { staged: Version, installed: Version },

    #[error("not enough disk space at {}: need ~{needed_mb} MB, {free_mb} MB free", root.display())]
    DiskSpace {
        root: PathBuf,
        needed_mb: u64,
        free_mb: u64,
    },

    #[error("no writable install root (tried /Applications and ~/Applications)")]
    NoWritableRoot,

    #[error("install step failed: {detail}")]
    Install { detail: String },

    #[error(transparent)]
    Gate(#[from] crate::Error),

    #[error("cancelled")]
    Cancelled,
}

/// One install per process. A `static` because the settings pane and the
/// onboarding wizard are separate views with separate state — the lock has to
/// outlive and span both.
static INSTALLING: AtomicBool = AtomicBool::new(false);

/// Pull-side snapshot of the running install (None when idle).
fn stage_slot() -> &'static Mutex<Option<InstallStage>> {
    static SLOT: OnceLock<Mutex<Option<InstallStage>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Current stage of the in-flight install, if any. Cheap — for a pane
/// reopened mid-install (the terminal re-check stays `DriverStatus::resolve`).
pub fn status() -> Option<InstallStage> {
    stage_slot().lock().ok().and_then(|slot| slot.clone())
}

fn set_stage(stage: Option<InstallStage>) {
    if let Ok(mut slot) = stage_slot().lock() {
        *slot = stage;
    }
}

/// Releases the process-wide lock and clears the stage even if the pipeline
/// panics — a poisoned lock would otherwise brick installs until relaunch.
struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        set_stage(None);
        INSTALLING.store(false, Ordering::SeqCst);
    }
}

/// What a caller holds while an install runs: the event stream to render, and
/// the join handle whose typed result is authoritative.
pub type RunningInstall = (
    Receiver<InstallEvent>,
    JoinHandle<Result<VerifiedDriver, InstallError>>,
);

/// Start an install on a background thread.
///
/// Returns [`InstallError::AlreadyRunning`] without touching anything when an
/// install is in flight — a second click or a second surface (settings +
/// onboarding) must not race the filesystem swap. Set `cancel` to abort;
/// cancellation is honored between steps and inside the download loop.
pub fn spawn_install(cancel: Arc<AtomicBool>) -> Result<RunningInstall, InstallError> {
    if INSTALLING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(InstallError::AlreadyRunning);
    }

    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("cua-driver-install".into())
        .spawn(move || {
            let _guard = RunGuard;
            let emit = |stage: InstallStage| {
                set_stage(Some(stage.clone()));
                let _ = sender.send(InstallEvent::Stage(stage));
            };
            let result = pipeline::run(&cancel, &emit);
            let _ = sender.send(match &result {
                Ok(driver) => InstallEvent::Done(driver.clone()),
                Err(InstallError::Cancelled) => InstallEvent::Cancelled,
                Err(err) => InstallEvent::Failed(err.to_string()),
            });
            result
        })
        .map_err(|err| {
            INSTALLING.store(false, Ordering::SeqCst);
            InstallError::Install {
                detail: format!("could not spawn install thread: {err}"),
            }
        })?;

    Ok((receiver, handle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_concurrent_install_is_refused_without_side_effects() {
        // Hold the lock as a running install would, then try to start another.
        assert!(INSTALLING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let result = spawn_install(Arc::new(AtomicBool::new(false)));
        assert!(matches!(result, Err(InstallError::AlreadyRunning)));
        INSTALLING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn run_guard_clears_lock_and_stage() {
        INSTALLING.store(true, Ordering::SeqCst);
        set_stage(Some(InstallStage::Resolving));
        drop(RunGuard);
        assert!(!INSTALLING.load(Ordering::SeqCst));
        assert_eq!(status(), None);
    }
}
