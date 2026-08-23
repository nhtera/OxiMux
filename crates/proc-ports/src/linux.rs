//! Linux listening sockets via `/proc/net/tcp` and the candidates' fd links.
//!
//! Linux is the platform that makes this crate's scoped API necessary. The
//! socket table names an *inode*, never an owner, so the pid has to be found
//! by asking which process holds a file descriptor pointing at that inode —
//! and the only way to ask is to read the fd directory of a process and see.
//! Doing that for every process on the machine, every poll, is the design this
//! crate exists to avoid; doing it for the caller's handful of candidates is a
//! few dozen `readlink` calls.
//!
//! The order matters: the socket table is read *first*, so the inode set is
//! small and the fd walk is a hash lookup per descriptor rather than a scan.

use std::collections::HashMap;

use crate::ListeningPort;
use crate::parse;

/// Most file descriptors examined per candidate process.
///
/// A dev server holds tens; a database holds hundreds. The cap is here so a
/// process that has leaked descriptors cannot turn a poll into a stall — and
/// a listening socket is opened early, so a truncated walk keeps the rows that
/// matter.
const MAX_FDS: usize = 4096;

pub(crate) fn listening_ports_of(pids: &[u32]) -> Vec<ListeningPort> {
    let mut sockets: HashMap<u64, (u16, bool)> = HashMap::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        // A missing table is an IPv6-less kernel, not a failure.
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for row in parse::proc_net_tcp(&text) {
            sockets.insert(row.inode, (row.port, row.loopback));
        }
    }
    if sockets.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for &pid in pids {
        // A process that exited between the caller's tree walk and this read
        // is the common case, not an error.
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for entry in dir.flatten().take(MAX_FDS) {
            let Ok(link) = std::fs::read_link(entry.path()) else {
                continue;
            };
            let Some(inode) = parse::socket_inode(&link.to_string_lossy()) else {
                continue;
            };
            if let Some(&(port, loopback)) = sockets.get(&inode) {
                out.push(ListeningPort { pid, port, loopback });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real `/proc`, on this machine. Not an assertion about which ports
    /// are open — it asserts the table parse and the inode→fd join survive
    /// contact with the kernel, which is the half that silently rots.
    #[test]
    fn our_own_listener_is_found_through_its_inode() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let me = std::process::id();

        let found = listening_ports_of(&[me]);
        let ours = found
            .iter()
            .find(|p| p.port == port)
            .expect("the socket this test is holding open must be reachable from our own fds");
        assert_eq!(ours.pid, me);
        assert!(ours.loopback, "bound to 127.0.0.1");
    }

    #[test]
    fn another_processs_ports_are_not_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        assert!(
            !listening_ports_of(&[u32::MAX]).iter().any(|p| p.port == port),
            "the fd walk is scoped to the pids asked for"
        );
    }
}
