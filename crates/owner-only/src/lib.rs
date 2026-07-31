//! Restricting a file to the account that created it.
//!
//! The app writes several things to disk that are only meaningful as secrets:
//! the relay's auth token, the Remote Control private signing key. On unix each
//! of these is `0600` and that is the whole story. Windows has no mode bits, and
//! the file's access comes from ACEs inherited from whatever directory it landed
//! in — which is a policy set somewhere else, by someone else, and readable to
//! whoever that policy happens to admit.
//!
//! So on Windows the DACL is stated rather than inherited, and stated
//! *protected*, so inheritance cannot widen it afterwards. Callers get one
//! function that means "only I can read this" and does not make them care which
//! of those two mechanisms is in play.
//!
//! This lives in its own crate because the three callers — the desktop app, the
//! relay daemon, and the remote host — have no other crate in common. The
//! alternative was the same SID lookup written out three times, which is how
//! security code drifts.

use std::io;
use std::path::Path;

/// Restrict `path` so only the account running this process can read or write
/// it. The file must already exist.
///
/// Note the ordering hazard this cannot fix on its own: on both platforms there
/// is a window between creation and this call during which the file carries
/// default access. Callers writing a secret should create the file restricted
/// (unix: `OpenOptionsExt::mode`) and treat this as the assertion that it
/// *stayed* that way, which also covers the case where the file already existed
/// with looser access.
pub fn restrict_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        windows_impl::set_owner_only_dacl(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no way to restrict file access on this platform",
        ))
    }
}

/// Whether `path` is currently readable only by the account running this
/// process.
///
/// This is the readback that makes [`restrict_file`] checkable: the failure
/// mode being guarded is a restriction that looks applied and is not, which no
/// amount of inspecting the write path can rule out. Call sites that write a
/// secret assert with this rather than assuming their own call worked.
pub fn is_restricted_to_owner(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777 == 0o600)
    }
    #[cfg(windows)]
    {
        let sddl = dacl_sddl(path)?;
        // Protected, or inherited ACEs can widen it after the fact; exactly one
        // allow-ACE, or something was added alongside ours rather than
        // displacing what the directory contributed; and that ACE is us.
        Ok(sddl.starts_with("D:P")
            && sddl.matches("(A;").count() == 1
            && sddl.contains(&current_user_sid()?))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(false)
    }
}

/// SDDL granting the current user full control of an object and naming nobody
/// else.
///
/// Reads as: `D:` a DACL follows; `P` protected, so ACEs inherited from the
/// container cannot widen it; `(A;;GA;;;<sid>)` one allow-ACE granting
/// GENERIC_ALL to this user's SID.
///
/// Administrators and SYSTEM are deliberately absent. Nothing in the product
/// connects as either, and an account that can already take ownership of the
/// object gains nothing from being listed.
#[cfg(windows)]
pub fn owner_only_sddl() -> io::Result<String> {
    Ok(format!("D:P(A;;GA;;;{})", windows_impl::current_user_sid()?))
}

/// The SID of the account this process runs as (`S-1-5-21-…`).
#[cfg(windows)]
pub fn current_user_sid() -> io::Result<String> {
    windows_impl::current_user_sid()
}

/// Read `path`'s DACL back as SDDL.
///
/// Exists so tests can assert a file really is restricted instead of assuming
/// the write worked — the whole failure mode here is a descriptor that looks
/// applied and is not. Also useful when diagnosing a permission complaint from
/// a real install.
///
/// Note that the string will not match [`owner_only_sddl`] verbatim: Windows
/// expands the generic rights in a stored ACE to the object-specific mask, so
/// the `GA` that went in reads back as `FA` on a file. Assert on the protected
/// flag, the ACE count and the SID — not on the mask spelling.
#[cfg(windows)]
pub fn dacl_sddl(path: &Path) -> io::Result<String> {
    windows_impl::read_dacl_sddl(path)
}

#[cfg(windows)]
mod windows_impl;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn restricting_a_file_leaves_it_readable_by_us() {
        // The point of the call is to *keep* our own access while removing
        // everyone else's — a descriptor that locked out the owner too would
        // pass a "not world readable" check and still break the app.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("secret.token");
        fs::write(&path, b"s3cret").expect("write");

        restrict_file(&path).expect("restrict");

        assert_eq!(fs::read(&path).expect("read back"), b"s3cret");
    }

    #[test]
    fn restricting_a_missing_file_is_an_error() {
        // Callers use this to assert a secret is protected. Succeeding on a
        // path that does not exist would let a caller believe it had protected
        // something it never wrote.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = restrict_file(&dir.path().join("nope")).expect_err("must fail");
        assert!(
            matches!(err.kind(), io::ErrorKind::NotFound),
            "expected NotFound, got {err:?}"
        );
    }

    #[test]
    fn a_restricted_file_reads_back_as_restricted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("k");
        fs::write(&path, b"x").expect("write");

        restrict_file(&path).expect("restrict");

        assert!(is_restricted_to_owner(&path).expect("read back"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_lands_on_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("k");
        fs::write(&path, b"x").expect("write");
        // Start deliberately wide so the assertion is about this call.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        restrict_file(&path).expect("restrict");

        let mode = fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn a_wide_open_file_reads_back_as_unrestricted() {
        // Guards the predicate itself: one that returned true unconditionally
        // would make every call-site assertion below vacuous.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("k");
        fs::write(&path, b"x").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).expect("chmod");

        assert!(!is_restricted_to_owner(&path).expect("read back"));
    }

    #[cfg(windows)]
    #[test]
    fn the_sddl_names_the_running_user() {
        let sddl = owner_only_sddl().expect("sddl");
        assert!(sddl.starts_with("D:P(A;;GA;;;S-1-"), "got {sddl}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_lands_on_a_protected_single_owner_dacl() {
        // The assertion the plan asks for: read the descriptor back off the
        // file rather than trusting that the write took. A file created in a
        // directory with inheritable ACEs starts out with several of them, so
        // the ACE count is what proves they were displaced and not merely
        // added to.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("k");
        fs::write(&path, b"x").expect("write");

        restrict_file(&path).expect("restrict");

        let sddl = dacl_sddl(&path).expect("read back");
        let sid = current_user_sid().expect("sid");
        assert!(
            sddl.starts_with("D:P"),
            "DACL must be protected or inheritance can widen it later: {sddl}"
        );
        assert_eq!(
            sddl.matches("(A;").count(),
            1,
            "expected exactly one allow-ACE, got {sddl}"
        );
        assert!(sddl.contains(&sid), "ACE must name {sid}: {sddl}");
    }
}
