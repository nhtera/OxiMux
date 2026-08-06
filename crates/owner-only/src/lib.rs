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
//!
//! # Directories are not files, and Windows is not unix
//!
//! [`prepare_owner_only_dir`] exists because the app's whole data directory —
//! not just the individual secrets in it — has to be closed to other accounts.
//! The two platforms need different things from it:
//!
//! - On unix `0700` on the directory is sufficient on its own. Denying `x`
//!   denies traversal, so nothing inside is reachable by another user no matter
//!   how permissive its own mode is.
//! - On Windows it is **not** sufficient. `SeChangeNotifyPrivilege` ("bypass
//!   traverse checking") is granted to Everyone by default, so a restrictive
//!   DACL on a directory does not stop anyone who knows the full path to a file
//!   inside it. The directory's ACE is therefore written *inheritable*, so
//!   files created later carry it too — and files that already existed still
//!   need their own [`restrict_file`], because inheritance is applied at
//!   creation and never retroactively.

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
        windows_impl::set_owner_only_dacl(path, &owner_only_sddl()?)
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

/// Create `dir` if it does not exist, restrict it to the account running this
/// process, and **verify by readback** that it stayed that way.
///
/// The one call sites should reach for. A directory holding secrets is not
/// protected by the write having been issued — it is protected by the mode or
/// descriptor that is actually on it afterwards, which is a different claim and
/// the only one worth asserting. An unverifiable restriction is an error rather
/// than a warning: a caller that continues past this believing the directory is
/// closed is worse off than one that knows it is not.
///
/// Idempotent, so it is safe on every boot rather than only on first run — and
/// it must run on every boot, because the directory may pre-date the version
/// that started restricting it.
pub fn prepare_owner_only_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir)?;
    if !is_dir_restricted_to_owner(dir)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not owner-only after restricting it",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Restrict a directory so only the account running this process can enter it.
///
/// See the module docs for why this alone is enough on unix and is not on
/// Windows.
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        windows_impl::set_owner_only_dacl(path, &owner_only_dir_sddl()?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no way to restrict directory access on this platform",
        ))
    }
}

/// Whether `path` is a directory only the running account can enter.
///
/// The directory counterpart of [`is_restricted_to_owner`] — `0700` rather than
/// `0600`, since a directory without `x` cannot be traversed even by its owner.
pub fn is_dir_restricted_to_owner(path: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777 == 0o700)
    }
    #[cfg(windows)]
    {
        // Identical check to the file case: what matters is that the DACL is
        // protected and names us alone. The inheritance flags the directory
        // carries live inside the ACE and do not change who it admits.
        windows_dacl_names_only_us(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(false)
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
        windows_dacl_names_only_us(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Whether `path`'s DACL is protected and admits this account alone.
///
/// Shared by the file and directory readbacks: on Windows the question is the
/// same for both, since access there comes from the descriptor rather than from
/// mode bits that mean different things per object type.
#[cfg(windows)]
fn windows_dacl_names_only_us(path: &Path) -> io::Result<bool> {
    let sddl = dacl_sddl(path)?;
    // Protected, or ACEs inherited from the parent can widen this after the
    // fact.
    if !sddl.starts_with("D:P") {
        return Ok(false);
    }
    let us = current_user_sid()?;
    let mut saw_an_ace = false;
    for token in ace_sids(&sddl) {
        saw_an_ace = true;
        // The ACE's SID cannot be compared as text: the serializer compresses
        // well-known accounts to aliases, so for a built-in Administrator the
        // SID we wrote reads back as `LA`. Canonicalize before comparing — and
        // treat a token that will not canonicalize as one we cannot vouch for,
        // which answers "is this restricted?" in the safe direction rather than
        // failing the whole call over a descriptor we did not write.
        match windows_impl::canonical_sid(token) {
            Ok(sid) if sid == us => {}
            _ => return Ok(false),
        }
    }
    // An empty DACL denies everyone, us included. That is not "restricted to
    // the owner", it is an unusable object, and reporting it as restricted
    // would hand the caller a directory it cannot write to.
    Ok(saw_an_ace)
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

/// The same DACL for a directory, with the ACE marked inheritable.
///
/// `OICI` — object-inherit, container-inherit — so files and subdirectories
/// created inside carry it too. Necessary rather than tidy: bypass-traverse-
/// checking means a restrictive descriptor on the directory does not protect
/// what is inside it, so without inheritance the directory's own DACL would
/// guard nothing but the directory entry itself.
///
/// Inheritance applies at creation time only. Files that pre-date the call
/// still need [`restrict_file`].
#[cfg(windows)]
pub fn owner_only_dir_sddl() -> io::Result<String> {
    Ok(format!(
        "D:P(A;OICI;GA;;;{})",
        windows_impl::current_user_sid()?
    ))
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
/// Note that the string will not match [`owner_only_sddl`] verbatim, and not
/// only in its mask spelling. Windows expands the generic rights in a stored ACE
/// to the object-specific mask, so the `GA` that went in reads back as `FA` on a
/// file — and on a *directory* an inheritable ACE comes back as two, an
/// effective one plus an inherit-only one. Assert on the protected flag and on
/// the SIDs the ACEs name; not on the mask spelling, and not on how many ACEs
/// the object type happened to need.
#[cfg(windows)]
pub fn dacl_sddl(path: &Path) -> io::Result<String> {
    windows_impl::read_dacl_sddl(path)
}

#[cfg(windows)]
mod windows_impl;

/// The SID token of every ACE in an SDDL DACL — both `LA`s of
/// `D:PAI(A;;FA;;;LA)(A;OICIIO;GA;;;LA)`.
///
/// Every ACE rather than the first, and every ACE *type* rather than allow
/// alone, because the question this answers is "does this descriptor name
/// anyone but us", and an ACE of any type naming a stranger is a stranger the
/// descriptor knows about.
///
/// Counting them is not the same question and is not a stable one: Windows
/// normalizes a single inheritable ACE on a container into two — an effective
/// ACE for the directory itself with the generic rights mapped to specific, and
/// an inherit-only ACE carrying the generic rights down to what is created
/// inside. One write, two ACEs, and the count is a property of the object type
/// rather than of who is admitted.
///
/// A malformed ACE yields whatever sits in its last field, empty string
/// included, rather than being skipped — an ACE that cannot be read is not an
/// ACE that can be dismissed.
///
/// Compiled outside Windows only for its test. The parsing is pure string work
/// and nothing about it is platform-specific, so gating it away from the other
/// platforms' test runs would put the half of this that *can* be checked
/// anywhere into the half that only CI's Windows runner ever executes.
#[cfg(any(windows, test))]
fn ace_sids(sddl: &str) -> impl Iterator<Item = &str> {
    sddl.split('(')
        .skip(1)
        .filter_map(|rest| rest.split_once(')'))
        .filter_map(|(ace, _)| ace.rsplit(';').next())
}

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

    #[test]
    fn preparing_a_dir_creates_it_restricted_and_keeps_us_in() {
        // The whole contract in one place: it appears, it is closed to others,
        // and we can still write to it. A restriction that locked out the owner
        // would satisfy every "not world readable" assertion and brick the app.
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("nested").join("data");

        prepare_owner_only_dir(&dir).expect("prepare");

        assert!(dir.is_dir(), "the directory should have been created");
        assert!(is_dir_restricted_to_owner(&dir).expect("read back"));
        fs::write(dir.join("probe"), b"x").expect("owner must retain write access");
    }

    #[test]
    fn preparing_an_existing_wide_open_dir_closes_it() {
        // The upgrade path, and the reason this runs on every boot rather than
        // only on first run: the directory already exists, created by a build
        // that never restricted it.
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("data");
        fs::create_dir_all(&dir).expect("seed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("widen");
            assert!(!is_dir_restricted_to_owner(&dir).expect("read back"));
        }
        fs::write(dir.join("oximux.db"), b"transcripts").expect("seed file");

        prepare_owner_only_dir(&dir).expect("prepare");

        assert!(is_dir_restricted_to_owner(&dir).expect("read back"));
        assert_eq!(
            fs::read(dir.join("oximux.db")).expect("contents survive"),
            b"transcripts".to_vec(),
            "hardening must not disturb what is already in the directory"
        );
    }

    #[test]
    fn preparing_is_idempotent() {
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("data");
        prepare_owner_only_dir(&dir).expect("first");
        prepare_owner_only_dir(&dir).expect("second run must be a no-op, not an error");
        assert!(is_dir_restricted_to_owner(&dir).expect("read back"));
    }

    #[cfg(unix)]
    #[test]
    fn a_dir_lands_on_0700_not_0600() {
        // 0600 would pass a naive "no group/other bits" check and still be
        // useless: without `x` the owner cannot traverse its own directory.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("d");
        fs::create_dir(&dir).expect("mkdir");

        restrict_dir(&dir).expect("restrict");

        let mode = fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn the_dir_predicate_rejects_a_traversable_dir() {
        // Guards the predicate: one that returned true unconditionally would
        // make the readback in `prepare_owner_only_dir` vacuous.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("d");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("widen");

        assert!(!is_dir_restricted_to_owner(&dir).expect("read back"));
    }

    #[cfg(windows)]
    #[test]
    fn a_dir_dacl_is_protected_and_inheritable() {
        // Inheritance is the load-bearing part on Windows: bypass-traverse-
        // checking means the directory's own descriptor does not protect the
        // files inside it, so the ACE has to flow down to them at creation.
        let base = tempfile::tempdir().expect("tempdir");
        let dir = base.path().join("d");
        fs::create_dir(&dir).expect("mkdir");

        restrict_dir(&dir).expect("restrict");

        let sddl = dacl_sddl(&dir).expect("read back");
        assert!(sddl.starts_with("D:P"), "must be protected: {sddl}");
        assert!(
            sddl.contains("OICI"),
            "the ACE must be object- and container-inheritable: {sddl}"
        );
        // Deliberately not an ACE count. Writing one inheritable ACE on a
        // container yields two on readback — `D:PAI(A;;FA;;;LA)(A;OICIIO;GA;;;LA)`
        // — because Windows splits the effective grant from the inherit-only
        // one. An earlier version of this asserted a count of 1 and failed
        // against a perfectly correct descriptor.
        let sids: Vec<_> = ace_sids(&sddl)
            .map(|token| windows_impl::canonical_sid(token).expect("canonicalize"))
            .collect();
        let us = current_user_sid().expect("sid");
        assert!(!sids.is_empty(), "an empty DACL admits nobody: {sddl}");
        assert!(
            sids.iter().all(|sid| *sid == us),
            "every ACE must name {us}: {sddl}"
        );
        assert!(is_dir_restricted_to_owner(&dir).expect("predicate agrees"));
    }

    #[cfg(windows)]
    #[test]
    fn the_dir_sddl_names_the_running_user() {
        let sddl = owner_only_dir_sddl().expect("sddl");
        assert!(sddl.starts_with("D:P(A;OICI;GA;;;S-1-"), "got {sddl}");
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
        // A count of exactly one *is* meaningful here, where it is not on a
        // directory: a file takes no inherit-only ACE, so a second one would
        // mean ours was added alongside what the parent contributed rather than
        // displacing it.
        let mut tokens = ace_sids(&sddl);
        // Via `canonical_sid`, not `contains`: on an account the SDDL
        // serializer knows by name (CI runs as the built-in Administrator),
        // the ACE reads back as an alias like `LA` rather than the SID.
        let token = tokens.next().expect("the ACE carries a SID token");
        assert_eq!(
            windows_impl::canonical_sid(token).expect("canonicalize"),
            sid,
            "ACE must name {sid}: {sddl}"
        );
        assert!(
            tokens.next().is_none(),
            "expected exactly one ACE on a file, got {sddl}"
        );
    }

    /// Runs everywhere on purpose — see [`ace_sids`]. The shapes asserted here
    /// are transcribed from a real Windows readback, so the parser stays
    /// checkable on the platforms that develop it.
    #[test]
    fn ace_sids_finds_every_token_in_both_spellings() {
        assert_eq!(ace_sids("D:PAI(A;;FA;;;LA)").collect::<Vec<_>>(), ["LA"]);
        assert_eq!(
            ace_sids("D:P(A;;GA;;;S-1-5-21-1-2-3-500)").collect::<Vec<_>>(),
            ["S-1-5-21-1-2-3-500"]
        );
        // The shape a directory actually reads back as, and the one that broke
        // the count-based predicate: one effective ACE plus the inherit-only
        // ACE Windows splits out of it, both naming the same account.
        assert_eq!(
            ace_sids("D:PAI(A;;FA;;;LA)(A;OICIIO;GA;;;LA)").collect::<Vec<_>>(),
            ["LA", "LA"]
        );
        // A deny-ACE names an account too, and the predicate has to see it.
        assert_eq!(
            ace_sids("D:P(A;;FA;;;LA)(D;;FA;;;BA)").collect::<Vec<_>>(),
            ["LA", "BA"]
        );
        assert!(ace_sids("D:P").next().is_none(), "no ACE, no token");
        assert_eq!(
            ace_sids("D:PAI(A;;FA;;;)").collect::<Vec<_>>(),
            [""],
            "a malformed ACE is surfaced, not skipped — the predicate must \
             reject what it cannot read rather than pass over it"
        );
    }
}
