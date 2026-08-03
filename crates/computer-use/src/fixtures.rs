//! Test scaffolding whose only difference across platforms is which binary
//! stands in for an idea.
//!
//! Several modules here need a *live process to point at* — the grant table
//! records pids, and a pid with nothing behind it resolves to `None` and makes
//! a test pass for the wrong reason. Others need *a program that accepts any
//! arguments and exits 0*, standing in for a driver that accepted the call.
//!
//! Neither idea is platform-specific; the binaries are. Keeping the choice
//! here means a test reads as "spawn a long-lived target" rather than as
//! `/bin/sleep`, and a future platform is one edit rather than thirty.

use std::path::PathBuf;

/// A program that runs long enough to be observed, and prints nothing.
///
/// Windows has no `sleep(1)`. Pinging loopback is the standard stand-in: it is
/// present on every install, needs no console, and `-n 120` outlasts any test.
pub fn long_lived() -> (&'static str, &'static [&'static str]) {
    #[cfg(windows)]
    {
        (r"C:\Windows\System32\PING.EXE", &["-n", "120", "127.0.0.1"])
    }
    #[cfg(not(windows))]
    {
        ("/bin/sleep", &["120"])
    }
}

/// A *second*, distinguishable long-lived program.
///
/// Tests that assert on two targets at once need the two to resolve to
/// different executables, so this must not be [`long_lived`] again.
///
/// Both stand-ins block on stdin, which the spawn helpers pipe and hold open —
/// the same trick `cat` plays on unix. `findstr` with a pattern and no file
/// argument reads stdin exactly as `cat` does.
pub fn long_lived_alt() -> (&'static str, &'static [&'static str]) {
    #[cfg(windows)]
    {
        (r"C:\Windows\System32\findstr.exe", &["x"])
    }
    #[cfg(not(windows))]
    {
        ("/bin/cat", &[])
    }
}

/// What [`long_lived`] is called on screen.
///
/// Runs the real [`crate::target::display_name`] rather than restating its
/// rule, so the two cannot drift — Windows drops the `.exe` for display, and a
/// hand-written `"PING.EXE"` here would have silently disagreed. The formatting
/// rule itself is tested in `target`, where it belongs; these callers only need
/// the name to match.
pub fn long_lived_name() -> String {
    crate::target::display_name(std::path::Path::new(long_lived().0))
}

/// What [`long_lived_alt`] is called on screen.
pub fn long_lived_alt_name() -> String {
    crate::target::display_name(std::path::Path::new(long_lived_alt().0))
}

/// A program that takes any arguments and exits **non**-zero, with no output.
///
/// Stands in for a driver that refused, and covers the no-detail fallback
/// rather than producing an empty message.
pub fn always_fails() -> PathBuf {
    #[cfg(windows)]
    {
        stub("always-fails.cmd", "@exit /b 1\r\n")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr/bin/false")
    }
}

/// A program that takes any arguments and exits 0.
///
/// Stands in for a driver that accepted whatever it was asked to do, so the
/// test can assert on the caller's handling rather than on the driver.
///
/// Unix has `/usr/bin/true`. Windows has no equivalent — every candidate in
/// `System32` either rejects unknown arguments or needs a console — so one is
/// written on demand. `.cmd` rather than a real executable because
/// `std::process::Command` routes batch files through `cmd.exe` itself, which
/// is the whole mechanism this needs.
pub fn always_succeeds() -> PathBuf {
    #[cfg(windows)]
    {
        stub("always-succeeds.cmd", "@exit /b 0\r\n")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr/bin/true")
    }
}

/// Write a one-line batch stub once per test binary and hand back its path.
///
/// The directory is held in a `static` so it outlives every test in the
/// binary. A local `TempDir` would delete the script at end of scope, and the
/// next caller would get a spawn error instead of the stub.
#[cfg(windows)]
fn stub(name: &'static str, body: &'static str) -> PathBuf {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    static WRITTEN: OnceLock<Mutex<HashMap<&'static str, PathBuf>>> = OnceLock::new();

    let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir for the stub scripts"));
    let mut written = WRITTEN
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("stub registry");

    written
        .entry(name)
        .or_insert_with(|| {
            let path = dir.path().join(name);
            // `@` suppresses echo; `exit /b` returns from the script rather
            // than killing the `cmd.exe` hosting it.
            std::fs::write(&path, body).expect("write the stub script");
            path
        })
        .clone()
}

/// An absolute path to some real, *unremarkable* executable.
///
/// For rows that only need a plausible executable recorded against a pid, and
/// for the one test that needs a binary which is neither this process nor
/// refused by [`crate::blocked`].
///
/// Unremarkable is load-bearing: it must not appear in any category or
/// blocklist table. `cmd.exe` was the obvious pick and is wrong for exactly
/// that reason — it is a `Terminal`, so it is refused outright. Notepad is a
/// plain text editor with no integrated terminal and no project, so it is
/// deliberately absent from the `Editor` table and stays ordinary.
pub fn some_executable() -> &'static str {
    #[cfg(windows)]
    {
        r"C:\Windows\System32\notepad.exe"
    }
    #[cfg(not(windows))]
    {
        "/usr/bin/true"
    }
}
