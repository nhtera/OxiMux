//! Linux listening sockets via `/proc/net/tcp` and the candidates' fd links.
//!
//! Linux is the platform where the scoped and unscoped queries genuinely
//! differ. The socket table names an *inode*, never an owner, so the pid has
//! to be found by asking which process holds a file descriptor pointing at
//! that inode — and the only way to ask is to read the fd directory of a
//! process and see. For the caller's handful of candidates that is a few dozen
//! `readlink` calls; for the whole machine it is one pass over `/proc`.
//!
//! The order matters, and it is what keeps the unscoped pass affordable: the
//! socket table is read *first*, so the inode set is small and every
//! descriptor examined is a hash lookup rather than a scan. The walk also
//! stops as soon as every listening inode has an owner — on a normal machine
//! that is long before `/proc` runs out of processes.
//!
//! Processes owned by another user answer `EACCES` and are skipped. Their
//! ports are still reported by [`crate::listening_ports`] where the socket
//! table lists them, they simply arrive without an owning pid to name — so
//! nothing is invented, and the row is dropped rather than misattributed.

use std::collections::{HashMap, HashSet};

use crate::ListeningPort;
use crate::parse;

/// Most file descriptors examined per candidate process.
///
/// A dev server holds tens; a database holds hundreds. The cap is here so a
/// process that has leaked descriptors cannot turn a poll into a stall — and
/// a listening socket is opened early, so a truncated walk keeps the rows that
/// matter.
const MAX_FDS: usize = 4096;

pub(crate) fn listening_ports(pids: Option<&[u32]>) -> Vec<ListeningPort> {
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

    let candidates: Vec<u32> = match pids {
        Some(pids) => pids.to_vec(),
        None => all_pids(),
    };

    let mut out = Vec::new();
    // Inodes that have found an owner. An unscoped walk can stop once every
    // listening inode is in here — the inodes are the question, and the
    // remaining processes have nothing left to answer. A set rather than a
    // countdown because one process can hold the same socket on two
    // descriptors (a server that `dup`ed its listener), and counting those
    // twice would end the walk with sockets still unattributed.
    let mut claimed: HashSet<u64> = HashSet::with_capacity(sockets.len());
    for pid in candidates {
        if claimed.len() == sockets.len() {
            break;
        }
        // A process that exited between the caller's tree walk and this read
        // is the common case, not an error. So is another user's process,
        // which answers `EACCES`.
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
                claimed.insert(inode);
            }
        }
    }
    out
}

/// Every pid `/proc` currently names.
///
/// Sorted, because the walk stops early once every listening inode has an
/// owner and directory order is not stable across reads — an unsorted walk
/// would attribute a socket shared by a parent and a forked worker to
/// whichever of them the filesystem happened to list first, and that answer
/// would change between polls. Ascending pid means the parent, which is the
/// older process, is found first.
fn all_pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = dir
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse().ok())
        .collect();
    pids.sort_unstable();
    pids
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

        let found = listening_ports(Some(&[me]));
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
            !listening_ports(Some(&[u32::MAX])).iter().any(|p| p.port == port),
            "the fd walk is scoped to the pids asked for"
        );
    }

    /// The unscoped query must find the same socket without being told whose
    /// it is — the half that makes a port started outside the app visible.
    #[test]
    fn an_unscoped_query_finds_a_listener_it_was_not_told_about() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        assert!(
            listening_ports(None)
                .iter()
                .any(|p| p.port == port && p.pid == std::process::id()),
            "a full /proc walk must reach our own fd table"
        );
    }
}
