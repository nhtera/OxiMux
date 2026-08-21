//! macOS listening sockets via `lsof`.
//!
//! The direct route is `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)`, and a bigger
//! crate should take it. `socket_fdinfo` is not in `libc`, so taking it here
//! would mean hand-declaring a nested union whose layout is private to the
//! SDK — inside a crate whose whole argument for existing is being small
//! enough to audit. The trade is deliberate: one short-lived process every few
//! seconds, in exchange for a format Apple documents and a parser that is
//! tested on every platform rather than only on a Mac.
//!
//! The invocation asks for exactly what this crate returns and nothing else:
//! `-nP` suppresses the DNS and service-name lookups that would otherwise make
//! this slow and lossy, `-iTCP -sTCP:LISTEN` narrows to listeners, `-a`
//! combines that with `-p` rather than unioning with it, and `-F pn` selects
//! the two fields the parser reads.

use std::process::{Command, Stdio};

use crate::ListeningPort;
use crate::parse;

/// Where macOS ships `lsof`.
///
/// Spelled absolutely, not looked up on `PATH`: an app launched from Finder
/// inherits a `PATH` that need not contain `/usr/sbin`, and a ports panel that
/// works from a terminal launch and silently finds nothing from a Dock launch
/// is the worst of both.
const LSOF: &str = "/usr/sbin/lsof";

pub(crate) fn listening_ports_of(pids: &[u32]) -> Vec<ListeningPort> {
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let output = Command::new(LSOF)
        .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-a", "-p", &list, "-F", "pn"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    // `lsof` exits non-zero when nothing matched, which is the ordinary
    // answer, so the status is not consulted — only what it printed.
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    parse::lsof(&text)
        .into_iter()
        // `-p` is a filter, not a promise: re-check rather than trust it, so a
        // future flag change cannot quietly widen what this crate reports.
        .filter(|row| pids.contains(&row.pid))
        .map(|row| ListeningPort {
            pid: row.pid,
            port: row.port,
            loopback: row.loopback,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `lsof`, on this machine. Not an assertion about which ports
    /// are open — it asserts the invocation and the `-F` parse survive contact
    /// with the tool, which is the half that silently rots.
    #[test]
    fn our_own_listener_is_found_by_lsof() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let me = std::process::id();

        let found = listening_ports_of(&[me]);
        let ours = found
            .iter()
            .find(|p| p.port == port)
            .expect("the socket this test is holding open must be reported by lsof");
        assert_eq!(ours.pid, me);
        assert!(ours.loopback, "bound to 127.0.0.1");
    }

    #[test]
    fn another_processs_ports_are_not_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        assert!(
            !listening_ports_of(&[u32::MAX]).iter().any(|p| p.port == port),
            "the query is scoped to the pids asked for"
        );
    }
}
