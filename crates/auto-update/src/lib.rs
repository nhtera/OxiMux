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
//! install* — `.app` bundles, DMG mounting, codesign pinning, the same-volume
//! exchange — is macOS-only and gated as such. Windows gets its own staging and
//! trust path (versioned directories, Authenticode) built on the same neutral
//! core; until then a Windows build carries the types but not the pipeline.

pub mod bundle;
pub mod feed;
#[cfg(target_os = "macos")]
pub mod pipeline;
#[cfg(target_os = "macos")]
pub mod staging;
pub mod version;

#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{mpsc, Arc};
#[cfg(target_os = "macos")]
use std::thread::JoinHandle;

pub use bundle::UnsupportedReason;
#[cfg(target_os = "macos")]
pub use bundle::{eligibility, InstalledApp};
#[cfg(target_os = "macos")]
pub use oximux_macos_trust::{DiskShortfall, SignaturePolicy, TrustError};
#[cfg(target_os = "macos")]
pub use staging::{boot_sweep, PendingUpdate};
pub use version::Version;

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
}

/// Everything a pipeline run needs, resolved once by the host app.
#[derive(Debug, Clone)]
#[cfg(target_os = "macos")]
pub struct UpdaterConfig {
    pub current_version: Version,
    /// Captured at boot — see [`bundle::InstalledApp`] on why never later.
    pub app: InstalledApp,
    /// Scratch space for DMGs and mountpoints (a cache directory).
    pub cache_dir: PathBuf,
    /// Where the pending-update manifest lives (app data directory).
    pub manifest_path: PathBuf,
}

/// One check per process, ever, at a time. A `static` compare-and-swap rather
/// than a status read: the 6h ticker firing while a slow download is still in
/// flight must be rejected here, atomically — two pipelines would race the
/// same staging area.
#[cfg(target_os = "macos")]
static CHECKING: AtomicBool = AtomicBool::new(false);

/// Releases the process-wide lock even if the pipeline panics.
#[cfg(target_os = "macos")]
struct RunGuard;

#[cfg(target_os = "macos")]
impl Drop for RunGuard {
    fn drop(&mut self) {
        CHECKING.store(false, Ordering::SeqCst);
    }
}

/// What a caller holds while a check runs: the status stream to render, and
/// the join handle whose final status is authoritative.
#[cfg(target_os = "macos")]
pub type RunningCheck = (mpsc::Receiver<UpdateStatus>, JoinHandle<UpdateStatus>);

/// Start a check on a dedicated thread, or refuse if one is in flight.
#[cfg(target_os = "macos")]
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
            let terminal = match pipeline::run(&config, trigger, &cancel, &emit) {
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

    // The single-flight lock guards the macOS pipeline, and every type this
    // needs to build a config — the bundle, the codesign pin — describes a
    // `.app`. Windows gets its own staging path; when it lands, this test's
    // counterpart goes with it.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_second_spawn_while_one_runs_is_rejected() {
        // Claim the lock by hand to simulate an in-flight check without any
        // network: the CAS must refuse, not queue.
        assert!(CHECKING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok());
        let config = UpdaterConfig {
            current_version: Version::new(0, 1, 0),
            app: InstalledApp {
                bundle_root: PathBuf::from("/Applications/OxiMux.app"),
                pin: SignaturePolicy {
                    identifier: "dev.nhtera.oximux".into(),
                    team_id: "TEAM".into(),
                },
            },
            cache_dir: std::env::temp_dir(),
            manifest_path: std::env::temp_dir().join("never-written.json"),
        };
        let err = spawn_check(
            config,
            CheckTrigger::Background,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("must refuse");
        assert!(matches!(err, UpdateError::AlreadyRunning), "got {err:?}");
        CHECKING.store(false, Ordering::SeqCst);
    }
}
