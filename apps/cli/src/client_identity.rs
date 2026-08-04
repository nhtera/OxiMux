//! The CLI's app-signing identity, **one key per host**.
//!
//! Mirrors `oximux_remote_host::identity`'s file+0600+readback pattern rather
//! than the phone's storage: `mobile-core` keeps its seed in the OS keystore
//! and rebuilds via `ClientSigner::from_seed`, which has no portable
//! equivalent here.
//!
//! **The weakening is real and is not papered over.** A raw Ed25519 seed in a
//! file is a weaker threat model than a hardware-backed keystore, and a CLI is
//! more likely than a phone to be sitting on a shared server. Three things
//! bound it: the file is owner-only and *verified so by readback* (a write that
//! cannot be restricted fails rather than proceeding), keys are per host so a
//! compromised enrollment does not transfer to the rest of the fleet, and
//! `oximux hosts rm` unpairs host-side so revocation does not depend on the
//! file. If OS-keyring integration is ever wanted for laptop installs, this is
//! the module it replaces.

use std::path::{Path, PathBuf};

use oximux_remote_session::ClientSigner;
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

use crate::cli::exit;
use crate::output::Failure;

/// Load this machine's signing identity for `host_name`, generating and
/// persisting one if it has none yet.
pub fn load_or_generate(dir: &Path, host_name: &str) -> Result<ClientSigner, Failure> {
    let path = seed_path(dir, host_name);
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return Ok(ClientSigner::from_seed(&seed));
    }
    // A short or corrupt file is regenerated rather than failing closed. The
    // consequence is honest and recoverable: the new public key is not the one
    // the host paired, so the next call is refused and the user re-pairs —
    // which beats a CLI that cannot start at all.
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    persist(&path, &seed)?;
    Ok(ClientSigner::from_seed(&seed))
}

/// Forget a host's key, so `hosts rm` leaves nothing behind that could
/// authenticate. Absent is success.
pub fn forget(dir: &Path, host_name: &str) {
    let _ = std::fs::remove_file(seed_path(dir, host_name));
}

/// `client-<sha256(host_name)[..16]>.key`. Hashed rather than using the name
/// directly so a host called `../../etc/passwd` cannot pick the path, and
/// SHA-256 rather than `DefaultHasher` because that one's output is not stable
/// across Rust versions — a rotating filename would silently mint a new
/// identity and un-pair the user on a toolchain bump.
fn seed_path(dir: &Path, host_name: &str) -> PathBuf {
    let digest = Sha256::digest(host_name.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("client-{hex}.key"))
}

/// Write the seed owner-only, and **prove** it. The restriction is asserted by
/// readback rather than assumed from the create flags: on Windows there is no
/// create-time equivalent of `mode`, and on unix a pre-existing file keeps its
/// old permissions. A signing key other accounts can read is worse than no
/// remote access at all, so a failure here propagates.
fn persist(path: &Path, seed: &[u8; 32]) -> Result<(), Failure> {
    let io = |what: &str, e: std::io::Error| {
        Failure::new(
            "identity",
            exit::ERROR,
            format!("could not {what} the client key {}: {e}", path.display()),
        )
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io("create the directory for", e))?;
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Created 0600 so there is no window where the key is readable.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| io("write", e))?;
        f.write_all(seed).map_err(|e| io("write", e))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, seed).map_err(|e| io("write", e))?;
    }
    oximux_owner_only::restrict_file(path).map_err(|e| io("restrict", e))?;
    // The receipt. `restrict_file` succeeding is not the same as the file being
    // restricted — this is the only check that actually looks.
    match oximux_owner_only::is_restricted_to_owner(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Failure::new(
            "identity",
            exit::ERROR,
            format!("{} is readable by other accounts", path.display()),
        )
        .with_steps(["a signing key must be owner-only; fix the directory's permissions".into()])),
        Err(e) => Err(io("verify the permissions of", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_persists_and_reloads_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_generate(dir.path(), "server").expect("generate");
        let again = load_or_generate(dir.path(), "server").expect("reload");
        assert_eq!(first.public_key(), again.public_key(), "same identity across runs");
    }

    /// Per host, so a compromised enrollment does not transfer across the fleet.
    #[test]
    fn each_host_gets_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_generate(dir.path(), "a").expect("a");
        let b = load_or_generate(dir.path(), "b").expect("b");
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn the_seed_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        load_or_generate(dir.path(), "server").expect("generate");
        let path = seed_path(dir.path(), "server");
        assert!(
            oximux_owner_only::is_restricted_to_owner(&path).unwrap(),
            "a signing seed must not be readable by other accounts"
        );
    }

    /// A truncated file regenerates rather than failing the whole CLI. The user
    /// re-pairs; they are not locked out.
    #[test]
    fn a_corrupt_seed_regenerates_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_path(dir.path(), "server");
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(&path, b"short").unwrap();
        let signer = load_or_generate(dir.path(), "server").expect("regenerate");
        assert_eq!(signer.public_key().len(), 32);
    }

    /// A host name that looks like a path must not select the path.
    #[test]
    fn a_traversing_host_name_cannot_escape_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = seed_path(dir.path(), "../../etc/passwd");
        assert_eq!(path.parent(), Some(dir.path()), "the name is hashed, not joined");
    }

    #[test]
    fn forgetting_a_key_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        load_or_generate(dir.path(), "server").expect("generate");
        forget(dir.path(), "server");
        forget(dir.path(), "server");
        assert!(!seed_path(dir.path(), "server").exists());
    }
}
