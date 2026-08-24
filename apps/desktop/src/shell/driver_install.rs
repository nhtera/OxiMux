//! Shared UI-side state machine for the one-click driver install.
//!
//! Both surfaces that offer the install (the Computer use settings pane and
//! the onboarding wizard) hold the same [`DriverInstallUi`] and drive it the
//! same way: start → poll [`pump`] on a timer → terminal state. The backend
//! enforces one install per process, so when another surface owns the running
//! install this one observes it through the backend's pull-style status
//! instead of a receiver — which is also what makes a closed-and-reopened
//! settings modal show live progress.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::JoinHandle;

use oximux_computer_use::install::{self, Anchor, InstallError, InstallEvent, InstallStage};
use oximux_computer_use::{VerifiedDriver, Version};

/// Where the official releases live — the "Download manually" escape hatch
/// when the in-app path fails.
pub(crate) const MANUAL_DOWNLOAD_URL: &str = "https://github.com/trycua/cua/releases";

/// What a surface renders for the install affordance.
#[derive(Debug)]
pub(crate) enum DriverInstallUi {
    Idle,
    Running {
        stage: InstallStage,
    },
    /// The download is staged and gated as far as this platform can gate it,
    /// and now needs a person. Only reached where there is no publisher to
    /// check — see `oximux_computer_use::trust`. The install is parked until
    /// [`approve`] or [`decline`], so this is *not* a terminal state.
    AwaitingApproval {
        version: Version,
        // Read only by the Windows-only evidence card (`pane_driver_trust`);
        // macOS never reaches this state at all.
        #[cfg_attr(not(windows), allow(dead_code))]
        sha256: String,
        #[cfg_attr(not(windows), allow(dead_code))]
        bytes: u64,
    },
    /// The backend refused or the pipeline failed — verbatim reason.
    Failed {
        message: String,
    },
}

impl DriverInstallUi {
    /// Whether an install is still in flight. Includes the parked-on-a-person
    /// state: the pipeline is alive, the poll timer must keep running, and a
    /// second click must not start a second install.
    pub(crate) fn is_running(&self) -> bool {
        matches!(
            self,
            DriverInstallUi::Running { .. } | DriverInstallUi::AwaitingApproval { .. }
        )
    }
}

/// The running install this surface owns. `None` receiver-side means we only
/// observe (another surface started it).
pub(crate) struct InstallHandle {
    receiver: Option<Receiver<InstallEvent>>,
    join: Option<JoinHandle<Result<VerifiedDriver, InstallError>>>,
    cancel: Arc<AtomicBool>,
}

impl InstallHandle {
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

/// Start the install, or attach to one already running elsewhere. Returns the
/// handle plus the state to render right away.
///
/// `anchor` is whose approval the gate may rely on where the platform has none
/// of its own — nothing on macOS, the trust store on Windows.
pub(crate) fn begin(anchor: Anchor) -> (Option<InstallHandle>, DriverInstallUi) {
    let cancel = Arc::new(AtomicBool::new(false));
    match install::spawn_install(cancel.clone(), anchor) {
        Ok((receiver, join)) => (
            Some(InstallHandle {
                receiver: Some(receiver),
                join: Some(join),
                cancel,
            }),
            DriverInstallUi::Running {
                stage: InstallStage::Resolving,
            },
        ),
        // Another surface owns the install: observe by pull. No cancel — this
        // surface didn't start it and shouldn't be able to kill it blind.
        Err(InstallError::AlreadyRunning) => (
            None,
            DriverInstallUi::Running {
                stage: install::status().unwrap_or(InstallStage::Resolving),
            },
        ),
        Err(err) => (
            None,
            DriverInstallUi::Failed {
                message: err.to_string(),
            },
        ),
    }
}

/// Advance the UI state from whatever the backend has produced since the last
/// poll. Returns `true` when the install reached a terminal state — the
/// caller re-resolves the driver status and stops its poll timer.
pub(crate) fn pump(handle: &mut Option<InstallHandle>, ui: &mut DriverInstallUi) -> bool {
    let Some(running) = handle else {
        // Observer mode: the backend's stage slot empties exactly when the
        // owning install finishes (RunGuard), so `None` is the terminal sign.
        return match install::status() {
            Some(stage) => {
                *ui = DriverInstallUi::Running { stage };
                false
            }
            None => {
                *ui = DriverInstallUi::Idle;
                true
            }
        };
    };

    let Some(receiver) = running.receiver.as_ref() else {
        return true;
    };
    loop {
        match receiver.try_recv() {
            Ok(InstallEvent::Stage(stage)) => {
                // The stage arrives alongside the request that carries the
                // details; keep whichever state actually has something to
                // render rather than flickering back to a bare stage line.
                if !matches!(
                    (&stage, &*ui),
                    (
                        InstallStage::AwaitingApproval,
                        DriverInstallUi::AwaitingApproval { .. }
                    )
                ) {
                    *ui = DriverInstallUi::Running { stage };
                }
            }
            Ok(InstallEvent::NeedsApproval {
                sha256,
                version,
                bytes,
            }) => {
                *ui = DriverInstallUi::AwaitingApproval {
                    version,
                    sha256,
                    bytes,
                };
            }
            // Cancel is the user's own choice — terminal, but not a failure;
            // the row reverts to the plain status line.
            Ok(InstallEvent::Done(_) | InstallEvent::Cancelled) => {
                *ui = DriverInstallUi::Idle;
                *handle = None;
                return true;
            }
            Ok(InstallEvent::Failed(message)) => {
                *ui = DriverInstallUi::Failed { message };
                *handle = None;
                return true;
            }
            Err(TryRecvError::Empty) => return false,
            // Thread ended without a terminal event (shouldn't happen — the
            // pipeline always sends one). The join result is authoritative.
            Err(TryRecvError::Disconnected) => {
                let outcome = running.join.take().and_then(|join| join.join().ok());
                *ui = match outcome {
                    Some(Ok(_)) | Some(Err(InstallError::Cancelled)) | None => {
                        DriverInstallUi::Idle
                    }
                    Some(Err(err)) => DriverInstallUi::Failed {
                        message: err.to_string(),
                    },
                };
                *handle = None;
                return true;
            }
        }
    }
}

/// One-line human label for a pipeline stage.
pub(crate) fn stage_label(stage: &InstallStage) -> String {
    const MB: u64 = 1024 * 1024;
    match stage {
        InstallStage::Resolving => "Finding the latest driver…".to_string(),
        InstallStage::Downloading {
            received,
            total: Some(total),
        } => format!("Downloading… {} / {} MB", received / MB, total / MB),
        InstallStage::Downloading {
            received,
            total: None,
        } => format!("Downloading… {} MB", received / MB),
        // The gate differs, so the label does. Claiming "verifying publisher"
        // on Windows would name a check that cannot happen there.
        #[cfg(target_os = "macos")]
        InstallStage::Verifying => "Verifying publisher + notarization…".to_string(),
        #[cfg(not(target_os = "macos"))]
        InstallStage::Verifying => "Checking the download…".to_string(),
        InstallStage::AwaitingApproval => "Waiting for your approval…".to_string(),
        InstallStage::Installing => "Installing…".to_string(),
    }
}

/// The user approved the staged bytes. The parked install resumes and pins
/// them; it does not become "verified" — see `oximux_computer_use::trust`.
pub(crate) fn approve() {
    install::approve();
}

/// The user declined. The install ends as cancelled and nothing is placed.
pub(crate) fn decline() {
    install::decline();
}

/// Shown once after a successful upgrade-over-existing: the shared daemon is
/// deliberately left running, so it serves the old version until it respawns.
pub(crate) const UPGRADE_NOTE: &str =
    "A running driver daemon keeps the old version until it next restarts.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_labels_report_progress_in_mb() {
        let label = stage_label(&InstallStage::Downloading {
            received: 5 * 1024 * 1024,
            total: Some(50 * 1024 * 1024),
        });
        assert_eq!(label, "Downloading… 5 / 50 MB");
    }

    #[test]
    fn pump_in_observer_mode_ends_when_backend_goes_idle() {
        // No handle and no backend install running → terminal immediately.
        let mut handle = None;
        let mut ui = DriverInstallUi::Running {
            stage: InstallStage::Resolving,
        };
        assert!(pump(&mut handle, &mut ui));
        assert!(matches!(ui, DriverInstallUi::Idle));
    }
}
