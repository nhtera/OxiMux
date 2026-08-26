//! In-app auto-update for the desktop bundle.
//!
//! The lifecycle, deliberately split in two:
//!
//! 1. **Background** (this crate, via [`spawn_check`]): poll the release
//!    feed, download the DMG, mount it, copy the app to a hidden staging dir
//!    next to the installed bundle, and verify it against the boot-time
//!    signature pin. Ends at [`UpdateStatus::Ready`] — a verified bundle
//!    *staged on disk*, installed bundle untouched.
//! 2. **Quit** (the app's shutdown path): re-verify the staged bundle and
//!    atomically swap it in. Never while running — subprocesses are resolved
//!    from the bundle path at spawn time (relay daemon, hook CLI, screen
//!    gate), and swapping under a live process would hand an old-version app
//!    new-version helpers.
//!
//! Restart is always the user's move. An ignored update simply applies on
//! the next natural quit.

//! ## Platform split
//!
//! The state machine, the release feed, and version comparison are plain data
//! work and compile everywhere. Everything that touches the *shape of an
//! install* is per-platform, because the two shapes have nothing in common: a
//! macOS install is one `.app` bundle delivered as a DMG and pinned to a
//! Developer ID signature; a Windows install is a directory of loose files
//! delivered as a zip and anchored to the signed release manifest. See
//! [`windows`] for the full comparison and why the trust roots differ.
//!
//! What both sides share — the manifest, its minisign signature, the download
//! host allow-list, the all-or-nothing swap — lives in [`release`], once, and
//! is shared with the CLI's `oximux update` rather than reimplemented per
//! consumer.
//!
//! The two platform modules present the same three entry points, so the host
//! app drives one code path: `eligibility` (is this install updatable),
//! `pipeline::run` (stage a verified payload), and `staging::apply_pending`
//! (swap it in at quit).

pub mod bundle;
pub mod feed;
#[cfg(target_os = "macos")]
pub mod pipeline;
pub mod release;
#[cfg(target_os = "macos")]
pub mod staging;
pub mod version;
#[cfg(target_os = "windows")]
pub mod windows;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;

pub use bundle::UnsupportedReason;
#[cfg(target_os = "macos")]
pub use bundle::{eligibility, InstalledApp};
#[cfg(target_os = "macos")]
pub use oximux_macos_trust::{DiskShortfall, SignaturePolicy, TrustError};
#[cfg(target_os = "macos")]
pub use staging::{boot_sweep, PendingUpdate};
pub use version::Version;
#[cfg(target_os = "windows")]
pub use windows::{boot_sweep, eligibility, staging, InstalledApp, PendingUpdate};

/// Whether this build has a self-update pipeline at all.
///
/// macOS and Windows do; a Linux desktop build carries the types (so the
/// settings pane and the menu compile everywhere) but nothing to run, and says
/// so through [`UnsupportedReason::NotABundle`] rather than by hiding the
/// controls — a missing row explains nothing.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Who asked for this check. Background failures stay quiet; a user who
/// clicked "Check for updates" gets told what went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckTrigger {
    Background,
    Manual,
}

/// The one state machine every update surface renders from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Checking {
        trigger: CheckTrigger,
    },
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
    },
    /// Staging + verifying — the disk-touching stretch after the download.
    Installing {
        version: String,
    },
    /// A verified bundle is staged on disk. The installed bundle is still
    /// the running version; the swap happens at quit.
    Ready {
        version: String,
        notes: String,
    },
    UpToDate,
    Unsupported(UnsupportedReason),
    Failed {
        error: String,
        trigger: CheckTrigger,
    },
}

impl UpdateStatus {
    /// Whether a new check may start from this state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Ready { .. } | Self::UpToDate | Self::Unsupported(_) | Self::Failed { .. }
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("a check is already running")]
    AlreadyRunning,

    #[error("release feed error: {detail}")]
    Feed { detail: String },

    #[error("release {tag} is missing asset `{name}`")]
    MissingAsset { name: String, tag: String },

    #[error("GitHub API rate limit hit — will retry later")]
    RateLimited,

    #[error("network error: {detail}")]
    Network { detail: String },

    #[error("download redirected to untrusted host `{host}`")]
    DisallowedHost { host: String },

    #[error("download exceeded its declared size ({declared} bytes declared, {received} received)")]
    Oversize { declared: u64, received: u64 },

    #[cfg(target_os = "macos")]
    #[error("not enough disk space: need ~{} MB, {} MB free", .0.needed_mb, .0.free_mb)]
    DiskSpace(DiskShortfall),

    #[error("could not mount the update image: {detail}")]
    Mount { detail: String },

    #[error("no OxiMux.app inside the update image")]
    NoAppInImage,

    #[error("{detail}")]
    Staging { detail: String },

    #[cfg(target_os = "macos")]
    #[error(transparent)]
    Gate(#[from] TrustError),

    #[error("update claims {staged} but the running app is already {running} — refusing")]
    Downgrade { staged: String, running: String },

    #[error("could not read the staged bundle's version (got: {output})")]
    UnreadableStagedVersion { output: String },

    #[error("cancelled")]
    Cancelled,

    /// Anything the shared signed-release machinery refused: a bad signature,
    /// a digest that did not match, an archive that would have written outside
    /// its staging directory.
    ///
    /// Transparent rather than re-worded per variant. Those errors are already
    /// written to be read by a user — "the release manifest is not signed by
    /// the OxiMux release key" says the whole thing — and a second layer of
    /// paraphrase is how the specific one gets lost.
    #[error(transparent)]
    Release(#[from] release::ReleaseError),
}

/// Everything a pipeline run needs, resolved once by the host app.
///
/// `app` is the one per-platform field: a `.app` bundle plus its codesign pin
/// on macOS, an install directory on Windows. Everything else is the same
/// question asked of both — what version is running, where may scratch files
/// go, and where is the record of what is staged.
#[derive(Debug, Clone)]
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub struct UpdaterConfig {
    pub current_version: Version,
    /// Captured at boot, never re-derived. Both platforms depend on that for
    /// the same reason: after a staged update lands, the install path holds
    /// different files, and an anchor read at quit would describe the very
    /// thing it is supposed to be checking.
    pub app: InstalledApp,
    /// Scratch space for downloads (a cache directory).
    pub cache_dir: PathBuf,
    /// Where the pending-update record lives (app data directory).
    pub manifest_path: PathBuf,
}

/// One check per process, ever, at a time. A `static` compare-and-swap rather
/// than a status read: the 6h ticker firing while a slow download is still in
/// flight must be rejected here, atomically — two pipelines would race the
/// same staging area.
#[cfg(any(target_os = "macos", target_os = "windows"))]
static CHECKING: AtomicBool = AtomicBool::new(false);

/// Releases the process-wide lock even if the pipeline panics.
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct RunGuard;

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Drop for RunGuard {
    fn drop(&mut self) {
        CHECKING.store(false, Ordering::SeqCst);
    }
}

/// What a caller holds while a check runs: the status stream to render, and
/// the join handle whose final status is authoritative.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub type RunningCheck = (mpsc::Receiver<UpdateStatus>, JoinHandle<UpdateStatus>);

/// The platform's staging pipeline. One name so [`spawn_check`] — the
/// threading, the single-flight lock, the status plumbing, all of it identical
/// — does not have to be written twice around the one call that differs.
#[cfg(target_os = "macos")]
use crate::pipeline::run as run_pipeline;
#[cfg(target_os = "windows")]
use crate::windows::pipeline::run as run_pipeline;

/// Start a check on a dedicated thread, or refuse if one is in flight.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn spawn_check(
    config: UpdaterConfig,
    trigger: CheckTrigger,
    cancel: Arc<AtomicBool>,
) -> Result<RunningCheck, UpdateError> {
    if CHECKING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(UpdateError::AlreadyRunning);
    }

    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("oximux-update-check".into())
        .spawn(move || {
            let _guard = RunGuard;
            let emit = |status: UpdateStatus| {
                let _ = tx.send(status);
            };
            let terminal = match run_pipeline(&config, trigger, &cancel, &emit) {
                Ok(status) => status,
                Err(UpdateError::Cancelled) => UpdateStatus::Idle,
                Err(err) => UpdateStatus::Failed {
                    error: err.to_string(),
                    trigger,
                },
            };
            emit(terminal.clone());
            terminal
        })
        .expect("spawn update-check thread");

    Ok((rx, handle))
}

// --- The three per-platform verbs, behind one signature each ----------------
//
// The host app's updater wiring — the status global, the periodic check, the
// quit hook — is identical on both platforms and should read that way. These
// three wrappers are where the platforms stop being the same, so that the file
// driving them contains no `cfg` at all.

/// Boot housekeeping: repair whatever a previous run left half-done, then throw
/// away what nothing references.
///
/// Returns a status worth showing the user, which today only macOS ever
/// produces — a bundle whose swap never confirmed cannot be repaired from
/// inside the process that *is* that bundle, so it is reported instead. The
/// Windows swap is per-file with a rollback, and its own interrupted state is
/// repairable, so it repairs it and returns nothing.
///
/// Does subprocess work and directory walks; call it off the UI thread.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn boot_housekeeping(config: &UpdaterConfig, sentinel: Option<&std::path::Path>) -> Option<UpdateStatus> {
    #[cfg(target_os = "macos")]
    {
        let interrupted = sentinel.and_then(|sentinel| {
            staging::recover_interrupted_swap(config, sentinel)
                .err()
                .map(|err| {
                    tracing::error!(%err, "installed bundle failed verification after an interrupted update");
                    UpdateStatus::Failed {
                        error: "an update was interrupted — reinstall to be safe".into(),
                        // Manual: this is not a check that quietly failed, it is
                        // a statement about the bundle running right now.
                        trigger: CheckTrigger::Manual,
                    }
                })
        });
        staging::boot_sweep(&config.cache_dir, &config.app.bundle_root, &config.manifest_path);
        interrupted
    }
    #[cfg(target_os = "windows")]
    {
        let _ = sentinel;
        staging::boot_sweep(&config.app.install_dir, &config.manifest_path);
        None
    }
}

/// Apply a staged update. Call from the quit path only — never while the app is
/// running, for the reasons in each platform's staging module.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn apply_pending_update(
    config: &UpdaterConfig,
    sentinel: Option<&std::path::Path>,
) -> staging::SwapOutcome {
    #[cfg(target_os = "macos")]
    {
        staging::apply_pending(config, sentinel)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = sentinel;
        staging::apply_pending(config, &config.manifest_path)
    }
}

/// What to relaunch to come back on the new version: the `.app` bundle on
/// macOS, the installed executable on Windows.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn relaunch_target(app: &InstalledApp) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        app.bundle_root.clone()
    }
    #[cfg(target_os = "windows")]
    {
        app.install_dir.join(windows::install::APP_EXE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_the_restartable_ones() {
        assert!(UpdateStatus::Idle.is_terminal());
        assert!(UpdateStatus::UpToDate.is_terminal());
        assert!(!UpdateStatus::Checking {
            trigger: CheckTrigger::Manual
        }
        .is_terminal());
        assert!(!UpdateStatus::Downloading {
            version: "0.2.0".into(),
            received: 1,
            total: None
        }
        .is_terminal());
    }

    /// A config that names nothing real. Only the single-flight test uses it,
    /// and that test refuses before the pipeline ever reads a field.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn unreachable_config() -> UpdaterConfig {
        UpdaterConfig {
            current_version: Version::new(0, 1, 0),
            #[cfg(target_os = "macos")]
            app: InstalledApp {
                bundle_root: PathBuf::from("/Applications/OxiMux.app"),
                pin: SignaturePolicy {
                    identifier: "dev.nhtera.oximux".into(),
                    team_id: "TEAM".into(),
                },
            },
            #[cfg(target_os = "windows")]
            app: InstalledApp {
                install_dir: PathBuf::from(r"C:\nowhere\OxiMux"),
            },
            cache_dir: std::env::temp_dir(),
            manifest_path: std::env::temp_dir().join("never-written.json"),
        }
    }

    /// The 6-hour ticker firing while a slow download is still in flight must
    /// be rejected atomically, not queued — two pipelines would race the same
    /// staging area. The lock is one `static` shared by both platforms, so this
    /// runs on both.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn a_second_spawn_while_one_runs_is_rejected() {
        // Claim the lock by hand to simulate an in-flight check without any
        // network: the CAS must refuse, not queue.
        assert!(CHECKING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let err = spawn_check(
            unreachable_config(),
            CheckTrigger::Background,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("must refuse");
        assert!(matches!(err, UpdateError::AlreadyRunning), "got {err:?}");
        CHECKING.store(false, Ordering::SeqCst);
    }

    /// Both platforms have a pipeline now; a build that reports otherwise would
    /// hide the update controls on a platform that can use them.
    #[test]
    fn the_two_desktop_platforms_report_themselves_as_updatable() {
        assert_eq!(SUPPORTED, cfg!(any(target_os = "macos", target_os = "windows")));
    }
}
