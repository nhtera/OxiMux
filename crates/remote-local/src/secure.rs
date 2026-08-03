//! Permissions with receipts: every restriction is asserted by **readback**,
//! never assumed from the write — `owner-only`'s own doc names the
//! create→chmod window, so secrets here are created restricted and then
//! *verified* to have stayed that way (the `identity.rs` pattern, not the
//! relay-supervisor's create-then-chmod one).

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rand::RngCore;
use rand::rngs::OsRng;

/// Create (if needed) and lock down the runtime directory holding the socket
/// and token, then verify by readback that it is traversable by the owner
/// alone. Reachability is one of the two trust factors, so a directory that
/// cannot be verified is a hard error, not a warning.
pub(crate) fn prepare_runtime_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict {}", dir.display()))?;
        let mode = std::fs::metadata(dir)
            .with_context(|| format!("stat {}", dir.display()))?
            .permissions()
            .mode();
        if mode & 0o777 != 0o700 {
            bail!("{} is not owner-only after chmod (mode {mode:o})", dir.display());
        }
    }
    // Windows: the pipe never lives in this directory (the namespace is flat);
    // only the token file does, and that carries its own verified DACL.
    Ok(())
}

/// Restrict the just-bound socket file itself and verify by readback. The
/// directory already refuses traversal to other users; this closes the second
/// door anyway, so a future relocation of the socket cannot silently rely on
/// a guarantee that moved away with the directory.
#[cfg(unix)]
pub(crate) fn restrict_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict {}", path.display()))?;
    let mode = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o777 != 0o600 {
        bail!("{} is not owner-only after chmod (mode {mode:o})", path.display());
    }
    Ok(())
}

/// Mint a fresh control token: 32 CSPRNG bytes, hex — a bearer credential,
/// never to be logged.
pub fn generate_token() -> String {
    let mut raw = [0u8; 32];
    OsRng.fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

/// Write the token **created restricted** (unix `mode(0o600)` at open), then
/// assert via `owner-only` readback that it stayed that way — which also
/// catches a pre-existing looser file being reused.
pub fn write_token_file(path: &Path, token: &str) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("create {}", path.display()))?;
    f.write_all(token.as_bytes()).with_context(|| format!("write {}", path.display()))?;
    drop(f);
    oximux_owner_only::restrict_file(path)
        .with_context(|| format!("restrict {}", path.display()))?;
    if !oximux_owner_only::is_restricted_to_owner(path)
        .with_context(|| format!("verify {}", path.display()))?
    {
        bail!("{} is not owner-only after restriction", path.display());
    }
    Ok(())
}

/// Read the token back. `NotFound` means local access has never been enabled
/// on this host — callers map it to their "turn the toggle on" guidance.
pub fn read_token_file(path: &Path) -> io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_is_created_owner_only_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-v1.token");
        let token = generate_token();
        assert_eq!(token.len(), 64, "32 bytes hex");
        write_token_file(&path, &token).unwrap();
        assert!(oximux_owner_only::is_restricted_to_owner(&path).unwrap());
        assert_eq!(read_token_file(&path).unwrap(), token);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_is_owner_only_after_prepare() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("runtime");
        prepare_runtime_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// A pre-existing world-readable token file must come out restricted —
    /// the readback half of the contract.
    #[cfg(unix)]
    #[test]
    fn a_loose_existing_token_file_is_re_restricted() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-v1.token");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_token_file(&path, "fresh").unwrap();
        assert!(oximux_owner_only::is_restricted_to_owner(&path).unwrap());
        assert_eq!(read_token_file(&path).unwrap(), "fresh");
    }
}
