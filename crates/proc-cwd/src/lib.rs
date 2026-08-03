//! Resolve a process's current working directory from its pid.
//!
//! Shared by the app (split-pane cwd inheritance, layout snapshots) and
//! the relay daemon (live-cwd refresh on disk scrollback checkpoints),
//! which is why this lives in its own dependency-light crate instead of
//! either binary.
//!
//! macOS: `proc_pidinfo` with `PROC_PIDVNODEPATHINFO` returns a struct
//! whose `pvi_cdir.vip_path` field is the absolute CWD. This is the
//! same syscall `lsof -p <pid>` uses for its `cwd` line.
//!
//! Linux: readlink on `/proc/<pid>/cwd` — the kernel keeps the symlink
//! pointed at the process's live working directory.
//!
//! Elsewhere (Windows): returns `None` for now. Windows has its own API;
//! wire it when something actually needs it there.

use std::path::PathBuf;

/// Best-effort lookup of the working directory of the running process
/// with the given OS pid. Returns `None` when:
///   - the platform isn't supported,
///   - the pid is no longer alive,
///   - the kernel call fails (permission, exited, race).
pub fn cwd_of_pid(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos::cwd_of_pid(pid)
    }
    #[cfg(target_os = "linux")]
    {
        linux::cwd_of_pid(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;

    pub fn cwd_of_pid(pid: u32) -> Option<PathBuf> {
        // Fails with ENOENT for a dead or zombie pid and EACCES for
        // another user's process — all of which mean "no answer" here.
        let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        // A directory removed while still the process's cwd reads back
        // as "<path> (deleted)" — a name that never existed. Callers
        // prefer no answer over a wrong one (they fall back to OSC 7).
        if path.to_string_lossy().ends_with(" (deleted)") {
            return None;
        }
        Some(path)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CStr;
    use std::path::PathBuf;

    // macOS proc_info constants — see <sys/proc_info.h>. Hard-coded
    // here so we don't need to pull in a fresh dependency just for one
    // syscall. PROC_PIDVNODEPATHINFO = 9.
    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
    const MAXPATHLEN: usize = 1024;

    // `vnode_info_path` and `proc_vnodepathinfo` from <sys/proc_info.h>.
    // We only need the first vnode (pvi_cdir = current working dir);
    // pvi_rdir follows in the kernel struct. Lay out both so the
    // returned byte count from `proc_pidinfo` matches the kernel's
    // expectation (it rejects undersized buffers with EINVAL/0).
    #[repr(C)]
    struct VInfoStat {
        _vst_dev: u32,
        _vst_mode: u16,
        _vst_nlink: u16,
        _vst_ino: u64,
        _vst_uid: u32,
        _vst_gid: u32,
        _vst_atime: i64,
        _vst_atimensec: i64,
        _vst_mtime: i64,
        _vst_mtimensec: i64,
        _vst_ctime: i64,
        _vst_ctimensec: i64,
        _vst_birthtime: i64,
        _vst_birthtimensec: i64,
        _vst_size: i64,
        _vst_blocks: i64,
        _vst_blksize: i32,
        _vst_flags: u32,
        _vst_gen: u32,
        _vst_rdev: u32,
        _vst_qspare: [i64; 2],
    }

    #[repr(C)]
    struct VnodeInfo {
        _vi_stat: VInfoStat,
        _vi_type: libc::c_int,
        _vi_pad: libc::c_int,
        _vi_fsid: [u8; 8],
    }

    #[repr(C)]
    struct VnodeInfoPath {
        vip_vi: VnodeInfo,
        vip_path: [libc::c_char; MAXPATHLEN],
    }

    #[repr(C)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        _pvi_rdir: VnodeInfoPath,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut libc::c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    pub fn cwd_of_pid(pid: u32) -> Option<PathBuf> {
        let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            proc_pidinfo(
                pid as libc::c_int,
                PROC_PIDVNODEPATHINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int,
            )
        };
        // proc_pidinfo returns the number of bytes filled, or 0 on
        // failure (errno set). Anything smaller than our struct means
        // the kernel rejected it — treat as a no-CWD answer.
        if n < std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int {
            return None;
        }
        let path_cstr = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr()) };
        let path = path_cstr.to_string_lossy().into_owned();
        if path.is_empty() {
            return None;
        }
        Some(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::cwd_of_pid;

    /// Self-test: our own process pid must report a real CWD. Guards
    /// against the FFI struct layout drifting from the kernel header on
    /// macOS, and against the /proc symlink shape changing on Linux.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cwd_of_self_returns_existing_path() {
        let pid = std::process::id();
        let cwd = cwd_of_pid(pid).expect("self-pid must return a CWD");
        assert!(cwd.is_absolute(), "CWD must be absolute, got {cwd:?}");
        assert!(cwd.exists(), "CWD must point to a real path, got {cwd:?}");
    }

    /// The documented behavior off macOS/Linux: no panic, no guess, just
    /// `None`. Callers treat that as "ask the shell instead" (OSC 7), so
    /// returning a wrong answer would be worse than returning nothing.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    #[test]
    fn an_unsupported_platform_reports_no_cwd() {
        assert!(cwd_of_pid(std::process::id()).is_none());
        assert!(cwd_of_pid(u32::MAX).is_none());
    }

    /// Non-existent pid returns None rather than panicking or returning
    /// a stale path from a previous query.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cwd_of_unlikely_pid_returns_none() {
        // u32::MAX is well outside any realistic pid range; macOS pid_t
        // is i32 so values above 2^31 wrap to negative and the syscall
        // rejects them, and no /proc entry can exist for it on Linux.
        assert!(cwd_of_pid(u32::MAX).is_none());
    }

    /// A child spawned with an explicit working directory reports that
    /// directory back — proving the lookup reads the *target* pid's
    /// state, which the self-pid test alone cannot show.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cwd_of_child_reports_spawn_directory() {
        // Canonicalize before comparing: the tempdir may live behind a
        // symlink (/var -> /private/var on macOS) and the kernel reports
        // the resolved path.
        let dir = tempfile::tempdir().expect("tempdir");
        let expected = dir.path().canonicalize().expect("canonicalize tempdir");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .expect("spawn sleep in tempdir");
        // Poll briefly rather than asserting the first read: if the
        // runtime fell back to fork/exec, the parent can observe the
        // child before its chdir has landed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = cwd_of_pid(child.id());
        while got.as_deref() != Some(expected.as_path()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            got = cwd_of_pid(child.id());
        }
        child.kill().ok();
        child.wait().ok();
        assert_eq!(got.as_deref(), Some(expected.as_path()));
    }
}
