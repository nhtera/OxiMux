//! Self-update: fetch a signed release manifest, prove it, and replace the
//! CLI and the relay together.
//!
//! The trust chain, in the order it must run and for the reason it must run in
//! that order:
//!
//! ```text
//! minisign signature over manifest.json   ← the only independent trust root
//!   └─ manifest parsed (never before)
//!        └─ version strictly greater than the running one
//!             └─ archive fetched, sha256 checked against the signed manifest
//!                  └─ extracted, platform gate, paired swap
//! ```
//!
//! The first step is what makes the rest worth anything. A sha256 taken from a
//! manifest published beside the artifact it describes proves only that the
//! download matches what the publisher said; a compromised publish token
//! rewrites both. The signature is checked against a key compiled into this
//! binary, which that token cannot reach.
//!
//! Nothing here ever restarts a running `oximux serve`. On unix the running
//! process keeps the image it already mapped, so a host stays up across the
//! swap and picks the new binary up whenever its service manager restarts it —
//! which is the service manager's decision, not this command's.
//!
//! Every step above except the last two lives in
//! [`oximux_auto_update::release`], shared with the desktop app's in-app
//! updater. What stays here is what is actually about *this* program: which two
//! binaries it owns, how they come out of a `.tar.gz`, and how a failure is
//! rendered into the CLI's JSON envelope.

pub mod archive;

use std::path::{Path, PathBuf};

use crate::cli::exit;
use crate::output::Failure;

pub use oximux_auto_update::release::{
    download, manifest, swap, verify, Staging, ReleaseError as UpdateError,
};
pub use oximux_auto_update::release::fetch_verified_manifest;

use download::Fetcher;


/// Printed whenever updating in place is not the answer.
pub const INSTALL_HINT: &str =
    "curl -fsSL https://raw.githubusercontent.com/nhtera/OxiMux/main/scripts/install-cli.sh | sh";

/// The command name users type, which is *not* the cargo bin name — the
/// desktop app owns `oximux` as a cargo target, so the CLI builds as
/// `oximux-cli` and is installed under this name.
pub fn cli_name() -> String {
    format!("oximux{}", std::env::consts::EXE_SUFFIX)
}

pub fn relay_name() -> String {
    format!("oximux-relay{}", std::env::consts::EXE_SUFFIX)
}

/// Map a release failure onto the CLI's shared failure vocabulary. "Already
/// current" is not an error and never reaches here.
///
/// A free function rather than a method, because [`UpdateError`] is the shared
/// [`oximux_auto_update::release::ReleaseError`] — the desktop app renders the
/// same failures as a line of text in its About pane, and neither rendering
/// belongs on the type both of them raise.
pub fn into_failure(err: UpdateError) -> Failure {
    use UpdateError as E;
    let code = match &err {
        E::Network { .. } | E::NoRelease | E::RateLimited => "unreachable",
        E::Verify(_) | E::DisallowedHost { .. } => "untrusted",
        _ => "update",
    };
    let exit = match &err {
        E::Network { .. } | E::NoRelease | E::RateLimited => exit::UNREACHABLE,
        _ => exit::ERROR,
    };
    let steps = next_steps(&err);
    Failure::new(code, exit, err.to_string()).with_steps(steps)
}

fn next_steps(err: &UpdateError) -> Vec<String> {
    use UpdateError as E;
    match err {
        E::ManagedInstall { command, .. } => vec![command.clone()],
        E::Swap(err) => swap_next_steps(err),
        E::Verify(verify::VerifyError::NotAnUpgrade { .. }) => {
            vec!["You are already on this version or newer; nothing to do.".into()]
        }
        E::Verify(verify::VerifyError::NoTrustRoot) => vec![
            "Reinstall from a release build, which carries the key: ".to_string() + INSTALL_HINT,
        ],
        E::Verify(_) | E::DisallowedHost { .. } => vec![
            "Do not retry blindly — this is what a tampered release looks like. Check \
             https://github.com/nhtera/OxiMux/releases before installing anything."
                .into(),
        ],
        E::Manifest(manifest::ManifestError::NoAssetForTarget { .. }) => {
            vec!["Build from source for this platform, or open an issue asking for it.".into()]
        }
        E::Network { .. } | E::NoRelease | E::RateLimited => {
            vec!["Check network access to github.com, then retry.".into()]
        }
        _ => vec![format!("Reinstall instead: {INSTALL_HINT}")],
    }
}

/// What to tell the user when the swap itself failed. A permission failure on
/// the install directory is the overwhelmingly common case and has a specific
/// answer; everything else falls back to reinstalling.
fn swap_next_steps(err: &swap::SwapError) -> Vec<String> {
    let Some((dir, _)) = err.context() else {
        // The inconsistent case, which is not about one directory: the error's
        // own message already names the backups a human has to move back.
        return vec![
            "Restore the binaries listed above by renaming them back, then reinstall.".to_string(),
            format!("Or reinstall from scratch: {INSTALL_HINT}"),
        ];
    };
    let mut steps = Vec::new();
    if err.is_permission_denied() {
        steps.push(format!(
            "You do not have write access to {}. Reinstall to a directory you own \
             (the installer defaults to ~/.local/bin), or update with the same \
             account that installed it.",
            dir.display()
        ));
    }
    steps.push(format!("Reinstall instead: {INSTALL_HINT}"));
    steps
}

/// The two binaries this installation owns, and the directory holding them.
#[derive(Debug, Clone)]
pub struct Install {
    pub dir: PathBuf,
    pub cli: PathBuf,
    pub relay: PathBuf,
}

impl Install {
    /// Locate the running installation. Symlinks are resolved first: the file
    /// that has to be replaced is the real one, and the relay is looked for
    /// beside *it*, matching how `relay-supervisor` resolves the daemon.
    pub fn discover() -> Result<Self, UpdateError> {
        let exe = std::env::current_exe().map_err(|err| UpdateError::Staging {
            detail: format!("could not find this binary on disk: {err}"),
        })?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        Self::at(&exe)
    }

    fn at(exe: &Path) -> Result<Self, UpdateError> {
        if let Some((manager, command)) = managed_by(exe) {
            return Err(UpdateError::ManagedInstall { manager, command });
        }
        let dir = exe
            .parent()
            .ok_or_else(|| UpdateError::Staging {
                detail: format!("{} has no parent directory", exe.display()),
            })?
            .to_path_buf();
        Ok(Self { cli: exe.to_path_buf(), relay: dir.join(relay_name()), dir })
    }
}

/// Package managers that own their files. Fighting them produces an
/// installation neither side can reason about, so the answer is their command,
/// not ours.
fn managed_by(exe: &Path) -> Option<(&'static str, String)> {
    let path = exe.to_string_lossy().replace('\\', "/");
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        return Some(("Homebrew", "brew upgrade oximux".to_string()));
    }
    if path.starts_with("/nix/store/") {
        return Some(("Nix", "Update through your Nix configuration.".to_string()));
    }
    None
}

/// What an update did, for the verb to render.
#[derive(Debug)]
pub struct Applied {
    pub from: String,
    pub to: String,
    /// Backups the swap could not delete because they are still running.
    /// Windows only; swept at the next start.
    pub deferred_cleanup: Vec<PathBuf>,
}

/// The whole pipeline. `current` is the running version, `target` the triple
/// whose asset belongs to this binary.
pub fn apply(
    fetcher: &dyn Fetcher,
    public_key: Option<&str>,
    current: &str,
    target: &str,
    install: &Install,
) -> Result<Applied, UpdateError> {
    let manifest = fetch_verified_manifest(fetcher, public_key)?;
    verify::verify_is_upgrade(&manifest.version, current)?;
    let asset = manifest.asset_for(target)?;
    let bytes = oximux_auto_update::release::fetch_verified_asset(fetcher, &manifest.tag(), asset)?;

    // Staged beside the installed binaries so every rename stays on one
    // filesystem — a cross-device rename is not atomic and would defeat the
    // all-or-nothing swap.
    let staging = Staging::claim(&install.dir)?;
    let unpacked = archive::extract(&bytes, staging.path())?;
    platform_gate(&unpacked.cli, &install.cli)?;

    let deferred_cleanup = swap::swap_all(&[
        swap::Replacement { installed: install.cli.clone(), staged: unpacked.cli },
        swap::Replacement { installed: install.relay.clone(), staged: unpacked.relay },
    ])?;

    Ok(Applied {
        from: current.to_string(),
        to: manifest.version.clone(),
        deferred_cleanup,
    })
}

/// macOS only: if the running binary is signed by a real Developer ID team,
/// the replacement must be signed by the same team.
///
/// Additive to the signature gate, never a substitute. Its value is the one
/// case a minisign signature cannot cover — a misused release key — and its
/// cost is bounded by `pinnable()`: an ad-hoc signature (what every local
/// `cargo build` gets, reported as `TeamIdentifier=not set`) is reproducible
/// by anyone, so pinning to it would be a gate any attacker satisfies while
/// breaking every developer's own build. Those pin nothing and pass.
#[cfg(target_os = "macos")]
fn platform_gate(candidate: &Path, running: &Path) -> Result<(), UpdateError> {
    let Ok(pin) = oximux_macos_trust::read_signature(running) else {
        return Ok(());
    };
    if !pin.pinnable() {
        return Ok(());
    }
    let found = oximux_macos_trust::read_signature(candidate).map_err(|err| UpdateError::Archive {
        detail: format!("the downloaded binary carries no readable signature: {err}"),
    })?;
    if found.team_id == pin.team_id {
        return Ok(());
    }
    Err(UpdateError::Archive {
        detail: format!(
            "the downloaded binary is signed by team {} but this installation is signed by {}",
            if found.team_id.is_empty() { "nobody" } else { &found.team_id },
            pin.team_id
        ),
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_gate(_candidate: &Path, _running: &Path) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(test)]
mod tests;
