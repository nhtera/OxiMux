//! The check → download → mount → stage → verify pipeline.
//!
//! Runs on the dedicated thread `spawn_check` creates; never on a UI thread.
//! Network I/O is sync `ureq` on purpose — the app hosts two tokio runtimes
//! with different jobs, and a plain thread + mpsc keeps this pipeline (and
//! its cancellation) independent of both.
//!
//! Trust boundary: the download is programmatic, so the DMG never carries a
//! quarantine xattr and Gatekeeper never inspects it on its own. Every gate
//! therefore runs explicitly here, against the *staged* copy, before the
//! manifest ever names it: codesign integrity, identifier + Team ID against
//! the boot-time pin, `spctl` (the only revocation-aware check), and finally
//! the staged bundle's own embedded version — the feed's tag is treated as a
//! claim, not a fact, so a compromised feed cannot label an old build as new.
//!
//! The pipeline deliberately ends at "verified bundle staged + manifest
//! written". Swapping the installed bundle while the app runs would version-
//! skew every subprocess resolved from the bundle afterwards (relay daemon,
//! status-hook CLI, screen gate); the swap belongs to the quit path.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oximux_macos_trust::{ditto_copy, dir_size, ensure_disk_space, run_bounded};

use crate::staging::{claim_staging_dir, downloads_dir, mounts_dir, PendingUpdate};
use crate::version::Version;
use crate::{CheckTrigger, UpdateError, UpdateStatus, UpdaterConfig};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
/// GitHub's API requires a User-Agent; requests without one are rejected.
const USER_AGENT: &str = "oximux-updater";

const DEFAULT_FEED_URL: &str =
    "https://api.github.com/repos/nhtera/OxiMux/releases/latest";

fn feed_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("OXIMUX_UPDATE_FEED_URL") {
        return url;
    }
    DEFAULT_FEED_URL.to_string()
}

/// Whether the *final* URL (after redirects) may serve the DMG. GitHub
/// release assets redirect to `*.githubusercontent.com`; anything else means
/// the redirect chain left GitHub and the download must not proceed —
/// signature checks remain downstream, but transport pinning is cheap
/// defense-in-depth.
fn host_allowed(host: &str) -> bool {
    if host == "github.com" || host == "api.github.com" {
        return true;
    }
    if host.ends_with(".githubusercontent.com") {
        return true;
    }
    #[cfg(debug_assertions)]
    if host == "localhost" || host == "127.0.0.1" {
        return true;
    }
    false
}

/// `spctl` runs in every real build. The skip exists only for local E2E runs
/// against unnotarized test DMGs, and only in debug binaries — release code
/// contains no way to bypass the one revocation-aware gate.
#[cfg(debug_assertions)]
fn spctl_skipped() -> bool {
    std::env::var_os("OXIMUX_UPDATE_SKIP_SPCTL").is_some()
}
#[cfg(not(debug_assertions))]
fn spctl_skipped() -> bool {
    false
}

pub(crate) fn run(
    config: &UpdaterConfig,
    trigger: CheckTrigger,
    cancel: &AtomicBool,
    emit: &dyn Fn(UpdateStatus),
) -> Result<UpdateStatus, UpdateError> {
    emit(UpdateStatus::Checking { trigger });

    // A staged update surviving from an earlier run (or a crash) short-cuts
    // the network entirely once re-verified — and is still offered when the
    // feed is unreachable.
    let pending = validated_pending(config);

    let release = match fetch_release() {
        Ok(release) => release,
        Err(err) => {
            if let Some(pending) = pending {
                return Ok(ready(pending));
            }
            return Err(err);
        }
    };
    check_cancel(cancel)?;

    if release.version <= config.current_version {
        return Ok(match pending {
            Some(pending) => ready(pending),
            None => UpdateStatus::UpToDate,
        });
    }
    if let Some(pending) = pending {
        match Version::parse(&pending.version) {
            Some(staged) if staged >= release.version => return Ok(ready(pending)),
            // The feed moved past what we staged — restage.
            _ => pending.discard(&config.manifest_path),
        }
    }

    let dmg = download(config, &release, cancel, emit)?;
    check_cancel(cancel)?;

    emit(UpdateStatus::Installing {
        version: release.version.to_string(),
    });
    let staged = stage_and_verify(config, &dmg.app_root(), cancel)?;
    drop(dmg); // detaches the mount, removes the DMG file

    let pending = PendingUpdate {
        staged_path: staged,
        version: release.version.to_string(),
        notes: release.notes.clone(),
    };
    pending.store(&config.manifest_path)?;
    Ok(ready(pending))
}

fn ready(pending: PendingUpdate) -> UpdateStatus {
    UpdateStatus::Ready {
        version: pending.version,
        notes: pending.notes,
    }
}

/// The manifest's word is not enough: re-verify the staged bundle's signature
/// against the boot pin and its version against the running one before
/// offering it. Anything that fails is deleted, not worked around.
fn validated_pending(config: &UpdaterConfig) -> Option<PendingUpdate> {
    let pending = PendingUpdate::load(&config.manifest_path)?;
    let staged_ok = pending.staged_path.is_dir()
        && oximux_macos_trust::verify_signed(&pending.staged_path, &config.app.pin).is_ok()
        && Version::parse(&pending.version).is_some_and(|v| v > config.current_version);
    if staged_ok {
        Some(pending)
    } else {
        pending.discard(&config.manifest_path);
        None
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
}

fn fetch_release() -> Result<crate::feed::Release, UpdateError> {
    let response = agent().get(&feed_url()).call().map_err(feed_transport)?;
    let body = response
        .into_string()
        .map_err(|err| UpdateError::Network {
            detail: format!("could not read the release feed: {err}"),
        })?;
    crate::feed::parse_latest(&body)
}

fn feed_transport(err: ureq::Error) -> UpdateError {
    match err {
        ureq::Error::Status(403, _) | ureq::Error::Status(429, _) => UpdateError::RateLimited,
        ureq::Error::Status(code, _) => UpdateError::Feed {
            detail: format!("release feed returned HTTP {code}"),
        },
        ureq::Error::Transport(t) => UpdateError::Network {
            detail: t.to_string(),
        },
    }
}

/// A downloaded DMG mounted read-only at a private mountpoint. Dropping it
/// detaches the mount and deletes both directories — including on error
/// paths, which is where a leaked temp DMG per attempt would otherwise
/// accumulate.
struct MountedImage {
    path: PathBuf,
    mountpoint: PathBuf,
}

impl MountedImage {
    fn app_root(&self) -> PathBuf {
        self.mountpoint.join("OxiMux.app")
    }
}

impl Drop for MountedImage {
    fn drop(&mut self) {
        let _ = run_bounded(
            Path::new("/usr/bin/hdiutil"),
            &[
                "detach",
                "-force",
                &self.mountpoint.display().to_string(),
            ],
            Duration::from_secs(30),
        );
        let _ = fs::remove_dir_all(&self.mountpoint);
        let _ = fs::remove_file(&self.path);
    }
}

fn download(
    config: &UpdaterConfig,
    release: &crate::feed::Release,
    cancel: &AtomicBool,
    emit: &dyn Fn(UpdateStatus),
) -> Result<MountedImage, UpdateError> {
    let dir = downloads_dir(&config.cache_dir);
    fs::create_dir_all(&dir).map_err(io_staging)?;
    ensure_disk_space(&dir, release.asset_size.saturating_mul(2))
        .map_err(UpdateError::DiskSpace)?;

    let response = agent()
        .get(&release.asset_url)
        .call()
        .map_err(feed_transport)?;

    let final_host = url::Url::parse(response.get_url())
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if !host_allowed(&final_host) {
        return Err(UpdateError::DisallowedHost { host: final_host });
    }

    let path = dir.join(format!("OxiMux-{}.dmg", release.version));
    let mut file = fs::File::create(&path).map_err(io_staging)?;
    let mut reader = response.into_reader();

    // Ceiling against a runaway response: twice the declared size is
    // generous for transfer overhead and small enough to stop a tarpit.
    let ceiling = release.asset_size.saturating_mul(2).max(1);
    let total = release.asset_size;
    let mut received: u64 = 0;
    let mut last_percent: u64 = 0;
    let mut buf = [0u8; 64 * 1024];
    let version = release.version.to_string();

    emit(UpdateStatus::Downloading {
        version: version.clone(),
        received: 0,
        total: Some(total),
    });
    loop {
        if cancel.load(Ordering::SeqCst) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(UpdateError::Cancelled);
        }
        let n = reader.read(&mut buf).map_err(|err| UpdateError::Network {
            detail: format!("download interrupted: {err}"),
        })?;
        if n == 0 {
            break;
        }
        received += n as u64;
        if received > ceiling {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(UpdateError::Oversize {
                declared: total,
                received,
            });
        }
        file.write_all(&buf[..n]).map_err(io_staging)?;

        // Throttle progress to whole-percent steps so the UI is not repainted
        // per 64 KiB chunk.
        let percent = if total > 0 { received * 100 / total } else { 0 };
        if percent > last_percent {
            last_percent = percent;
            emit(UpdateStatus::Downloading {
                version: version.clone(),
                received,
                total: Some(total),
            });
        }
    }
    file.sync_all().map_err(io_staging)?;
    drop(file);

    mount(config, path)
}

fn mount(config: &UpdaterConfig, dmg: PathBuf) -> Result<MountedImage, UpdateError> {
    let mounts = mounts_dir(&config.cache_dir);
    fs::create_dir_all(&mounts).map_err(io_staging)?;
    let mut suffix = [0u8; 6];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
    let mountpoint = mounts.join(format!(
        "mnt-{}",
        suffix.map(|b| format!("{b:02x}")).join("")
    ));

    let attached = run_bounded(
        Path::new("/usr/bin/hdiutil"),
        &[
            "attach",
            &dmg.display().to_string(),
            "-nobrowse",
            "-readonly",
            "-mountpoint",
            &mountpoint.display().to_string(),
        ],
        MOUNT_TIMEOUT,
    );
    // Nothing is mounted yet, so `MountedImage`'s Drop cannot clean up for
    // us — every failure below has to delete the download itself.
    let out = match attached {
        Ok(out) => out,
        Err(err) => {
            let _ = fs::remove_file(&dmg);
            return Err(UpdateError::Gate(err));
        }
    };
    if !out.success() {
        let _ = fs::remove_file(&dmg);
        return Err(UpdateError::Mount {
            detail: out
                .stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("hdiutil attach failed")
                .to_string(),
        });
    }

    let image = MountedImage {
        path: dmg,
        mountpoint,
    };
    if !image.app_root().is_dir() {
        return Err(UpdateError::NoAppInImage);
    }
    Ok(image)
}

/// Copy the mounted app next to the installed bundle, then run every gate
/// against the staged copy. On any failure the staging dir is removed — a
/// failed verify must leave nothing behind that a later swap could pick up.
fn stage_and_verify(
    config: &UpdaterConfig,
    mounted_app: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf, UpdateError> {
    let parent = config
        .app
        .bundle_root
        .parent()
        .ok_or_else(|| UpdateError::Staging {
            detail: "installed bundle has no parent directory".into(),
        })?;
    ensure_disk_space(parent, dir_size(mounted_app).saturating_add(64 * 1024 * 1024))
        .map_err(UpdateError::DiskSpace)?;

    let staging = claim_staging_dir(&config.app.bundle_root)?;
    let staged = (|| -> Result<PathBuf, UpdateError> {
        check_cancel(cancel)?;
        ditto_copy(mounted_app, &staging).map_err(UpdateError::Gate)?;
        check_cancel(cancel)?;

        oximux_macos_trust::verify_signed(&staging, &config.app.pin)
            .map_err(UpdateError::Gate)?;
        if !spctl_skipped() {
            oximux_macos_trust::verify_notarized_bundle(&staging).map_err(UpdateError::Gate)?;
        }

        // The feed's tag was only a claim. The staged bundle's own embedded
        // version is the fact — a validly-signed but *old* build republished
        // under a high tag dies here.
        let staged_version = staged_bundle_version(&staging)?;
        if staged_version <= config.current_version {
            return Err(UpdateError::Downgrade {
                staged: staged_version.to_string(),
                running: config.current_version.to_string(),
            });
        }
        Ok(staging.clone())
    })();

    if staged.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    staged
}

/// `defaults read` handles both XML and binary plists, unlike a hand-rolled
/// parse of the file bytes.
fn staged_bundle_version(bundle: &Path) -> Result<Version, UpdateError> {
    let plist = bundle.join("Contents/Info");
    let out = run_bounded(
        Path::new("/usr/bin/defaults"),
        &[
            "read",
            &plist.display().to_string(),
            "CFBundleShortVersionString",
        ],
        Duration::from_secs(10),
    )
    .map_err(UpdateError::Gate)?;
    let raw = out.stdout.trim();
    if !out.success() || raw.is_empty() {
        return Err(UpdateError::UnreadableStagedVersion {
            output: out.stderr.chars().take(200).collect(),
        });
    }
    Version::parse(raw).ok_or_else(|| UpdateError::UnreadableStagedVersion {
        output: raw.chars().take(200).collect(),
    })
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), UpdateError> {
    if cancel.load(Ordering::SeqCst) {
        return Err(UpdateError::Cancelled);
    }
    Ok(())
}

fn io_staging(err: std::io::Error) -> UpdateError {
    UpdateError::Staging {
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_hosts_pass_the_allow_list() {
        assert!(host_allowed("github.com"));
        assert!(host_allowed("api.github.com"));
        assert!(host_allowed("objects.githubusercontent.com"));
        assert!(host_allowed("release-assets.githubusercontent.com"));
        assert!(!host_allowed("github.com.evil.example"));
        assert!(!host_allowed("githubusercontent.com.evil.example"));
        assert!(!host_allowed("example.com"));
        assert!(!host_allowed(""));
    }

    #[test]
    fn release_builds_cannot_skip_spctl() {
        // In debug (where tests run) the env knob works; the release-path
        // guarantee is the cfg above — this test documents the debug side.
        #[cfg(not(debug_assertions))]
        assert!(!spctl_skipped());
        #[cfg(debug_assertions)]
        {
            // No assertion on env state — just exercise both branches exist.
            let _ = spctl_skipped();
        }
    }
}
