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
//!
//! Windows: `QueryFullProcessImageNameW` against a handle opened with
//! `PROCESS_QUERY_LIMITED_INFORMATION` — the least-privileged right that still
//! answers this question, and the one an unelevated process can obtain against
//! same-integrity targets.
//!
//! A higher-integrity target (anything elevated) refuses the open and resolves
//! to `None`. That is the **correct** answer rather than a limitation to work
//! around: OxiMux could not drive such a process anyway, because UIPI blocks
//! input injection across the integrity boundary. A `None` here and a refusal
//! there are the same fact reaching the user twice, which is why the contract
//! above says `None` means refuse.

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
    #[cfg(windows)]
    {
        windows::executable_of_pid(pid)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// Room for an extended-length path. `QueryFullProcessImageNameW` reports
    /// the length it used, so oversizing costs one stack-free allocation and
    /// removes the retry loop a smaller buffer would need.
    const MAX_PATH_WIDE: usize = 32_768;

    pub fn executable_of_pid(pid: u32) -> Option<PathBuf> {
        // SAFETY: `OpenProcess` is a pure lookup. It returns null on failure —
        // including the pid being dead, or being a higher-integrity process we
        // are not allowed to ask about — which is checked before use.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }

        let mut buf = vec![0u16; MAX_PATH_WIDE];
        let mut len = buf.len() as u32;
        // SAFETY: `handle` is live and owned here; `buf` is `len` wide chars
        // and outlives the call. `len` is in/out — on success it is overwritten
        // with the count actually written, excluding the terminator.
        let ok = unsafe {
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        // SAFETY: closing the handle this function opened, exactly once,
        // before any early return below can skip it.
        unsafe { CloseHandle(handle) };

        if ok == 0 {
            return None;
        }
        // Trust the returned length rather than scanning for a NUL: the buffer
        // was not zeroed by the call, so a scan could run past the real end.
        Some(PathBuf::from(OsString::from_wide(&buf[..len as usize])))
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
    ///
    /// Runs on both real implementations. A process can always open itself, so
    /// this reaches `QueryFullProcessImageNameW` without depending on what
    /// integrity level the test runner happens to have.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn resolves_our_own_executable() {
        let path = executable_of_pid(std::process::id()).expect("self pid must resolve");
        assert!(path.is_absolute(), "expected an absolute path, got {path:?}");
        assert!(path.exists(), "expected a real file, got {path:?}");
    }

    /// A dead or impossible pid must resolve to `None` rather than a stale or
    /// empty path — the whole grant check rests on this being trustworthy.
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn an_impossible_pid_resolves_to_nothing() {
        assert!(executable_of_pid(u32::MAX).is_none());
        // Above the kernel's pid ceiling but still a valid i32, so this exercises
        // the syscall's own rejection rather than the try_from guard.
        //
        // macOS only: Windows pids are recycled multiples of four with no such
        // ceiling, so a seven-digit pid is merely unlikely rather than
        // impossible — and a test that fails when the machine is busy is worse
        // than one that does not run.
        #[cfg(target_os = "macos")]
        assert!(executable_of_pid(4_000_000).is_none());
    }

    /// The path must come back in a form the grant table can compare against.
    ///
    /// `grants.rs` records the executable at grant time and re-resolves on every
    /// later use, so a path that round-trips differently — a `\\?\` prefix here
    /// and none there — would read as "a different program has this pid" and
    /// silently revoke a live grant.
    #[cfg(windows)]
    #[test]
    fn the_resolved_path_matches_what_the_std_library_reports() {
        let resolved = executable_of_pid(std::process::id()).expect("self pid must resolve");
        let expected = std::env::current_exe().expect("current_exe");
        assert_eq!(
            resolved, expected,
            "pid resolution and current_exe must agree, or grant re-checks will \
             see a program change that did not happen"
        );
    }
}
