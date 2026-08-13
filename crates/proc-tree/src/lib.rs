//! Walk the descendants of a process, by pid.
//!
//! A terminal's PTY child is a *shell*; whatever the user typed into it —
//! `codex`, `gemini`, a build — runs as that shell's descendant. Asking the
//! kernel who those descendants are is the only way to know a program is
//! running in a terminal that does not depend on the program announcing
//! itself. Unlike a window title or a status sideband, a live pid is a fact:
//! it exists while the program runs, whether or not the program is producing
//! output, and it is gone the moment the program exits.
//!
//! Sibling of [`oximux-proc-cwd`](../../proc-cwd), and its own crate for the
//! same reason: dependency-light kernel introspection that the app and the
//! relay both reach for, sharing nothing else.
//!
//! macOS: `proc_listpids(PROC_PPID_ONLY)` for children, `proc_name` for the
//! executable name, `sysctl(KERN_PROCARGS2)` for the argument vector.
//!
//! Linux: one pass over `/proc/<pid>/stat` builds the parent→children map;
//! `/proc/<pid>/comm` and `/proc/<pid>/cmdline` supply name and arguments.
//!
//! Windows: a `CreateToolhelp32Snapshot` process walk supplies names. Reading
//! another process's command line there needs `NtQueryInformationProcess`
//! against a foreign address space, so [`cmdline_of_pid`] reports `None` and
//! callers fall back to matching the executable name.

/// One process in a walked tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    /// Executable name as the kernel records it.
    ///
    /// **Weaker evidence than [`argv_of_pid`], and not interchangeable with
    /// it.** Measured on macOS: this reports the *resolved* binary, so a
    /// program run through a symlink is named for the link's target — the
    /// shape most agent CLIs install as — and it was observed reporting a
    /// stale name for a process whose argument vector said otherwise. Prefer
    /// the argument vector wherever it can be read, and fall back to this only
    /// where it cannot (Windows, where it is the true image name).
    pub name: String,
}

/// Most processes returned from one [`descendants`] walk. A shell's tree is
/// normally 0-3 deep; this only bounds a pathological fork storm so a scan on
/// the UI thread can never run long.
const MAX_NODES: usize = 64;

/// Deepest generation [`descendants`] descends to. An agent CLI launched from
/// a shell sits at depth 1-3 (`shell → node shim → binary`); the extra room
/// covers a wrapper script or two without inviting an unbounded walk.
const MAX_DEPTH: usize = 8;

/// Every live descendant of `root`, breadth-first, excluding `root` itself.
///
/// Bounded by [`MAX_NODES`] and [`MAX_DEPTH`] — a caller polling this from the
/// render loop needs a hard ceiling more than it needs completeness. Returns
/// empty when the platform is unsupported, the pid is gone, or the kernel
/// refuses the query; callers treat that as "nothing detected", never as an
/// error worth surfacing.
pub fn descendants(root: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut frontier = vec![root];
    let mut depth = 0;
    while !frontier.is_empty() && depth < MAX_DEPTH && out.len() < MAX_NODES {
        let mut next = Vec::new();
        for pid in frontier {
            for child in children_of(pid) {
                if out.len() >= MAX_NODES {
                    break;
                }
                // A pid seen twice would mean the kernel reported a cycle;
                // impossible for a real process tree, but the guard keeps a
                // corrupt read from looping.
                if out.iter().any(|p: &ProcInfo| p.pid == child) {
                    continue;
                }
                out.push(ProcInfo {
                    pid: child,
                    name: name_of_pid(child).unwrap_or_default(),
                });
                next.push(child);
            }
        }
        frontier = next;
        depth += 1;
    }
    out
}

/// The argument vector of `pid`, or `None` when the platform cannot read it,
/// the process is gone, or the kernel denies access.
///
/// The strongest identity signal available: `argv[0]` is the path the program
/// was actually invoked through, before any symlink resolution, and a script's
/// path appears as an argument of its interpreter. Returned as separate
/// arguments rather than one joined string so a path containing spaces stays
/// unambiguous.
///
/// Separate from [`descendants`] because it is the expensive half — one
/// syscall and one buffer per process — so callers pay for it per process
/// rather than for the whole walk.
pub fn argv_of_pid(pid: u32) -> Option<Vec<String>> {
    imp::argv_of_pid(pid)
}

/// One process by pid, or `None` when it is gone or unreadable.
///
/// Companion to [`descendants`], which excludes its own root: a terminal whose
/// PTY child *is* the program of interest (a command `exec`ed in place of the
/// shell) has no descendant to find, and the caller has to look at the root
/// itself.
pub fn process(pid: u32) -> Option<ProcInfo> {
    Some(ProcInfo {
        pid,
        name: name_of_pid(pid)?,
    })
}

fn children_of(pid: u32) -> Vec<u32> {
    imp::children_of(pid)
}

fn name_of_pid(pid: u32) -> Option<String> {
    imp::name_of_pid(pid)
}

#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "windows", path = "windows.rs")]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    path = "unsupported.rs"
)]
mod imp;

#[cfg(test)]
mod tests {
    use super::*;

    /// Self-test: a child we spawn ourselves must appear in our own tree,
    /// with a name we can recognise. Guards the FFI struct layouts and the
    /// `/proc` parsing against drifting from the kernel.
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    #[test]
    fn a_spawned_child_appears_in_our_descendants() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // The child is visible to the kernel the moment `spawn` returns, but
        // a fork/exec runtime can report it before the exec lands, when its
        // name is still the parent's. Poll for the name rather than the pid.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            found = descendants(std::process::id())
                .into_iter()
                .find(|p| p.pid == child.id() && p.name.contains("sleep"));
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let killed = child.kill();
        child.wait().ok();
        killed.expect("kill sleep");
        let found = found.expect("spawned child must appear in our descendants");
        assert!(
            found.name.contains("sleep"),
            "child must be named for its executable, got {:?}",
            found.name
        );
    }

    /// A grandchild — the shape that actually matters, since an agent CLI is
    /// normally a *grandchild* of the terminal's shell (`shell → npm shim →
    /// binary`). A walk that only reported direct children would pass the
    /// test above and still miss every real agent.
    ///
    /// Two traps this has to sidestep, both of which made an earlier version
    /// pass without testing anything:
    ///
    /// * A shell handed one simple command `exec`s it **in place**, so
    ///   `sh -c 'sleep 30'` leaves no grandchild at all. The trailing `true`
    ///   is what forces the shell to stay and fork.
    /// * The tests in this module run concurrently and several spawn `sleep`,
    ///   so matching on the name alone can be satisfied by a sibling test's
    ///   process. The grandchild is identified by the unique path it was
    ///   invoked through instead.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn a_grandchild_appears_in_our_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("oximux-proc-tree-grandchild");
        std::os::unix::fs::symlink("/bin/sleep", &link).expect("symlink sleep");
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("'{}' 30; true", link.display()))
            .spawn()
            .expect("spawn sh");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut found = None;
        while std::time::Instant::now() < deadline {
            found = descendants(std::process::id()).into_iter().find(|p| {
                p.pid != child.id()
                    && argv_of_pid(p.pid)
                        .and_then(|argv| argv.into_iter().next())
                        .is_some_and(|argv0| argv0 == link.display().to_string())
            });
            if found.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        let found = found.expect("the shell's own child must appear in the walked tree");
        assert_ne!(
            found.pid,
            child.id(),
            "the match must be the grandchild, not the shell itself"
        );
    }

    /// The walk must terminate and stay empty for a pid that cannot exist,
    /// rather than erroring or returning a stale tree.
    #[test]
    fn an_impossible_pid_has_no_descendants() {
        assert!(descendants(u32::MAX).is_empty());
        assert!(argv_of_pid(u32::MAX).is_none());
        assert!(process(u32::MAX).is_none());
    }

    /// The argument vector must report the path a program was *invoked*
    /// through, not the symlink target the kernel resolved it to — the whole
    /// reason it outranks [`ProcInfo::name`]. Every agent CLI that installs as
    /// a symlink (and the common ones do) depends on this.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn argv_reports_the_invoked_path_not_the_resolved_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("some-other-name");
        std::os::unix::fs::symlink("/bin/sleep", &link).expect("symlink sleep");
        let mut child = std::process::Command::new(&link)
            .arg("30")
            .spawn()
            .expect("spawn through the symlink");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut argv = None;
        while std::time::Instant::now() < deadline {
            argv = argv_of_pid(child.id());
            if argv.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        let argv = argv.expect("a live child must report an argument vector");
        assert!(
            argv[0].ends_with("some-other-name"),
            "argv[0] must be the invoked path, got {:?}",
            argv[0]
        );
    }

    /// Our own argument vector must contain the test binary's path — proving
    /// the read targets the requested pid and parses out real arguments.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn cmdline_of_self_contains_our_own_argv() {
        let argv = argv_of_pid(std::process::id()).expect("self argv");
        assert!(!argv.is_empty(), "self argv must not be empty");
        let exe = std::env::current_exe().expect("current exe");
        let stem = exe.file_stem().expect("exe stem").to_string_lossy();
        assert!(
            argv[0].contains(stem.as_ref()),
            "argv[0] {:?} must name the running test binary {stem:?}",
            argv[0]
        );
    }
}
