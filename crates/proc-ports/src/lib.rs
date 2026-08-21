//! Which local TCP ports a given set of processes is listening on.
//!
//! A dev server announces itself by printing a URL, and then that line scrolls
//! away. The socket does not scroll away: it exists for exactly as long as the
//! server is willing to accept a connection, and the kernel will name the
//! process holding it. Asking the kernel is therefore the only answer to
//! "what is listening, and did I start it" that survives a cleared screen, a
//! server that prints nothing, and a server that printed a URL it is not
//! actually reachable at.
//!
//! Sibling of [`oximux-proc-tree`](../../proc-tree) and
//! [`oximux-proc-cwd`](../../proc-cwd), and its own crate for the same reason
//! both of those are: dependency-light kernel introspection whose consumers
//! share nothing else with each other.
//!
//! ## Why the query takes a pid set
//!
//! [`listening_ports_of`] answers for pids the caller names rather than
//! returning every listener on the machine. Two reasons, the second decisive:
//!
//! * A caller only wants ports it can attribute to a terminal it owns. A panel
//!   that also listed the OS's own listeners would be a security dashboard
//!   nobody asked for, and it would bury the one row that matters.
//! * On Linux the owning pid is *not in the socket table*. `/proc/net/tcp`
//!   names an inode, and turning an inode into a pid means reading
//!   `/proc/<pid>/fd` for every process on the system. Scoped to a handful of
//!   candidates that is a few dozen `readlink` calls; unscoped it is thousands
//!   of them, on a cadence, forever.
//!
//! ## Per platform
//!
//! **Windows**: `GetExtendedTcpTable(TCP_TABLE_OWNER_PID_LISTENER)` for both
//! address families. The owning pid is a column, so there is no second pass.
//!
//! **Linux**: `/proc/net/tcp` and `/proc/net/tcp6` for the listening sockets
//! and their inodes, then the candidate pids' own `/proc/<pid>/fd` links to
//! find which of them holds one.
//!
//! **macOS**: `lsof`. `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` is the direct
//! route and is what a bigger crate should use, but `socket_fdinfo` is not in
//! `libc` — taking it would mean hand-declaring a nested union whose layout is
//! private to the SDK, inside a crate whose whole argument for existing is
//! being small enough to audit. `lsof` ships with the OS, its `-F` output is a
//! documented machine format, and the cost is one short-lived process every
//! few seconds. If that ever stops being cheap enough, the honest fix is the
//! FFI, not a faster parser.

pub mod parse;

#[cfg_attr(target_os = "macos", path = "macos.rs")]
#[cfg_attr(target_os = "linux", path = "linux.rs")]
#[cfg_attr(target_os = "windows", path = "windows.rs")]
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux", target_os = "windows")),
    path = "unsupported.rs"
)]
mod imp;

/// One process's offer to accept connections on one local port.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListeningPort {
    /// The process holding the socket.
    pub pid: u32,
    pub port: u16,
    /// Bound to loopback rather than to a wildcard or routable address.
    ///
    /// A presentation hint, not a security claim: it says the server chose to
    /// be reachable only from this machine, which is worth telling the user
    /// because the opposite — a dev server bound to `0.0.0.0` on a shared
    /// network — is usually unintentional.
    pub loopback: bool,
}

/// Most pids one query will consider.
///
/// The caller's set comes from walking terminal process trees, which are
/// already bounded; this only stops a pathological fork storm from turning a
/// poll into a `/proc` crawl. Excess pids are dropped, not sampled: a stable
/// prefix beats a shuffling window when the result feeds a list a person is
/// reading.
const MAX_PIDS: usize = 256;

/// Every local TCP port that a process in `pids` is listening on.
///
/// Returns empty when nothing matched, when the platform has no
/// implementation, or when the kernel refused the query — callers treat all
/// three as "nothing detected", never as an error worth surfacing. A port
/// vanishing from consecutive calls is the normal way a server exit is
/// noticed.
///
/// The result is sorted by port then pid, and carries one row per pid+port:
/// a server bound to both `127.0.0.1` and `[::1]` is one thing a person can
/// open, not two.
pub fn listening_ports_of(pids: &[u32]) -> Vec<ListeningPort> {
    if pids.is_empty() {
        return Vec::new();
    }
    let mut wanted = pids.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    wanted.truncate(MAX_PIDS);
    collapse(imp::listening_ports_of(&wanted))
}

/// Fold per-socket rows into one row per pid+port.
///
/// A server that binds both address families produces two sockets and one
/// port. It is marked loopback only when *every* binding was — one wildcard
/// bind is enough to make it reachable from off the machine, and that is the
/// half worth reporting.
fn collapse(mut rows: Vec<ListeningPort>) -> Vec<ListeningPort> {
    rows.sort_unstable_by_key(|r| (r.port, r.pid));
    let mut out: Vec<ListeningPort> = Vec::with_capacity(rows.len());
    for row in rows {
        match out.last_mut() {
            Some(prev) if prev.port == row.port && prev.pid == row.pid => {
                prev.loopback &= row.loopback;
            }
            _ => out.push(row),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, port: u16, loopback: bool) -> ListeningPort {
        ListeningPort { pid, port, loopback }
    }

    #[test]
    fn an_empty_pid_set_asks_the_kernel_nothing() {
        assert!(listening_ports_of(&[]).is_empty());
    }

    #[test]
    fn both_address_families_of_one_server_are_one_row() {
        assert_eq!(
            collapse(vec![row(10, 3000, true), row(10, 3000, true)]),
            vec![row(10, 3000, true)]
        );
    }

    #[test]
    fn a_wildcard_binding_wins_over_a_loopback_one() {
        // Bound to both: it *is* reachable from the network, and saying
        // "local only" would be the dangerous direction to be wrong in.
        assert_eq!(
            collapse(vec![row(10, 3000, true), row(10, 3000, false)]),
            vec![row(10, 3000, false)]
        );
    }

    #[test]
    fn two_processes_on_one_port_stay_two_rows() {
        // SO_REUSEPORT, or a parent and a forked worker sharing the listener.
        let out = collapse(vec![row(20, 3000, true), row(10, 3000, true)]);
        assert_eq!(out, vec![row(10, 3000, true), row(20, 3000, true)]);
    }

    #[test]
    fn rows_come_back_ordered_by_port() {
        let out = collapse(vec![row(10, 8080, true), row(10, 3000, true), row(10, 5173, true)]);
        assert_eq!(
            out.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 5173, 8080]
        );
    }

    /// Not an assertion about *this* machine's ports — it asserts the call is
    /// safe to make on every platform and honest about a pid that cannot be
    /// listening. `u32::MAX` is past every platform's pid ceiling, and unlike
    /// pid 0 it is not a real owner: Windows attributes kernel-held sockets to
    /// pid 0, so that one is not the free assertion it looks like.
    #[test]
    fn a_pid_that_cannot_own_a_socket_owns_none() {
        assert!(listening_ports_of(&[u32::MAX]).is_empty());
    }
}
