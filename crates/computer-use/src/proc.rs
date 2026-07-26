//! Resolve a pid to the executable behind it.
//!
//! A grant is recorded against a pid, but a pid is a recycled integer: the
//! process that owned it can exit and a completely different program can be
//! handed the same number. So the grant records the *executable path* too, and
//! every later use re-resolves the pid and compares. That narrows the reuse
//! window to the gap between our check and the driver's own resolution on the
//! far side of its socket — it does not close it, and nothing on this side can
//! (see [`crate::grants`]).
//!
//! macOS: `proc_pidpath` fills a buffer with the absolute path of a pid's
//! executable. Declared directly rather than via a crate, matching how
//! `oximux-proc-cwd` handles the neighbouring `proc_pidinfo` call.

use std::path::PathBuf;

/// The absolute executable path of a running pid, or `None` when the pid is
/// dead, out of range, unreadable, or the platform is unsupported.
///
/// Callers must treat `None` as "cannot attribute this call to a program" and
/// refuse, never as "probably fine".
pub fn executable_of_pid(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos::executable_of_pid(pid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CStr;
    use std::path::PathBuf;

    /// `PROC_PIDPATHINFO_MAXSIZE` from `<sys/proc_info.h>` — 4 * MAXPATHLEN.
    /// `proc_pidpath` documents this as the required buffer size and rejects
    /// anything smaller.
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

    unsafe extern "C" {
        fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32)
        -> libc::c_int;
    }

    pub fn executable_of_pid(pid: u32) -> Option<PathBuf> {
        // pid_t is i32; a value that would wrap to negative is not a pid we
        // could ever have granted, so reject it before the syscall.
        let pid = i32::try_from(pid).ok()?;
        let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        let n = unsafe {
            proc_pidpath(
                pid,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as u32,
            )
        };
        // Returns the path length on success, 0 on failure (errno set).
        if n <= 0 {
            return None;
        }
        let path = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
        if path.is_empty() {
            return None;
        }
        Some(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-test: our own pid must resolve to a real file. Guards against the
    /// FFI declaration drifting from the kernel's.
    #[cfg(target_os = "macos")]
    #[test]
    fn resolves_our_own_executable() {
        let path = executable_of_pid(std::process::id()).expect("self pid must resolve");
        assert!(path.is_absolute(), "expected an absolute path, got {path:?}");
        assert!(path.exists(), "expected a real file, got {path:?}");
    }

    /// A dead or impossible pid must resolve to `None` rather than a stale or
    /// empty path — the whole grant check rests on this being trustworthy.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_impossible_pid_resolves_to_nothing() {
        assert!(executable_of_pid(u32::MAX).is_none());
        // Above the kernel's pid ceiling but still a valid i32, so this exercises
        // the syscall's own rejection rather than the try_from guard.
        assert!(executable_of_pid(4_000_000).is_none());
    }
}
