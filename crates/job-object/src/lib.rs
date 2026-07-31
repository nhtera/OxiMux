//! Killing a child *and everything it started*, on Windows.
//!
//! On unix every process the app spawns for an agent or a terminal is put in its
//! own process group, so one `kill(-pgid, …)` reaches the child, the shell it
//! ran, and whatever that shell ran. Windows has no process groups in that
//! sense: `TerminateProcess` ends exactly one process, and the tree it started
//! keeps running with no parent to account for it. An agent CLI's `bash` tool
//! children, a terminal's compiler, a dev server holding a port — all of them
//! outlive the thing the user actually stopped.
//!
//! A Job Object is the mechanism that does have tree semantics. A child assigned
//! to one is joined by everything it spawns afterwards, and terminating the job
//! terminates the whole set. Two properties make it worth the ceremony:
//!
//! - `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means the tree dies when the last
//!   handle to the job closes — including the implicit close when *this* process
//!   dies. So an app crash reaps its agents rather than stranding them, which is
//!   a guarantee the unix side does not actually have.
//! - Assignment is inherited, so a grandchild is covered without being tracked.
//!
//! The gap this does not close: a process spawned by the child in the window
//! between `CreateProcess` returning and `AssignProcessToJobObject` running is
//! not in the job. Closing it needs `CREATE_SUSPENDED` and a resume, which
//! `std::process::Command` gives no way to express. The race is small — the
//! child has to fork before its first instruction is scheduled — and the
//! consequence is one escaped process rather than a broken kill, so this starts
//! here and escalates to a raw `CreateProcessW` spawn seam only if an orphan is
//! ever actually observed.

#![cfg(windows)]

use std::io;
use std::process::Child;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// Exit code reported for processes killed as part of a tree. Arbitrary but
/// distinct, so a survivor's exit status says how it died.
const TREE_KILL_EXIT_CODE: u32 = 1;

/// A Job Object owning one child and its descendants.
///
/// Hold it for as long as the child should live. Dropping it closes the job
/// handle, which — because of the kill-on-close limit — terminates whatever is
/// still running inside. That is deliberate: it makes "the owner went away"
/// and "the tree should die" the same event.
pub struct JobObject(HANDLE);

// SAFETY: a job handle is just a kernel handle; every use below is a Win32 call
// that takes it by value, and the only mutation is through those calls, which
// are themselves thread-safe.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}

impl JobObject {
    /// Put `child` — and everything it goes on to spawn — into a fresh job.
    ///
    /// Returns an error rather than a silently empty job if any step fails, so a
    /// caller that wanted tree-kill semantics finds out at spawn time instead of
    /// at kill time, when the orphans are already running.
    pub fn adopt(child: &Child) -> io::Result<Self> {
        let job = Self::empty()?;
        // SAFETY: the handle comes from a live `Child` we own, so it is valid for
        // the duration of this call.
        let ok = unsafe { AssignProcessToJobObject(job.0, child_handle(child)) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Same, for a child we only know by pid.
    ///
    /// The relay's PTY children come back from `portable-pty` as trait objects
    /// that expose a process id and nothing else, so there is no handle to
    /// borrow. Only call this while the process is known to be alive and unreaped
    /// — a pid whose process has exited is free for Windows to reissue, and this
    /// would then adopt a stranger into the job and later kill it.
    pub fn adopt_pid(pid: u32) -> io::Result<Self> {
        // SAFETY: a plain Win32 call; the returned handle is checked and closed
        // by `OwnedHandle` below.
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle(handle);

        let job = Self::empty()?;
        // SAFETY: `process.0` is a live handle from the call just above.
        let ok = unsafe { AssignProcessToJobObject(job.0, process.0) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// A job with the kill-on-close limit set and nothing in it yet.
    fn empty() -> io::Result<Self> {
        // SAFETY: a null name creates an anonymous job; null attributes take the
        // default security descriptor, which is fine for a handle never shared
        // outside this process.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(raw);

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a correctly-typed, fully-initialised value of the
        // size being passed, and the class matches the struct.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Terminate every process still in the job.
    ///
    /// There is no graceful counterpart. Windows has nothing to deliver that
    /// would ask a tree to wind itself down — no SIGTERM, and console control
    /// events only reach processes sharing a console, which these do not have.
    /// Callers wanting a graceful tier have to get it from the protocol they
    /// speak to the child, not from here.
    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle owned by this value.
        let ok = unsafe { TerminateJobObject(self.0, TREE_KILL_EXIT_CODE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // Closing the last handle is what triggers kill-on-close, so this is the
        // reaping path as much as it is cleanup.
        // SAFETY: `self.0` came from a successful `CreateJobObjectW` and is
        // closed exactly once, here.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// The raw process handle behind a `Child`.
fn child_handle(child: &Child) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    child.as_raw_handle() as HANDLE
}

/// Closes a Win32 handle on drop, including on the early-return paths above.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from a successful `OpenProcess` and is closed
        // exactly once, here. Assignment to a job does not consume the handle —
        // the job holds its own reference to the process.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// ~30s of doing nothing, safe with stdin redirected.
    ///
    /// NOT `timeout /t`: that one refuses to run when stdin is not a console
    /// ("Input redirection is not supported") and exits immediately with a
    /// non-zero code — which is exactly what a killed child looks like, so
    /// every assertion here would have passed without testing anything.
    const SLEEP_30: &str = "ping -n 31 127.0.0.1 > NUL";

    fn spawn_sleeper() -> Child {
        Command::new("cmd")
            .args(["/c", SLEEP_30])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    /// A private directory for one test's script + marker.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oximux-job-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// Write the grandchild script into `dir`. It waits, then drops a marker
    /// beside itself — so if it is still alive after the kill, it says so.
    ///
    /// `%~dp0` is the script's own directory, which means no path is ever
    /// passed on a command line: `cmd.exe` parses quotes by its own rules,
    /// not the CRT's, and a temp path with a space would be a coin flip.
    fn write_grandchild_script(dir: &Path) -> PathBuf {
        let script = dir.join("grandchild.cmd");
        std::fs::write(
            &script,
            "@echo off\r\nping -n 4 127.0.0.1 > NUL\r\necho alive > \"%~dp0marker.txt\"\r\n",
        )
        .expect("write script");
        script
    }

    /// `start /b` detaches the grandchild — the shape that survives a plain
    /// TerminateProcess on the direct child, and the whole reason a job object
    /// is here rather than a pid list. Run from `dir`, so the script is named
    /// relatively and nothing needs quoting.
    fn spawn_detached_tree(dir: &Path) -> Child {
        Command::new("cmd")
            .args(["/c", &format!("start /b grandchild.cmd & {SLEEP_30}")])
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cmd")
    }

    fn marker_appears_within(dir: &Path, wait: Duration) -> bool {
        let marker = dir.join("marker.txt");
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if marker.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn killing_the_job_ends_the_child() {
        let mut child = spawn_sleeper();
        let job = JobObject::adopt(&child).expect("adopt");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "child should still be running before the kill"
        );

        job.kill().expect("kill");

        let status = child.wait().expect("wait");
        assert!(!status.success(), "a killed child must not report success");
    }

    #[test]
    fn dropping_the_job_ends_the_child() {
        // The property an app crash relies on: nobody calls `kill`, the handle
        // just goes away. Without KILL_ON_JOB_CLOSE this test hangs on `wait`.
        let mut child = spawn_sleeper();
        let job = JobObject::adopt(&child).expect("adopt");
        drop(job);

        let status = child.wait().expect("wait");
        assert!(!status.success(), "closing the job must reap the tree");
    }

    // The control. Without it, "no marker" could mean the job worked OR that
    // the grandchild never ran at all — and a test that cannot tell those
    // apart is the one that lets a broken tree-kill ship.
    #[test]
    fn an_unjobbed_grandchild_outlives_its_parent() {
        let dir = scratch("control");
        write_grandchild_script(&dir);
        let mut child = spawn_detached_tree(&dir);

        // End the direct child only. A detached grandchild is untouched by
        // this, which is the problem job objects exist to solve.
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            marker_appears_within(&dir, Duration::from_secs(15)),
            "grandchild should have survived its parent and written the marker"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_grandchild_dies_with_the_job() {
        let dir = scratch("jobbed");
        write_grandchild_script(&dir);
        let mut child = spawn_detached_tree(&dir);
        let job = JobObject::adopt(&child).expect("adopt");

        job.kill().expect("kill");
        let status = child.wait().expect("wait");
        assert!(!status.success(), "direct child must die too");

        // The actual guarantee: the grandchild never gets to write. Waited out
        // well past its own delay, so "not yet" cannot be mistaken for "never".
        assert!(
            !marker_appears_within(&dir, Duration::from_secs(10)),
            "grandchild survived the job kill"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
