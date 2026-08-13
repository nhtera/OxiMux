//! Presence of an agent CLI in a *plain* terminal, read from its process tree.
//!
//! The other two ambient signals are things an agent says. The OSC-9999
//! sideband exists only where OxiMux installed hooks, which is Claude Code
//! alone; the window title has to be written by the CLI, and several — Codex
//! among them — leave it alone unless the user opts in. Both are also
//! *events*, so they say nothing about an agent sitting idle at its prompt.
//!
//! A process is not a statement. The shell driving a terminal has the agent as
//! a descendant for exactly as long as the agent runs, so walking that tree
//! answers "is an agent here, and which one" for every CLI, working or idle,
//! whether or not it ever announces itself. That makes this the presence
//! signal and leaves the other two to do what they are actually good at:
//! saying what the agent is *doing*.
//!
//! Scanning is throttled to [`SCAN_INTERVAL`] and driven from the terminal's
//! poll tick ahead of its no-output early return, so an idle agent keeps its
//! row and an agent that exits without printing still loses one.

use std::time::{Duration, Instant};

/// How often a terminal re-walks its process tree. An agent appearing or
/// exiting shows up within this window; a walk costs a handful of syscalls, so
/// the interval is set by how fast the rail should react, not by cost.
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Per-terminal agent-presence scan. One per `TerminalView`; idle until the
/// terminal has a resolvable shell pid.
#[derive(Default)]
pub struct AgentProcessScan {
    /// Shell pid this terminal's tree is rooted at. Cached because resolving
    /// it can read the relay's checkpoint file, which is not worth doing on a
    /// cadence. Dropped when the process behind it dies, so a respawned or
    /// re-attached shell is picked up on the next scan.
    root: Option<u32>,
    last_scan: Option<Instant>,
    label: Option<&'static str>,
}

impl AgentProcessScan {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-walk the tree if a scan is due, otherwise do nothing.
    ///
    /// `resolve_root` supplies the terminal's shell pid and is called only
    /// when there is no live cached one, so the relay path's checkpoint read
    /// stays off the cadence.
    pub fn poll(&mut self, now: Instant, resolve_root: impl FnOnce() -> Option<u32>) {
        if !self.due(now) {
            return;
        }
        self.last_scan = Some(now);
        // Only a live pid is worth keeping: a cached root whose process is
        // gone is a shell that exited, and a freshly resolved one that is
        // already dead is a stale answer. Caching either would mean walking a
        // pid the kernel may since have reused, and would stop the resolver
        // from being asked again.
        let root = self
            .root
            .or_else(resolve_root)
            .filter(|&pid| oximux_proc_tree::process(pid).is_some());
        self.root = root;
        self.label = root.and_then(detect);
    }

    /// The agent CLI running in this terminal, as a display label shared with
    /// the title heuristic, or `None` when the terminal is running something
    /// else. Cheap — reads the last scan's result.
    pub fn current(&self) -> Option<&'static str> {
        self.label
    }

    /// Forget the scan entirely: no root, no reading, and the next poll scans
    /// immediately. For a terminal whose session was replaced under it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn due(&self, now: Instant) -> bool {
        self.last_scan
            .is_none_or(|last| now.saturating_duration_since(last) >= SCAN_INTERVAL)
    }
}

/// Name the shallowest agent CLI in `root`'s tree.
///
/// Shallowest wins because that is the program the user ran: an agent that
/// spawns a second agent as a tool should still present as itself. `root` is
/// checked too, for a terminal whose command replaced the shell outright.
/// Every walked process is read, with no cap of its own: the walk is already
/// bounded, and a cap here would be worse than none. Skipping a read hands the
/// matcher an absent argument vector, which it cannot distinguish from a
/// platform that has none — so it would fall back to the executable name, the
/// signal that has been seen naming an agent for a process that was not one.
/// Not looking must not be able to masquerade as nothing to look at.
fn detect(root: u32) -> Option<&'static str> {
    // `descendants` walks breadth-first, so the first match is the shallowest.
    oximux_proc_tree::process(root)
        .into_iter()
        .chain(oximux_proc_tree::descendants(root))
        .find_map(|proc| {
            let argv = oximux_proc_tree::argv_of_pid(proc.pid);
            oximux_agents::agent_label_for_process(&proc.name, argv.as_deref())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scan_is_due_immediately_then_throttled() {
        let mut scan = AgentProcessScan::new();
        let t0 = Instant::now();
        assert!(scan.due(t0), "a fresh scan must not wait for its first walk");
        scan.poll(t0, || None);
        assert!(!scan.due(t0), "a scan just taken must not repeat");
        assert!(!scan.due(t0 + SCAN_INTERVAL - Duration::from_millis(1)));
        assert!(scan.due(t0 + SCAN_INTERVAL));
    }

    #[test]
    fn the_root_is_resolved_once_and_then_reused() {
        let mut scan = AgentProcessScan::new();
        let t0 = Instant::now();
        // Our own pid is alive, so the second poll must reuse it rather than
        // paying the resolver again (which can read the relay checkpoint).
        scan.poll(t0, || Some(std::process::id()));
        scan.poll(t0 + SCAN_INTERVAL, || {
            panic!("a live cached root must not be re-resolved")
        });
        assert_eq!(scan.root, Some(std::process::id()));
    }

    #[test]
    fn a_dead_root_is_dropped_so_a_respawned_shell_is_picked_up() {
        let mut scan = AgentProcessScan::new();
        let t0 = Instant::now();
        // A pid that cannot exist stands in for a shell that has exited.
        scan.poll(t0, || Some(u32::MAX));
        // It never becomes the cached root, because it resolves to no process.
        assert_eq!(scan.root, None);
        let mut resolved_again = false;
        scan.poll(t0 + SCAN_INTERVAL, || {
            resolved_again = true;
            None
        });
        assert!(resolved_again, "a dead root must be re-resolved next scan");
    }

    #[test]
    fn a_terminal_with_no_pid_reports_no_agent() {
        let mut scan = AgentProcessScan::new();
        scan.poll(Instant::now(), || None);
        assert_eq!(scan.current(), None);
    }

    /// The real walk, against a real process tree: a shell running an
    /// agent-named command must be recognised through the shell, which is the
    /// exact shape of a hand-typed agent in a terminal.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_agent_under_a_shell_is_detected_through_the_whole_stack() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A symlink to `sleep` named `codex`: the kernel names the process for
        // the path it was asked to exec, so this is a process the registry
        // recognises by executable name without running a real agent. It must
        // be a symlink and not a copy — a freshly copied Mach-O exits
        // immediately under this platform's binary vetting, which would make
        // the test look like a detection failure.
        let fake = dir.path().join("codex");
        std::os::unix::fs::symlink("/bin/sleep", &fake).expect("symlink sleep as codex");
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("exec {} 30", fake.display()))
            .spawn()
            .expect("spawn shell");

        let mut scan = AgentProcessScan::new();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(5);
        let mut at = start;
        while scan.current().is_none() && Instant::now() < deadline {
            scan.poll(at, || Some(std::process::id()));
            at += SCAN_INTERVAL;
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        assert_eq!(
            scan.current(),
            Some("Codex"),
            "an agent running under a shell must be found by the tree walk"
        );
    }
}
