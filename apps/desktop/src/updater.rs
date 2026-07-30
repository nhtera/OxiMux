//! App-side wiring for the auto-updater: the shared status global, the
//! periodic check, and the quit-time swap.
//!
//! # Why the swap waits for quit
//!
//! Several things this app spawns are resolved from the bundle path *at spawn
//! time*, not at boot: the relay daemon (whose socket name is compiled from
//! the wire-protocol version), the `agent-status` hook CLI embedded into live
//! agent sessions, and the screen-control gate. Replacing the bundle under a
//! running process would leave an old-version app spawning new-version
//! helpers — a skew with no error path, because each side is individually
//! valid. So the background pipeline stops at a verified staged copy, and the
//! millisecond swap happens on the way out.
//!
//! The user is never restarted. Ignoring the pill costs nothing: the next
//! ordinary quit applies the update, and the next launch is the new version.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, AsyncApp, Global};
use oximux_auto_update::staging::SwapOutcome;
use oximux_auto_update::{CheckTrigger, UpdateStatus, UpdaterConfig, Version};
use oximux_settings::AutoUpdateSettings;

const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const MANIFEST_FILE: &str = "pending-update.json";
/// Sentinel written across the quit-time swap. Present at boot means the swap
/// never confirmed, so the installed bundle is re-verified before trusting it.
const SWAP_SENTINEL: &str = "update-pending-verify";

/// How long after boot the first check waits. Boot is the most latency-
/// sensitive stretch in the app; an update can wait a minute.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
/// Cadence afterwards. Releases are not frequent enough to justify hourly
/// polling, and this is well inside the unauthenticated API's rate limit.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the UI drains the pipeline's status channel while a check runs.
const PUMP_INTERVAL: Duration = Duration::from_millis(150);

/// The one place every update surface reads from.
pub struct UpdaterState {
    pub status: UpdateStatus,
    /// `None` when this process can't update itself (dev build, unsigned,
    /// translocated, read-only install root) — the reason is carried in
    /// `status` as `Unsupported`.
    config: Option<UpdaterConfig>,
}

impl Global for UpdaterState {}

impl UpdaterState {
    /// The staged version awaiting a restart, if any.
    pub fn ready_version(&self) -> Option<&str> {
        match &self.status {
            UpdateStatus::Ready { version, .. } => Some(version),
            _ => None,
        }
    }

    pub fn bundle_root(&self) -> Option<PathBuf> {
        self.config.as_ref().map(|c| c.app.bundle_root.clone())
    }
}

fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

fn manifest_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(MANIFEST_FILE))
}

fn sentinel_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join(SWAP_SENTINEL))
}

fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join(APP_DATA_SUBDIR))
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0))
}

fn build_config() -> Result<UpdaterConfig, oximux_auto_update::UnsupportedReason> {
    use oximux_auto_update::UnsupportedReason;

    let exe = std::env::current_exe().map_err(|_| UnsupportedReason::NotABundle)?;
    let app = oximux_auto_update::eligibility(&exe)?;
    let (Some(cache_dir), Some(manifest_path)) = (cache_dir(), manifest_path()) else {
        return Err(UnsupportedReason::RootNotWritable);
    };
    Ok(UpdaterConfig {
        current_version: current_version(),
        app,
        cache_dir,
        manifest_path,
    })
}

/// Install the updater: resolve what this install is, recover from any
/// interrupted swap, sweep crash leftovers, then start the periodic check.
///
/// Call once at boot. Nothing here touches the disk on the GPUI thread —
/// `build_config` runs `codesign`, and a subprocess spawn in the boot closure
/// would delay the first paint.
pub fn install(cx: &mut App) {
    cx.set_global(UpdaterState {
        status: UpdateStatus::Idle,
        config: None,
    });

    cx.spawn(async move |cx: &mut AsyncApp| {
        let config = cx
            .background_executor()
            .spawn(async move { build_config() })
            .await;

        let config = match config {
            Ok(config) => config,
            Err(reason) => {
                tracing::debug!(?reason, "auto-update disabled for this install");
                let _ = cx.update(|cx| set_status(cx, UpdateStatus::Unsupported(reason)));
                return;
            }
        };

        // The pin inside this config was read from the running bundle before
        // anything could have been staged. It is the process's trust anchor
        // for the rest of its life — including the quit-time swap, which
        // reads it back out of the global rather than re-deriving it.
        let _ = cx.update(|cx| {
            cx.global_mut::<UpdaterState>().config = Some(config.clone());
        });

        // Housekeeping: a stranded mount or staging dir from a killed run
        // would otherwise sit there until someone noticed a missing hundred
        // megabytes.
        let sweep = config.clone();
        let interrupted = cx
            .background_executor()
            .spawn(async move {
                let interrupted = recover_interrupted_swap(&sweep);
                oximux_auto_update::boot_sweep(
                    &sweep.cache_dir,
                    &sweep.app.bundle_root,
                    &sweep.manifest_path,
                );
                interrupted
            })
            .await;
        if let Some(status) = interrupted {
            let _ = cx.update(|cx| set_status(cx, status));
        }

        cx.background_executor().timer(FIRST_CHECK_DELAY).await;
        loop {
            let enabled = cx.update(|cx| {
                cx.try_global::<AutoUpdateSettings>()
                    .is_none_or(|s| s.enabled)
            });
            if enabled {
                run_check(config.clone(), CheckTrigger::Background, cx).await;
            }
            cx.background_executor().timer(CHECK_INTERVAL).await;
        }
    })
    .detach();
}

/// Quit and come back on the staged version.
///
/// The helper is spawned *before* quitting so it is already waiting on this
/// pid; the swap itself happens in the quit path. If the helper cannot be
/// spawned we still quit — the update applies either way, and the user gets
/// the new version the next time they open the app.
pub fn restart_to_update(cx: &mut App) {
    let bundle_root = cx
        .try_global::<UpdaterState>()
        .and_then(|state| state.bundle_root());
    match bundle_root {
        Some(root) => {
            if let Err(err) = crate::platform::relaunch::spawn_relaunch_helper(&root) {
                tracing::warn!(%err, "could not spawn the relaunch helper; quitting anyway");
            }
        }
        None => {
            tracing::warn!("no installed bundle to relaunch; quitting without restart");
        }
    }
    cx.quit();
}

/// Tell the user, once, that this launch is a new version.
///
/// Deferred by a beat because the toast bus has no surface until a window has
/// mounted and registered one; firing at install time would no-op silently.
pub fn announce_update(cx: &mut App) {
    let version = env!("CARGO_PKG_VERSION");
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        let _ = cx.update(|cx| {
            crate::shell::chrome::toast::toast(
                cx,
                crate::shell::chrome::toast::ToastKind::Success,
                format!("Updated to v{version}"),
            );
        });
    })
    .detach();
}

/// Start a check now, from a user click. Refused (silently) when one is
/// already in flight — the pipeline's own guard is the authority.
pub fn check_now(cx: &mut App) {
    let Some(config) = cx
        .try_global::<UpdaterState>()
        .and_then(|state| state.config.clone())
    else {
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        run_check(config, CheckTrigger::Manual, cx).await;
    })
    .detach();
}

/// Run one pipeline pass, pumping its status into the global as it goes.
async fn run_check(config: UpdaterConfig, trigger: CheckTrigger, cx: &mut AsyncApp) {
    let cancel = Arc::new(AtomicBool::new(false));
    let started = cx
        .background_executor()
        .spawn({
            let config = config.clone();
            let cancel = cancel.clone();
            async move { oximux_auto_update::spawn_check(config, trigger, cancel) }
        })
        .await;

    let (rx, handle) = match started {
        Ok(running) => running,
        // AlreadyRunning is the expected outcome of a click during a
        // background check; nothing to report.
        Err(_) => return,
    };

    loop {
        let drained: Vec<UpdateStatus> = rx.try_iter().collect();
        let finished = handle.is_finished();
        for status in drained {
            let _ = cx.update(|cx| set_status(cx, status));
        }
        if finished {
            break;
        }
        cx.background_executor().timer(PUMP_INTERVAL).await;
    }
    // The joined value is authoritative — the channel may have been drained
    // before the terminal status landed.
    if let Ok(terminal) = cx
        .background_executor()
        .spawn(async move { handle.join() })
        .await
    {
        let _ = cx.update(|cx| set_status(cx, terminal));
    }
}

fn set_status(cx: &mut App, status: UpdateStatus) {
    // Repainting every window on each 64 KiB progress tick would be the most
    // expensive thing this feature does; only changes get through.
    if cx
        .try_global::<UpdaterState>()
        .is_none_or(|state| state.status == status)
    {
        return;
    }
    cx.global_mut::<UpdaterState>().status = status;
    cx.refresh_windows();
}

/// Apply a staged update, if one is ready. Called from the quit path — after
/// session capture, before the process exits.
///
/// Fails closed in every direction: a staged bundle that no longer verifies is
/// discarded and the running version stays installed. Returns whether a swap
/// actually happened (for logging; the caller quits either way).
/// `from_signal` skips the swap: a SIGINT/SIGTERM shutdown runs against a 3s
/// force-exit deadline that the swap's two verifications could lose, and
/// being killed mid-swap is the one case the sentinel exists to clean up
/// after. Skipping costs nothing — the staged update is still on disk, still
/// verified, and applies at the next clean quit.
pub fn apply_pending_at_quit(cx: &mut App, from_signal: bool) -> bool {
    if from_signal {
        tracing::debug!("signal shutdown: leaving the staged update for the next clean quit");
        return false;
    }
    // The boot-captured config, NOT a fresh one. Re-deriving the pin here
    // would read whatever currently sits at the bundle path — which at this
    // exact moment may be a staged update — and pin the swap gate to the
    // thing it is supposed to be checking.
    let Some(config) = cx
        .try_global::<UpdaterState>()
        .and_then(|state| state.config.clone())
    else {
        return false;
    };
    oximux_auto_update::staging::apply_pending(&config, sentinel_path().as_deref())
        == SwapOutcome::Applied
}

/// A sentinel at boot means a previous swap never confirmed. Nothing here can
/// safely repair a bundle that fails to verify — the running process *is*
/// that bundle — so the failure is reported for the About pane to show, and
/// the sentinel is left in place for the next boot to re-check.
fn recover_interrupted_swap(config: &UpdaterConfig) -> Option<UpdateStatus> {
    let sentinel = sentinel_path()?;
    match oximux_auto_update::staging::recover_interrupted_swap(config, &sentinel) {
        Ok(()) => None,
        Err(err) => {
            tracing::error!(%err, "installed bundle failed verification after an interrupted update");
            Some(UpdateStatus::Failed {
                error: "an update was interrupted — reinstall to be safe".into(),
                // Manual: this is not a check that quietly failed, it is a
                // statement about the bundle the user is running right now.
                trigger: CheckTrigger::Manual,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_apps_own_version_parses() {
        // A malformed CARGO_PKG_VERSION would silently pin the updater at
        // 0.0.0 and offer every release as an upgrade.
        assert!(
            Version::parse(env!("CARGO_PKG_VERSION")).is_some(),
            "workspace version {} is not a plain x.y.z triple",
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn ready_version_is_only_reported_when_staged() {
        let idle = UpdaterState {
            status: UpdateStatus::Idle,
            config: None,
        };
        assert_eq!(idle.ready_version(), None);
        let ready = UpdaterState {
            status: UpdateStatus::Ready {
                version: "0.2.0".into(),
                notes: String::new(),
            },
            config: None,
        };
        assert_eq!(ready.ready_version(), Some("0.2.0"));
    }
}
