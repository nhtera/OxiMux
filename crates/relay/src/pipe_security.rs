//! Owner-only access control for the Windows named pipe.
//!
//! A unix-domain socket inherits the protection of the directory holding it, so
//! the relay's socket is unreachable to other accounts without anything being
//! spelled out. `\\.\pipe\` has no such containment: it is one flat namespace
//! shared by every session on the machine, and a pipe created with the default
//! security descriptor is reachable by principals that have no business
//! carrying this daemon's traffic — which is every keystroke typed into every
//! terminal and every byte those terminals print back.
//!
//! So the descriptor is stated explicitly, and every path through this module
//! either produces one or fails. There is deliberately no fallback that creates
//! the pipe without it: a relay that refuses to start is a visible problem,
//! while a relay listening on an open pipe is an invisible one.

use anyhow::{Context, Result, bail};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::{U16CStr, U16CString};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Build a security descriptor granting the current user full control and
/// nobody else any access at all.
///
/// The SDDL reads: `D:` a DACL follows; `P` protected, so ACEs inherited from
/// the namespace container cannot widen it; `(A;;GA;;;<sid>)` one allow-ACE
/// granting GENERIC_ALL to this user's SID.
///
/// Only the user's own SID is listed. Administrators and SYSTEM are left out on
/// purpose — nothing in the product connects as either, and an account that can
/// already take ownership of the object gains nothing from being named here.
pub fn owner_only_descriptor() -> Result<SecurityDescriptor> {
    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;GA;;;{sid})");
    let wide = U16CString::from_str(&sddl).context("security descriptor string is not valid UTF-16")?;
    // Rejects a malformed descriptor here rather than at pipe creation, which
    // is why a mistake in the string above surfaces as a startup failure.
    SecurityDescriptor::deserialize(&wide)
        .with_context(|| format!("parse security descriptor {sddl}"))
}

/// Look up the SID of the account this process runs as, in string form
/// (`S-1-5-21-...`).
fn current_user_sid() -> Result<String> {
    // SAFETY: each call below is checked before its output is used, the token
    // handle is closed by `OwnedHandle` on every exit path, and the SID string
    // is freed with `LocalFree` as `ConvertSidToStringSidW` requires. The token
    // buffer's alignment is handled by `TokenBuffer` — see its comment.
    unsafe {
        let mut raw_token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) == 0 {
            bail!("OpenProcessToken failed: {}", std::io::Error::last_os_error());
        }
        let token = OwnedHandle(raw_token);

        // First call is expected to fail with ERROR_INSUFFICIENT_BUFFER; its
        // purpose is to report the size through `needed`.
        let mut needed: u32 = 0;
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            bail!(
                "GetTokenInformation would not report a size: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut buf = TokenBuffer::with_capacity(needed as usize);
        if GetTokenInformation(token.0, TokenUser, buf.as_mut_ptr(), needed, &mut needed) == 0 {
            bail!(
                "GetTokenInformation failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // The buffer holds a TOKEN_USER whose `Sid` points elsewhere within it.
        let token_user = &*(buf.as_mut_ptr() as *const TOKEN_USER);
        let mut sid_str = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str) == 0 {
            bail!(
                "ConvertSidToStringSidW failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let sid = U16CStr::from_ptr_str(sid_str).to_string_lossy();
        LocalFree(sid_str.cast());
        Ok(sid)
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

/// A byte buffer that is aligned well enough to hold a `TOKEN_USER`.
///
/// The obvious `vec![0u8; n]` is wrong: it is only byte-aligned, while
/// `TOKEN_USER` contains a pointer and must be pointer-aligned, so reading the
/// struct back out of it is undefined behaviour. Backing the allocation with
/// `u64` gives 8-byte alignment, which covers the pointer alignment of every
/// Windows target.
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
    fn the_descriptor_builds_for_the_running_user() {
        // Exercises the SDDL end to end: if the format string above is ever
        // malformed, this fails rather than a pipe silently opening wide.
        assert!(owner_only_descriptor().is_ok());
    }
}
