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

pub mod archive;
pub mod download;
pub mod manifest;
pub mod swap;
#[cfg(test)]
pub mod testkit;
pub mod verify;

use std::path::{Path, PathBuf};

use crate::cli::exit;
use crate::output::Failure;
use download::Fetcher;
use manifest::Manifest;

/// A manifest is a few hundred bytes of JSON; a signature is four lines.
/// Ceilings this tight mean a tarpit is refused before it can matter.
const MANIFEST_CEILING: u64 = 256 * 1024;
const SIGNATURE_CEILING: u64 = 8 * 1024;

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

#[derive(Debug)]
pub enum UpdateError {
    Network { detail: String },
    /// The release exists but has no manifest, or there is no release at all.
    NoRelease,
    RateLimited,
    DisallowedHost { host: String },
    Oversize { ceiling: u64 },
    Archive { detail: String },
    Staging { detail: String },
    /// Installed by a package manager that owns the files.
    ManagedInstall { manager: &'static str, command: String },
    Manifest(manifest::ManifestError),
    Verify(verify::VerifyError),
    Swap(swap::SwapError),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network { detail } => write!(f, "could not reach the release server: {detail}"),
            Self::NoRelease => write!(f, "no published release carries an update manifest"),
            Self::RateLimited => write!(f, "the release server is rate-limiting this machine"),
            Self::DisallowedHost { host } => {
                write!(f, "the download was redirected to {host}, which is not a release host")
            }
            Self::Oversize { ceiling } => {
                write!(f, "the download exceeded its {ceiling}-byte ceiling and was abandoned")
            }
            Self::Archive { detail } => write!(f, "{detail}"),
            Self::Staging { detail } => write!(f, "{detail}"),
            Self::ManagedInstall { manager, .. } => {
                write!(f, "this oximux was installed by {manager}, which owns the files")
            }
            Self::Manifest(err) => write!(f, "{err}"),
            Self::Verify(err) => write!(f, "{err}"),
            Self::Swap(err) => write!(f, "{err}"),
        }
    }
}

impl From<manifest::ManifestError> for UpdateError {
    fn from(err: manifest::ManifestError) -> Self {
        Self::Manifest(err)
    }
}
impl From<verify::VerifyError> for UpdateError {
    fn from(err: verify::VerifyError) -> Self {
        Self::Verify(err)
    }
}
impl From<swap::SwapError> for UpdateError {
    fn from(err: swap::SwapError) -> Self {
        Self::Swap(err)
    }
}

impl UpdateError {
    /// Map onto the CLI's shared failure vocabulary. "Already current" is not
    /// an error and never reaches here.
    pub fn into_failure(self) -> Failure {
        let code = match &self {
            Self::Network { .. } | Self::NoRelease | Self::RateLimited => "unreachable",
            Self::Verify(_) | Self::DisallowedHost { .. } => "untrusted",
            _ => "update",
        };
        let exit = match &self {
            Self::Network { .. } | Self::NoRelease | Self::RateLimited => exit::UNREACHABLE,
            _ => exit::ERROR,
        };
        let steps = self.next_steps();
        Failure::new(code, exit, self.to_string()).with_steps(steps)
    }

    fn next_steps(&self) -> Vec<String> {
        match self {
            Self::ManagedInstall { command, .. } => vec![command.clone()],
            Self::Swap(err) => err.next_steps(),
            Self::Verify(verify::VerifyError::NotAnUpgrade { .. }) => {
                vec!["You are already on this version or newer; nothing to do.".into()]
            }
            Self::Verify(verify::VerifyError::NoTrustRoot) => vec![
                "Reinstall from a release build, which carries the key: ".to_string()
                    + INSTALL_HINT,
            ],
            Self::Verify(_) | Self::DisallowedHost { .. } => vec![
                "Do not retry blindly — this is what a tampered release looks like. Check \
                 https://github.com/nhtera/OxiMux/releases before installing anything."
                    .into(),
            ],
            Self::Manifest(manifest::ManifestError::NoAssetForTarget { .. }) => {
                vec!["Build from source for this platform, or open an issue asking for it.".into()]
            }
            Self::Network { .. } | Self::NoRelease | Self::RateLimited => {
                vec!["Check network access to github.com, then retry.".into()]
            }
            _ => vec![format!("Reinstall instead: {INSTALL_HINT}")],
        }
    }
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

/// Fetch the manifest and prove it before parsing a single field of it.
pub fn fetch_verified_manifest(
    fetcher: &dyn Fetcher,
    public_key: Option<&str>,
) -> Result<Manifest, UpdateError> {
    // Fail on a keyless build before spending two requests on bytes that
    // could never be trusted.
    if public_key.is_none() {
        return Err(verify::VerifyError::NoTrustRoot.into());
    }
    let raw = fetcher.get(&download::manifest_url(), MANIFEST_CEILING)?;
    let signature = fetcher.get(&download::signature_url(), SIGNATURE_CEILING)?;
    let signature = String::from_utf8(signature).map_err(|_| {
        UpdateError::Verify(verify::VerifyError::MalformedSignature("not text".into()))
    })?;
    verify::verify_manifest_signature(&raw, &signature, public_key)?;
    Ok(Manifest::parse(&raw)?)
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

    let url = download::asset_url(&manifest.tag(), &asset.archive);
    // Twice the declared size: generous for transfer overhead, tight enough
    // to stop a stream that intends to never end.
    let bytes = fetcher.get(&url, asset.size.saturating_mul(2).max(1))?;
    verify::verify_digest(&bytes, &asset.sha256)?;

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

/// A staging directory that removes itself, so a failed verify leaves nothing
/// a later run could mistake for a verified binary.
struct Staging(PathBuf);

impl Staging {
    fn claim(dir: &Path) -> Result<Self, UpdateError> {
        let mut suffix = [0u8; 6];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut suffix);
        let hex: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!(".oximux.update-{hex}"));
        std::fs::create_dir(&path).map_err(|err| UpdateError::Staging {
            detail: format!("could not stage the update in {}: {err}", dir.display()),
        })?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
