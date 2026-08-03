//! The Win32 half: naming the running account, and stamping a DACL that lists
//! only it onto a file.

use std::io;
use std::path::Path;

use widestring::{U16CStr, U16CString};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The only SDDL revision Windows defines.
const SDDL_REVISION_1: u32 = 1;

/// Replace `path`'s DACL with `sddl`, and mark it protected so the parent
/// directory's inheritable ACEs do not flow back in.
///
/// `SE_FILE_OBJECT` covers directories as well as files, so the only thing that
/// varies between the two is the descriptor the caller passes — a directory's
/// carries inheritance flags (see `owner_only_dir_sddl`), a file's does not.
pub(crate) fn set_owner_only_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    // Checked up front so a missing file is `NotFound` here exactly as it is on
    // unix, rather than whichever error code `SetNamedSecurityInfoW` happens to
    // pick — callers use that distinction to tell "not protected" from "not
    // there", and this crate's contract should not vary by platform.
    let _ = path.metadata()?;

    let descriptor = SecurityDescriptor::from_sddl(sddl)?;
    let dacl = descriptor.dacl()?;

    let mut wide_path = U16CString::from_os_str(path.as_os_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?
        .into_vec_with_nul();

    // SAFETY: `wide_path` is NUL-terminated and outlives the call; `dacl` points
    // into `descriptor`, which is still alive here. Owner, group and SACL are
    // null because the corresponding bits are absent from `securityinfo`.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// Read `path`'s DACL back as an SDDL string.
pub(crate) fn read_dacl_sddl(path: &Path) -> io::Result<String> {
    let mut wide_path = U16CString::from_os_str(path.as_os_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?
        .into_vec_with_nul();

    let mut raw: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: `wide_path` is NUL-terminated and outlives the call. Only the
    // descriptor out-param is requested; the individual owner/group/ACL
    // out-params are null because they would point into the same block, which
    // `SecurityDescriptor`'s drop frees as a unit.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut raw,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = SecurityDescriptor(raw);

    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: `descriptor` is live across the call; the returned string is
    // LocalAlloc'd and freed below.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `out` is a NUL-terminated string the callee allocated.
    let sddl = unsafe {
        let s = U16CStr::from_ptr_str(out).to_string_lossy();
        LocalFree(out.cast());
        s
    };
    Ok(sddl)
}

/// The SID of the account this process runs as, in string form (`S-1-5-21-…`).
pub(crate) fn current_user_sid() -> io::Result<String> {
    // SAFETY: every call is checked before its output is used, the token handle
    // is closed by `OwnedHandle` on all exit paths, and the SID string is freed
    // with `LocalFree` as `ConvertSidToStringSidW` requires. Buffer alignment is
    // handled by `TokenBuffer` — see its comment.
    unsafe {
        let mut raw_token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(raw_token);

        // The first call is expected to fail with ERROR_INSUFFICIENT_BUFFER; its
        // purpose is to report the size through `needed`.
        let mut needed: u32 = 0;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut buf = TokenBuffer::with_capacity(needed as usize);
        if GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr(), needed, &mut needed) == 0 {
            return Err(io::Error::last_os_error());
        }

        // The buffer holds a TOKEN_USER whose `Sid` points elsewhere within it.
        let token_user = &*(buf.as_mut_ptr() as *const TOKEN_USER);
        let mut sid_str = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = U16CStr::from_ptr_str(sid_str).to_string_lossy();
        LocalFree(sid_str.cast());
        Ok(sid)
    }
}

/// Canonicalize an SDDL SID token to its literal `S-1-…` spelling.
///
/// The SDDL serializer compresses well-known accounts to two-letter aliases —
/// on a machine whose user is the built-in Administrator, the ACE written as
/// `S-1-5-21-…-500` reads back as `LA` — so a readback can never be compared
/// against [`current_user_sid`] as text. Round-tripping the token through a
/// real SID yields one spelling for both sides of that comparison.
pub(crate) fn canonical_sid(token: &str) -> io::Result<String> {
    let wide = U16CString::from_str(token)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "SID token contains a NUL"))?;
    // SAFETY: `wide` is NUL-terminated and outlives the call; both out-buffers
    // are LocalAlloc'd by their callee and freed here on every path.
    unsafe {
        let mut sid: PSID = std::ptr::null_mut();
        if ConvertStringSidToSidW(wide.as_ptr(), &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sid_str: *mut u16 = std::ptr::null_mut();
        let ok = ConvertSidToStringSidW(sid, &mut sid_str);
        LocalFree(sid.cast());
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let s = U16CStr::from_ptr_str(sid_str).to_string_lossy();
        LocalFree(sid_str.cast());
        Ok(s)
    }
}

/// A security descriptor parsed from SDDL, freed on drop.
struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = U16CString::from_str(sddl)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "SDDL contains a NUL"))?;
        let mut raw: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call. On success the
        // callee hands back a LocalAlloc'd block now owned by the return value.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut raw,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }

    /// The DACL inside this descriptor. The pointer borrows from `self`, so it
    /// must not outlive it — the lifetime tie is what keeps the descriptor alive
    /// across the `SetNamedSecurityInfoW` call that consumes the pointer.
    fn dacl(&self) -> io::Result<*const ACL> {
        let mut present = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: `self.0` is a descriptor from a successful parse above.
        let ok = unsafe {
            GetSecurityDescriptorDacl(self.0, &mut present, &mut dacl, &mut defaulted)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if present == 0 || dacl.is_null() {
            // `D:P(A;;GA;;;<sid>)` always carries a DACL, so this means the SDDL
            // built upstream lost its `D:` clause. Refuse rather than call
            // SetNamedSecurityInfo with a null DACL — that grants *everyone*
            // full access, which is the exact opposite of this crate's job.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owner-only descriptor parsed without a DACL",
            ));
        }
        Ok(dacl.cast_const())
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful parse and is freed once, here.
        unsafe {
            LocalFree(self.0.cast());
        }
    }
}

/// Closes a Win32 handle on drop, including on the early-return paths above.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `OpenProcessToken` and is
        // closed exactly once, here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// A byte buffer aligned well enough to hold a `TOKEN_USER`.
///
/// The obvious `vec![0u8; n]` is wrong: it is only byte-aligned, while
/// `TOKEN_USER` contains a pointer and must be pointer-aligned, so reading the
/// struct back out of it is undefined behaviour. Backing the allocation with
/// `u64` gives 8-byte alignment, which covers every Windows target.
struct TokenBuffer(Vec<u64>);

impl TokenBuffer {
    fn with_capacity(bytes: usize) -> Self {
        Self(vec![0u64; bytes.div_ceil(size_of::<u64>())])
    }

    fn as_mut_ptr(&mut self) -> *mut std::ffi::c_void {
        self.0.as_mut_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_buffer_is_pointer_aligned() {
        // The reason this type exists instead of a Vec<u8>.
        let mut buf = TokenBuffer::with_capacity(37);
        assert_eq!(buf.as_mut_ptr() as usize % align_of::<TOKEN_USER>(), 0);
    }

    #[test]
    fn token_buffer_rounds_up_to_hold_the_request() {
        // A short allocation would have GetTokenInformation write past the end.
        let buf = TokenBuffer::with_capacity(37);
        assert!(buf.0.len() * size_of::<u64>() >= 37);
    }

    #[test]
    fn the_running_user_has_a_resolvable_sid() {
        let sid = current_user_sid().expect("current user must have a SID");
        assert!(sid.starts_with("S-1-"), "got {sid}");
    }

    #[test]
    fn an_alias_and_its_literal_sid_canonicalize_alike() {
        // BUILTIN\Administrators has a fixed SID, so the pair is stable on any
        // machine. This is the exact shape of the readback problem: the same
        // account, spelled two ways.
        assert_eq!(
            canonical_sid("BA").expect("alias"),
            canonical_sid("S-1-5-32-544").expect("literal"),
        );
    }

    #[test]
    fn a_garbage_sid_token_is_an_error_not_a_match() {
        assert!(canonical_sid("not-a-sid").is_err());
    }

    #[test]
    fn a_malformed_descriptor_is_rejected_rather_than_ignored() {
        // The failure mode this guards: a bad SDDL silently yielding no DACL,
        // which SetNamedSecurityInfo would read as "grant everyone everything".
        assert!(SecurityDescriptor::from_sddl("not-a-descriptor").is_err());
    }

    #[test]
    fn the_owner_only_descriptor_carries_a_dacl() {
        let sddl = super::super::owner_only_sddl().expect("sddl");
        let sd = SecurityDescriptor::from_sddl(&sddl).expect("parse");
        assert!(sd.dacl().is_ok());
    }
}
