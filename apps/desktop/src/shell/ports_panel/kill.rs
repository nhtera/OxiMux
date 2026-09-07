//! Stopping the process behind a port.
//!
//! **Why the panel offers this at all.** "Port 3000 is already in use" is the
//! most common way a dev server fails to start, and the fix is always the
//! same: find what has it, stop that, try again. The finding half is what the
//! rest of this module already does; making the user leave for a terminal to
//! type `kill $(lsof -ti:3000)` is asking them to re-derive by hand a pid the
//! panel is already showing them.
//!
//! **Why only project-attributed rows.** An external row is something this
//! window merely noticed — a system daemon, another app's helper, a container.
//! The panel has no evidence the user meant to be responsible for it, and a
//! stop button next to `ControlCenter` is a footgun with a tooltip. So the
//! action exists only where attribution said "this is yours"; see
//! [`super::scan::PortRow::is_owned`].
//!
//! **Why the pid is re-checked.** A rendered row is up to one poll old, and a
//! pid that exited in that window can have been recycled by something else.
//! Killing on a stale row would then kill an innocent process that happens to
//! have inherited the number. So [`stop_listener`] re-reads the socket table
//! at the moment of the click and refuses unless that pid is *still* listening
//! on *that* port — the same fact the row was claiming, re-established rather
//! than trusted.
//!
//! **Why SIGTERM first.** A dev server asked to stop flushes its build cache,
//! removes its socket file, and lets its own children exit; killed outright it
//! leaves all three behind. The escalation to SIGKILL exists for the process
//! that ignores the polite request, and it happens on a detached timer so the
//! UI thread never waits on it.

use std::time::Duration;

/// How long a process gets to exit on its own before the escalation lands.
///
/// Matches the agent runtimes' grace elsewhere in the app: long enough for a
/// bundler to finish writing, short enough that a user who clicked Stop does
/// not start wondering whether the click registered.
const TERM_GRACE: Duration = Duration::from_secs(5);

/// Why a stop request was refused. Every variant is a sentence the panel can
/// show as-is — a refusal the user cannot read is indistinguishable from a
/// button that does nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The row was stale: nothing with that pid holds that port any more.
    GoneOrRecycled,
    /// The pid is this application, or something it needs to keep running.
    ProtectedProcess,
    /// The kernel refused — another user's process, most often.
    Denied,
}

impl Refusal {
    pub(crate) fn message(&self, port: u16) -> String {
        match self {
            Self::GoneOrRecycled => {
                format!("Nothing is listening on {port} any more")
            }
            Self::ProtectedProcess => {
                "OxiMux will not stop its own process".to_string()
            }
            Self::Denied => format!("Not allowed to stop the process on {port}"),
        }
    }
}

/// Pids this app must never signal, whatever a row says.
///
/// The app itself and its parent: OxiMux binds no TCP port of its own, so
/// neither should ever reach this code — which is exactly why the guard is
/// cheap enough to keep. A future feature that does listen (a preview server,
/// the remote-control bridge) would otherwise ship a button that quits the app.
fn is_protected(pid: u32) -> bool {
    // Pid 0 and 1 are the kernel and init; on Windows pid 0 is what the
    // socket table attributes kernel-held listeners to, which is precisely
    // the row a user would be tempted to click.
    pid <= 1 || pid == std::process::id() || Some(pid) == parent_pid()
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::getppid() } as u32)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Ask the process holding `port` to stop, escalating if it will not.
/// **Blocking only for the initial signal** — the grace wait and the
/// escalation run on `spawner`.
///
/// Returns `Ok(())` once the polite signal has been delivered, not once the
/// process is gone: a caller that waited for the latter would be blocking a
/// click on a process that may take seconds to unwind. The next poll is what
/// makes the row disappear, which is the same mechanism that notices a server
/// the user stopped in a terminal.
pub(crate) fn stop_listener(
    pid: u32,
    port: u16,
    spawner: impl FnOnce(Duration, Box<dyn FnOnce() + Send>) + 'static,
) -> Result<(), Refusal> {
    if is_protected(pid) {
        return Err(Refusal::ProtectedProcess);
    }
    // Re-establish the row's claim rather than trusting it. This is the guard
    // against a recycled pid, and it costs one scoped socket read.
    if !still_listening(pid, port) {
        return Err(Refusal::GoneOrRecycled);
    }
    signal(pid, Signal::Term)?;
    spawner(
        TERM_GRACE,
        Box::new(move || {
            // Re-check before escalating for the same reason as above, and
            // because the ordinary outcome is that SIGTERM already worked.
            if still_listening(pid, port) {
                let _ = signal(pid, Signal::Kill);
            }
        }),
    );
    Ok(())
}

/// Whether `pid` still holds `port`. A scoped query — the pid is known, so
/// there is no reason to read the whole machine's table to answer this.
fn still_listening(pid: u32, port: u16) -> bool {
    oximux_proc_ports::listening_ports_of(&[pid])
        .iter()
        .any(|row| row.port == port)
}

/// `unix` and `windows` are the only arms, and there is deliberately no
/// fallback: a platform this app can be built for and cannot signal on would
/// ship a Stop button that silently does nothing, and a compile error is the
/// better way to find that out.
#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal(pid: u32, which: Signal) -> Result<(), Refusal> {
    let sig = match which {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // Signalling the pid rather than its process group: the group of a server
    // started in a terminal contains that terminal's shell, and taking the
    // shell down with the server would close the pane the user is watching.
    let rc = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if rc == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        // ESRCH: it exited between the check and the signal. That is the
        // outcome the click wanted, so it is not a failure.
        Some(libc::ESRCH) => Ok(()),
        _ => Err(Refusal::Denied),
    }
}

#[cfg(windows)]
fn signal(pid: u32, which: Signal) -> Result<(), Refusal> {
    // Windows has no SIGTERM. `taskkill` without `/F` posts WM_CLOSE and asks
    // a console process to end, which is the closest thing to a polite
    // request; `/F` is the escalation. Shelling out rather than calling
    // `TerminateProcess` directly because the polite half has no single API
    // equivalent, and having the two paths differ in shape is how one of them
    // rots unnoticed.
    let mut cmd = std::process::Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string()]);
    if matches!(which, Signal::Kill) {
        cmd.arg("/F");
    }
    match cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        // 0 is "the request was delivered". 128 is `taskkill`'s "no such
        // process", which is the ESRCH case above: it exited between the
        // re-check and the signal, which is the outcome the click wanted.
        //
        // Every other status is a refusal — most often access denied on a
        // process owned by another user or running elevated — and must be
        // reported as one. Treating them all as success was a lie the panel
        // showed as "Stopping the process on 3000" while the port stayed up.
        Ok(status) => match status.code() {
            Some(0) | Some(128) => Ok(()),
            _ => Err(Refusal::Denied),
        },
        Err(_) => Err(Refusal::Denied),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A spawner that records whether an escalation was scheduled without
    /// ever running it — the tests below must not depend on a timer.
    fn recording_spawner(flag: Arc<AtomicBool>) -> impl FnOnce(Duration, Box<dyn FnOnce() + Send>) {
        move |_, _| flag.store(true, Ordering::SeqCst)
    }

    #[test]
    fn our_own_process_is_protected() {
        assert!(is_protected(std::process::id()));
    }

    #[test]
    fn the_kernel_and_init_are_protected() {
        // Windows attributes kernel-held listeners to pid 0, and those rows do
        // reach the panel.
        assert!(is_protected(0));
        assert!(is_protected(1));
    }

    #[test]
    fn stopping_our_own_process_is_refused_before_any_signal() {
        let escalated = Arc::new(AtomicBool::new(false));
        let result = stop_listener(
            std::process::id(),
            3000,
            recording_spawner(escalated.clone()),
        );
        assert_eq!(result, Err(Refusal::ProtectedProcess));
        assert!(!escalated.load(Ordering::SeqCst));
    }

    #[test]
    fn a_stale_row_is_refused_rather_than_signalling_a_recycled_pid() {
        // A pid past every platform's ceiling cannot be listening on anything,
        // which is the shape of a row whose process exited a poll ago.
        let escalated = Arc::new(AtomicBool::new(false));
        let result = stop_listener(u32::MAX, 3000, recording_spawner(escalated.clone()));
        assert_eq!(result, Err(Refusal::GoneOrRecycled));
        assert!(!escalated.load(Ordering::SeqCst));
    }

    #[test]
    fn a_live_listener_that_is_not_on_that_port_is_refused() {
        // Our own process holds a socket, but not this port. Guards against a
        // check that only asks "is the pid alive".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let ours = listener.local_addr().expect("local addr").port();
        let other = ours.wrapping_add(1).max(1024);
        assert!(!still_listening(std::process::id(), other));
        assert!(still_listening(std::process::id(), ours));
    }

    /// End to end against a real process: spawn a listener, stop it, and
    /// assert the socket is gone. The only test here that proves the signal
    /// path actually signals.
    #[cfg(unix)]
    #[test]
    fn a_spawned_listener_is_stopped_by_the_polite_signal() {
        // A shell holding a socket open with nothing else to do: it exits on
        // SIGTERM, which is the ordinary case the escalation never sees.
        let mut child = std::process::Command::new("python3")
            .args(["-c", "import socket,time\ns=socket.socket()\ns.bind(('127.0.0.1',0))\ns.listen(1)\nprint(s.getsockname()[1],flush=True)\ntime.sleep(60)"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn a listener");
        let mut port_line = String::new();
        {
            use std::io::{BufRead as _, BufReader};
            let stdout = child.stdout.take().expect("piped stdout");
            BufReader::new(stdout)
                .read_line(&mut port_line)
                .expect("the child prints its port");
        }
        let port: u16 = port_line.trim().parse().expect("a port number");
        let pid = child.id();
        assert!(still_listening(pid, port), "the child is holding the socket");

        let escalated = Arc::new(AtomicBool::new(false));
        stop_listener(pid, port, recording_spawner(escalated.clone())).expect("the stop lands");
        assert!(escalated.load(Ordering::SeqCst), "an escalation is armed");

        // SIGTERM is asynchronous; wait for the process rather than the clock.
        let status = child.wait().expect("the child exits");
        assert!(!status.success() || status.code().is_none());
        let _ = child.kill();
    }
}
