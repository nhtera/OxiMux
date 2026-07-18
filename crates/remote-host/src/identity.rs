//! The host's persistent Ed25519 identity.
//!
//! One key per workspace, its filename keyed on **SHA-256(workdir)** — never
//! `DefaultHasher`, whose output isn't stable across Rust versions (OxiMux has
//! already been bitten by that in the sidebar dot-colour hashing). The private
//! key file is written `0600`. This is the host's stable node identity; the iroh
//! transport key (added with the endpoint) is derived from or seeded by it later.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// A loaded host identity.
pub struct HostIdentity {
    signing: SigningKey,
}

impl HostIdentity {
    /// Load the identity for `workdir` from `dir`, generating and persisting a new
    /// one (`0600`) if none exists yet.
    pub fn load_or_generate(dir: &Path, workdir: &str) -> io::Result<Self> {
        let path = identity_path(dir, workdir);
        if let Ok(bytes) = fs::read(&path)
            && let Ok(key) = <[u8; 32]>::try_from(bytes.as_slice())
        {
            return Ok(Self { signing: SigningKey::from_bytes(&key) });
        }
        // A corrupt/short/absent key file: (re)generate rather than fail closed.
        // Seed from OS entropy directly (any 32 bytes is a valid Ed25519 seed),
        // avoiding ed25519-dalek's optional `rand_core` feature.
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let signing = SigningKey::from_bytes(&seed);
        persist(&path, signing.to_bytes())?;
        Ok(Self { signing })
    }

    /// The host's public key — its stable identity, safe to publish.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.verifying_key().to_bytes()
    }

    /// The verifying half, for server-side signatures.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// The signing key (e.g. to seed the iroh transport key later).
    pub fn signing_key(&self) -> &SigningKey {
        &self.signing
    }
}

/// `remote-host-<sha256(workdir)[..16]>.key` under `dir`. SHA-256 so the name is
/// stable across Rust/toolchain versions.
fn identity_path(dir: &Path, workdir: &str) -> PathBuf {
    let digest = Sha256::digest(workdir.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("remote-host-{hex}.key"))
}

/// Write the key bytes with `0600` perms (owner read/write only). On Unix the
/// file is *created* `0600` so the private key is never briefly world/group
/// readable in the window between write and chmod.
fn persist(path: &Path, bytes: [u8; 32]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(&bytes)?;
        // Re-assert perms in case the file pre-existed with looser bits (`mode`
        // only applies on creation).
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_reloads_the_same_key() {
        let dir = std::env::temp_dir().join(format!("oximux-host-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let first = HostIdentity::load_or_generate(&dir, "/repo/a").expect("generate");
        let reloaded = HostIdentity::load_or_generate(&dir, "/repo/a").expect("reload");
        assert_eq!(first.public_key_bytes(), reloaded.public_key_bytes(), "same key across loads");

        // A different workspace gets a distinct key file (distinct identity).
        let other = HostIdentity::load_or_generate(&dir, "/repo/b").expect("other");
        assert_ne!(first.public_key_bytes(), other.public_key_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = identity_path(&dir, "/repo/a");
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file is 0600");
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
