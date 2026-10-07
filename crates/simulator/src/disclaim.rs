//! Spawning a child that is responsible for itself, as macOS's privacy
//! system (TCC) sees it.
//!
//! Normally a process OxiMux starts uses OxiMux's privacy grants: macOS asks
//! about, and remembers, the *responsible* app. The capture helper must hold
//! the camera grant itself — or the grant would be OxiMux's, and every
//! terminal and agent under OxiMux could open the camera. So it is spawned
//! with responsibility disclaimed (`responsibility_spawnattrs_setdisclaim`,
//! what the system uses to launch independent helpers). Measured in the
//! spike: the disclaimed child reads its own bundle's grant, and an ordinary
//! child of the same host still reads `notDetermined`.
//!
//! The call is private, so it is looked up at run time; without it the child
//! is spawned plainly and [`DisclaimedChild::disclaimed`] says so.
//!
//! stdin and stdout are pipes, stderr is the given log (or `/dev/null`), the
//! environment is exactly `env`, and nothing else of ours is inherited
//! (`POSIX_SPAWN_CLOEXEC_DEFAULT`). Ignored signals are reset, as `Command`
//! does: we ignore `SIGPIPE`, and a child must not inherit that.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;

type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int;

/// A child spawned by [`spawn`]: what `std::process::Child` offers that the
/// helper session uses. It is reaped once, by whichever wait sees it exit.
#[derive(Debug)]
pub struct DisclaimedChild {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
    disclaimed: bool,
}

impl DisclaimedChild {
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    /// Whether macOS took the child as responsible for itself.
    pub fn disclaimed(&self) -> bool {
        self.disclaimed
    }

    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: `pid` is our unreaped child, so it names no other process.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_with(libc::WNOHANG)
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.wait_with(0)? {
                return Ok(status);
            }
        }
    }

    fn wait_with(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut raw = 0;
        // SAFETY: plain waitpid on our own child.
        match unsafe { libc::waitpid(self.pid, &mut raw, flags) } {
            0 => Ok(None),
            -1 => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted { Ok(None) } else { Err(e) }
            }
            _ => {
                let status = ExitStatus::from_raw(raw);
                self.status = Some(status);
                Ok(Some(status))
            }
        }
    }
}

/// The spawned child and our ends of its stdin and stdout.
pub struct Disclaimed {
    pub child: DisclaimedChild,
    pub stdin: File,
    pub stdout: File,
}

/// Spawn `path args…` with exactly `env`, responsible for itself.
pub fn spawn(path: &Path, args: &[String], env: &[(String, std::ffi::OsString)], stderr: Option<&File>) -> io::Result<Disclaimed> {
    let program = cstring(path.as_os_str())?;
    let mut argv_owned = vec![program.clone()];
    for arg in args {
        argv_owned.push(cstring(OsStr::new(arg))?);
    }
    let env_owned = env
        .iter()
        .map(|(k, v)| {
            let mut pair = k.as_bytes().to_vec();
            pair.push(b'=');
            pair.extend_from_slice(v.as_bytes());
            CString::new(pair).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in the environment"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let argv = nul_terminated(&argv_owned);
    let envp = nul_terminated(&env_owned);

    let (child_stdin, our_stdin) = pipe()?;
    let (our_stdout, child_stdout) = pipe()?;
    let null;
    let err_fd = match stderr {
        Some(file) => file.as_raw_fd(),
        None => {
            null = File::options().write(true).open("/dev/null")?;
            null.as_raw_fd()
        }
    };

    let mut actions = Actions::new()?;
    actions.dup2(child_stdin.as_raw_fd(), 0)?;
    actions.dup2(child_stdout.as_raw_fd(), 1)?;
    actions.dup2(err_fd, 2)?;
    let mut attr = Attr::new()?;
    attr.prepare()?;
    let disclaimed = attr.disclaim();
    if !disclaimed {
        tracing::warn!("this macOS cannot spawn a self-responsible child; the capture helper uses OxiMux's camera grant");
    }

    let mut pid = 0;
    // SAFETY: every pointer is valid for the call: `argv`/`envp` are
    // NUL-terminated arrays of NUL-terminated strings kept alive above, and
    // `actions`/`attr` are initialized.
    let rc = unsafe { libc::posix_spawn(&mut pid, program.as_ptr(), &actions.0, &attr.0, argv.as_ptr(), envp.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    // The child's ends close here; ours stay.
    drop((child_stdin, child_stdout));
    Ok(Disclaimed {
        child: DisclaimedChild { pid, status: None, disclaimed },
        stdin: File::from(our_stdin),
        stdout: File::from(our_stdout),
    })
}

fn cstring(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in an argument"))
}

fn nul_terminated(strings: &[CString]) -> Vec<*mut libc::c_char> {
    strings.iter().map(|s| s.as_ptr().cast_mut()).chain(std::iter::once(std::ptr::null_mut())).collect()
}

/// `(read end, write end)`, both close-on-exec: no other child of ours may
/// hold the helper's stdin open (it exits when stdin closes).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe(2) just returned these, owned by nobody else.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [&read, &write] {
        // SAFETY: fcntl on a descriptor we own.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((read, write))
}

struct Actions(libc::posix_spawn_file_actions_t);

impl Actions {
    fn new() -> io::Result<Self> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: initializes `raw`, destroyed in `drop`.
        check(unsafe { libc::posix_spawn_file_actions_init(&mut raw) })?;
        Ok(Self(raw))
    }

    fn dup2(&mut self, from: libc::c_int, to: libc::c_int) -> io::Result<()> {
        // SAFETY: `self.0` is initialized.
        check(unsafe { libc::posix_spawn_file_actions_adddup2(&mut self.0, from, to) })
    }
}

impl Drop for Actions {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`.
        unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
    }
}

struct Attr(libc::posix_spawnattr_t);

impl Attr {
    fn new() -> io::Result<Self> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: initializes `raw`, destroyed in `drop`.
        check(unsafe { libc::posix_spawnattr_init(&mut raw) })?;
        Ok(Self(raw))
    }

    /// Inherit no descriptor but 0–2, reset every signal to its default and
    /// block none.
    fn prepare(&mut self) -> io::Result<()> {
        // SAFETY: `self.0` is initialized; the sets are filled before use.
        unsafe {
            let mut all: libc::sigset_t = std::mem::zeroed();
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            libc::sigemptyset(&mut none);
            check(libc::posix_spawnattr_setsigdefault(&mut self.0, &all))?;
            check(libc::posix_spawnattr_setsigmask(&mut self.0, &none))?;
            let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK;
            check(libc::posix_spawnattr_setflags(&mut self.0, flags as libc::c_short))
        }
    }

    /// Ask for the child to be responsible for itself; false when this macOS
    /// does not offer it.
    fn disclaim(&mut self) -> bool {
        // SAFETY: dlsym with a NUL-terminated name; the symbol, when present,
        // has the signature `SetDisclaim` (libquarantine's private API).
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"responsibility_spawnattrs_setdisclaim".as_ptr());
            if symbol.is_null() {
                return false;
            }
            let set: SetDisclaim = std::mem::transmute(symbol);
            set(&mut self.0, 1) == 0
        }
    }
}

impl Drop for Attr {
    fn drop(&mut self) {
        // SAFETY: initialized in `new`.
        unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(rc)) }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    #[test]
    fn a_disclaimed_child_round_trips_stdin_and_sees_only_its_env() {
        let env = vec![("OXIMUX_PROBE".to_owned(), std::ffi::OsString::from("yes"))];
        let script = "cat; printf \"|%s|%s\" \"$OXIMUX_PROBE\" \"${HOME:-unset}\"";
        let mut spawned = spawn(Path::new("/bin/sh"), &["-c".into(), script.into()], &env, None).unwrap();
        assert!(spawned.child.disclaimed(), "this macOS offers responsibility_spawnattrs_setdisclaim");
        spawned.stdin.write_all(b"hello").unwrap();
        drop(spawned.stdin); // EOF: `cat` ends
        let mut out = String::new();
        spawned.stdout.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello|yes|unset");
        assert!(spawned.child.wait().unwrap().success());
        // Reaped once; asking again is answered from memory.
        assert!(spawned.child.try_wait().unwrap().unwrap().success());
    }

    #[test]
    fn a_disclaimed_child_can_be_killed() {
        let mut spawned = spawn(Path::new("/bin/sleep"), &["30".into()], &[], None).unwrap();
        assert_eq!(spawned.child.try_wait().unwrap(), None);
        spawned.child.kill().unwrap();
        assert_eq!(spawned.child.wait().unwrap().signal(), Some(libc::SIGKILL));
        spawned.child.kill().unwrap(); // already reaped: a no-op
    }

    #[test]
    fn a_missing_program_is_an_error() {
        assert!(spawn(Path::new("/nonexistent/helper"), &[], &[], None).is_err());
    }

    #[test]
    fn nul_in_an_argument_is_rejected() {
        let args = vec!["first\0second".to_owned()];
        let result = spawn(Path::new("/bin/sh"), &args, &[], None);
        assert!(result.is_err());
    }

    #[test]
    fn nul_in_an_environment_variable_is_rejected() {
        let env = vec![("KEY".to_owned(), std::ffi::OsString::from("val\0ue"))];
        let result = spawn(Path::new("/bin/sh"), &[], &env, None);
        assert!(result.is_err());
    }

    #[test]
    fn nul_in_environment_key_is_rejected() {
        let env = vec![("KE\0Y".to_owned(), std::ffi::OsString::from("value"))];
        let result = spawn(Path::new("/bin/sh"), &[], &env, None);
        assert!(result.is_err());
    }

    #[test]
    fn try_wait_on_exited_child_returns_status() {
        use std::time::Duration;
        let mut spawned = spawn(Path::new("/bin/sh"), &["-c".into(), "exit 0".into()], &[], None).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let first = spawned.child.try_wait().unwrap();
        assert!(first.is_some());
        // Asking again should return the cached status
        let second = spawned.child.try_wait().unwrap();
        assert!(second.is_some());
    }
}
