//! One-click driver install: download → gate → crash-safe swap.
//!
//! One pipeline, every platform. The steps that differ — which archive, how to
//! unpack it, what the gate is, how placement swaps — live in [`platform`],
//! which is also where the reasons they differ are written down.
//!
//! # Trust boundary
//!
//! The asset URL comes only from the typed `api.github.com` response (release
//! downloads then 302-redirect to GitHub's asset CDN — host pinning is not the
//! control here, the post-download gates are). `checksums.txt` is transport
//! integrity, not authentication: it ships from the same origin as the archive,
//! so whoever could serve a bad archive could serve a matching hash.
//!
//! What authenticates the bytes is the gate, and it is not the same question on
//! both platforms:
//!
//! - **macOS** asks the publisher. A programmatic download carries no
//!   quarantine xattr, so Gatekeeper never inspects it on its own; the gates in
//!   [`crate::verify`] — codesign integrity, identifier, Team ID, minimum
//!   version, and the bundle-level notarization assessment — are run explicitly
//!   against the **staged** bundle.
//! - **Windows** cannot: every artifact upstream publishes there is unsigned
//!   and carries no build provenance. So it asks the user, once, about these
//!   exact bytes ([`crate::trust`]) — continuity rather than identity, and the
//!   UI built on it must never call the result "verified".
//!
//! Either way the gate runs **before** placement. Nothing lands where
//! [`crate::discovery`] will find it until the gate has answered.
//!
//! # What install deliberately does not do
//!
//! It never stops or restarts the driver daemon — that is a machine-wide
//! singleton other MCP clients share (see [`crate::daemon`]). Placement swaps
//! rather than overwrites (an exchange on macOS, a junction retarget on
//! Windows), so a running daemon keeps the old binary until it next respawns;
//! the UI surfaces that after an upgrade.
//!
//! # Concurrency and progress
//!
//! One install per process, enforced here (settings pane and onboarding both
//! call in). Progress is pushed over an [`mpsc`] channel *and* readable via
//! [`status`] — a pane reopened mid-install pulls the current stage instead of
//! replaying events, which is also why an approval request is a stage and not
//! only an event.

pub mod platform;
pub mod release_feed;

mod pipeline;
/// macOS placement mechanics — `ditto`, the `RENAME_SWAP` exchange, the
/// `/Applications` roots. Only that recipe calls it, so only that platform
/// compiles it; a Windows build would carry it as dead code.
#[cfg(target_os = "macos")]
mod place;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

pub use platform::Anchor;

use crate::verify::VerifiedDriver;
use crate::version::Version;

/// Where the pipeline currently is. `Downloading.total` is the asset size the
/// feed reported (absent if it didn't).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStage {
    Resolving,
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    /// Parked on a person. Only platforms with no automatic gate reach this —
    /// see [`platform`] — and it is a *stage* as well as an event so a pane
    /// reopened mid-install pulls the state instead of missing the one-shot.
    AwaitingApproval,
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
    /// The gate could not answer on its own and these bytes need a person.
    /// The install is parked until [`approve`] or [`decline`]; the UI built on
    /// this must say "unverified publisher" and must not say "verified" — see
    /// [`crate::trust`].
    NeedsApproval {
        sha256: String,
        version: Version,
        bytes: u64,
    },
    Done(VerifiedDriver),
    Cancelled,
    Failed(String),
}

/// The answer to a [`InstallEvent::NeedsApproval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Approved,
    Declined,
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

    /// Named for what it means rather than for one platform's payload: the
    /// macOS archive is missing `CuaDriver.app`, the Windows one is missing
    /// `cua-driver.exe`, and a message naming a `.app` inside a `.zip` reads as
    /// a bug in OxiMux rather than as a broken download.
    #[error("the release archive did not contain the driver ({listing})")]
    ArchiveIncomplete { listing: String },

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

/// Where a pending approval waits. A process-wide slot rather than a channel
/// per install because [`INSTALLING`] already guarantees there is at most one
/// install to answer, and the answering surface (a settings pane, an onboarding
/// step) is not the one holding the install handle.
fn decision_slot() -> &'static (Mutex<Option<Decision>>, Condvar) {
    static SLOT: OnceLock<(Mutex<Option<Decision>>, Condvar)> = OnceLock::new();
    SLOT.get_or_init(|| (Mutex::new(None), Condvar::new()))
}

fn decide(decision: Decision) {
    let (lock, condvar) = decision_slot();
    if let Ok(mut slot) = lock.lock() {
        *slot = Some(decision);
    }
    condvar.notify_all();
}

/// The user approved the staged bytes: pin them and finish the install.
pub fn approve() {
    decide(Decision::Approved);
}

/// The user declined. The install ends as cancelled and nothing is placed.
pub fn decline() {
    decide(Decision::Declined);
}

/// How often the parked pipeline re-checks the cancel flag. Cancellation does
/// not go through the condvar — it is a flag other code sets — so the wait has
/// to be a bounded one rather than a plain `wait`.
const DECISION_POLL: Duration = Duration::from_millis(200);

/// Park until the user answers, the install is cancelled, or the process ends.
fn await_decision(cancel: &AtomicBool) -> Result<Decision, InstallError> {
    let (lock, condvar) = decision_slot();
    let mut slot = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    loop {
        if let Some(decision) = slot.take() {
            return Ok(decision);
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(InstallError::Cancelled);
        }
        slot = condvar
            .wait_timeout(slot, DECISION_POLL)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
}

/// Releases the process-wide lock and clears the stage even if the pipeline
/// panics — a poisoned lock would otherwise brick installs until relaunch.
struct RunGuard;

impl Drop for RunGuard {
    fn drop(&mut self) {
        set_stage(None);
        // A decision that arrived too late (or was never consumed) must not be
        // inherited by the next install as a pre-approval.
        if let Ok(mut slot) = decision_slot().0.lock() {
            *slot = None;
        }
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
/// cancellation is honored between steps, inside the download loop, and while
/// parked on an approval.
///
/// `anchor` is what the gate will rely on where the platform has no answer of
/// its own — see [`Anchor`]. On macOS it carries nothing; on Windows the caller
/// must name a trust store, exactly as [`crate::prepare`] requires.
pub fn spawn_install(
    cancel: Arc<AtomicBool>,
    anchor: Anchor,
) -> Result<RunningInstall, InstallError> {
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
            let emit = |event: InstallEvent| {
                match &event {
                    InstallEvent::Stage(stage) => set_stage(Some(stage.clone())),
                    InstallEvent::NeedsApproval { .. } => {
                        set_stage(Some(InstallStage::AwaitingApproval))
                    }
                    _ => {}
                }
                let _ = sender.send(event);
            };
            let result = pipeline::run(&cancel, &anchor, &emit);
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

    /// Serializes the tests that touch the process-global install state.
    ///
    /// `INSTALLING` and the stage slot are `static`s shared by every test in
    /// this binary, and cargo runs tests on parallel threads — so without this
    /// one test's `store(true)` lands inside another's `compare_exchange`
    /// window and the CAS legitimately fails. That showed up as a ~1-in-6
    /// failure of the whole workspace suite and looked like a product bug.
    ///
    /// Poisoning is recovered rather than propagated: if a test panics while
    /// holding this, the *other* test should still report its own result
    /// instead of a confusing `PoisonError`.
    fn install_state_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The anchor a test passes when the install never reaches the gate.
    #[cfg(windows)]
    fn test_anchor() -> Anchor {
        crate::trust::TrustStore::at(std::env::temp_dir().join("oximux-unreached-pins.json"))
    }

    #[cfg(not(windows))]
    fn test_anchor() -> Anchor {
        Anchor
    }

    #[test]
    fn second_concurrent_install_is_refused_without_side_effects() {
        let _serial = install_state_lock();
        // Hold the lock as a running install would, then try to start another.
        assert!(INSTALLING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let result = spawn_install(Arc::new(AtomicBool::new(false)), test_anchor());
        assert!(matches!(result, Err(InstallError::AlreadyRunning)));
        INSTALLING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn run_guard_clears_lock_and_stage() {
        let _serial = install_state_lock();
        INSTALLING.store(true, Ordering::SeqCst);
        set_stage(Some(InstallStage::Resolving));
        drop(RunGuard);
        assert!(!INSTALLING.load(Ordering::SeqCst));
        assert_eq!(status(), None);
    }
}
