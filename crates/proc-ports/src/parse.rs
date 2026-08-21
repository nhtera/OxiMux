//! The text formats the Unix platforms answer socket questions in.
//!
//! Compiled on **every** platform, not only the two that read them. A parser
//! behind `#[cfg]` is a parser whose tests only run on the machine that
//! happens to be that platform, and the machine that happens to be that
//! platform is rarely the one someone is developing on. These are pure
//! string→struct functions with no IO in them, so there is nothing platform
//! about them except who calls them — and keeping them compiled everywhere
//! means a Windows laptop still catches a broken `/proc/net/tcp` parse.

/// One listening socket as `/proc/net/tcp` describes it.
///
/// The pid is *absent by design*: Linux's socket table names an inode, not an
/// owner. Turning that inode into a pid is the caller's second pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcNetRow {
    pub port: u16,
    pub inode: u64,
    pub loopback: bool,
}

/// One listening socket as `lsof -F pn` describes it. Here the pid *is*
/// present — `lsof` reports process and file together, which is the whole
/// reason macOS goes through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LsofRow {
    pub pid: u32,
    pub port: u16,
    pub loopback: bool,
}

/// The `st` column's value for `TCP_LISTEN`. Every other state is a
/// connection, not an offer to accept one.
const TCP_LISTEN: &str = "0A";

/// Listening rows of a `/proc/net/tcp` or `/proc/net/tcp6` dump.
///
/// The columns are fixed and positional:
///
/// ```text
/// sl  local_address rem_address   st tx:rx      tr:when    retrnsmt uid timeout inode
///  0: 0100007F:1F90 00000000:0000 0A 00000000:0 00:0000000 00000000   0       0 24601 …
/// ```
///
/// Anything that does not parse is skipped rather than reported: this file is
/// a kernel snapshot taken while processes come and go, and a caller polling
/// it wants the rows that made sense, not a failure because one did not.
pub fn proc_net_tcp(text: &str) -> Vec<ProcNetRow> {
    text.lines()
        .skip(1) // column header
        .filter_map(|line| {
            let mut field = line.split_whitespace();
            let _sl = field.next()?;
            let local = field.next()?;
            let _remote = field.next()?;
            if field.next()? != TCP_LISTEN {
                return None;
            }
            // Consumed 4; inode is the 10th column, so 5 more to skip.
            let inode = field.nth(5)?.parse().ok()?;
            let (addr, port) = local.rsplit_once(':')?;
            Some(ProcNetRow {
                port: u16::from_str_radix(port, 16).ok()?,
                inode,
                loopback: hex_addr_is_loopback(addr),
            })
        })
        .collect()
}

/// The inode behind a `/proc/<pid>/fd/<n>` symlink, when that fd is a socket.
///
/// Every other kind of fd — a file, a pipe, an epoll — reads as a different
/// shape and returns `None`.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// Listening rows of an `lsof -F pn` dump.
///
/// The `-F` format is one field per line, tagged by its first byte: `p` opens
/// a process record and every `n` after it belongs to that process until the
/// next `p`. That statefulness is the format, not an accident of parsing —
/// `lsof` prints the pid once and the files under it.
pub fn lsof(text: &str) -> Vec<LsofRow> {
    let mut rows = Vec::new();
    let mut pid: Option<u32> = None;
    for line in text.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => pid = rest.trim().parse().ok(),
            // A name arriving before any `p` line has no owner to attribute
            // it to, so it is dropped rather than guessed at.
            "n" => {
                if let Some(pid) = pid
                    && let Some((host, port)) = rest.rsplit_once(':')
                    // `-sTCP:LISTEN` should have excluded connections, but a
                    // peer arrow here would make `rsplit_once` read the remote
                    // port as a local one. Cheap to refuse outright.
                    && !rest.contains("->")
                    && let Ok(port) = port.trim().parse::<u16>()
                {
                    rows.push(LsofRow {
                        pid,
                        port,
                        loopback: host_is_loopback(host),
                    });
                }
            }
            _ => {}
        }
    }
    rows
}

/// Whether an `lsof` host half names the loopback interface.
///
/// `*` means every interface, which is the opposite claim.
fn host_is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "[::1]" | "localhost")
        || host.starts_with("127.")
        || host.starts_with("[::ffff:127.")
}

/// Whether a `/proc/net/tcp` address half names the loopback interface.
///
/// The encoding is not "hex of the dotted quad": each 32-bit word is written
/// in *host* byte order, so 127.0.0.1 appears as `0100007F` on the
/// little-endian machines this runs on. Reading the low byte gets the first
/// octet back without reconstructing the whole address.
fn hex_addr_is_loopback(addr: &str) -> bool {
    match addr.len() {
        8 => u32::from_str_radix(addr, 16).is_ok_and(|word| word & 0xFF == 127),
        // `::1` — one bit set, in the last word's low byte.
        32 => {
            if addr.eq_ignore_ascii_case("00000000000000000000000001000000") {
                return true;
            }
            // An IPv4-mapped address (`::ffff:a.b.c.d`) carries a v4 address in
            // its final word and is loopback exactly when that word is.
            addr[16..24].eq_ignore_ascii_case("FFFF0000") && hex_addr_is_loopback(&addr[24..])
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two real lines from a Linux box: a listener and an established
    /// connection. Only the listener is an offer to accept.
    const NET_TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 24601 1 0000000000000000 100 0 0 10 0
   1: 0100007F:1F90 0100007F:C1A2 01 00000000:00000000 00:00000000 00000000  1000        0 24999 1 0000000000000000 20 4 30 10 -1
   2: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 24777 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn only_listening_rows_survive() {
        let rows = proc_net_tcp(NET_TCP);
        assert_eq!(
            rows.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![8080, 8081],
            "an established connection is not something you can open in a browser"
        );
    }

    #[test]
    fn the_inode_is_read_from_the_tenth_column() {
        let rows = proc_net_tcp(NET_TCP);
        assert_eq!(rows[0].inode, 24601);
        assert_eq!(rows[1].inode, 24777);
    }

    #[test]
    fn a_wildcard_bind_is_not_loopback() {
        let rows = proc_net_tcp(NET_TCP);
        assert!(rows[0].loopback, "0100007F is 127.0.0.1");
        assert!(!rows[1].loopback, "00000000 is every interface");
    }

    #[test]
    fn the_header_is_never_mistaken_for_a_row() {
        assert!(proc_net_tcp("  sl  local_address rem_address st\n").is_empty());
        assert!(proc_net_tcp("").is_empty());
    }

    #[test]
    fn a_truncated_line_is_skipped_not_panicked_on() {
        // A read of /proc racing the kernel can hand back a short line.
        assert!(proc_net_tcp("hdr\n   0: 0100007F:1F90 00000000:0000 0A\n").is_empty());
    }

    #[test]
    fn ipv6_loopback_is_recognised_in_both_spellings() {
        let v6 = "\
hdr
   0: 00000000000000000000000001000000:1F90 00000000000000000000000000000000:0000 0A 0 0 0 0 0 31337 1
   1: 0000000000000000FFFF00000100007F:1F91 00000000000000000000000000000000:0000 0A 0 0 0 0 0 31338 1
   2: 00000000000000000000000000000000:1F92 00000000000000000000000000000000:0000 0A 0 0 0 0 0 31339 1
";
        let rows = proc_net_tcp(v6);
        assert!(rows[0].loopback, "::1");
        assert!(rows[1].loopback, "::ffff:127.0.0.1");
        assert!(!rows[2].loopback, ":: is every interface");
    }

    #[test]
    fn a_socket_link_yields_its_inode_and_nothing_else_does() {
        assert_eq!(socket_inode("socket:[24601]"), Some(24601));
        assert_eq!(socket_inode("/home/me/notes.txt"), None);
        assert_eq!(socket_inode("pipe:[24601]"), None);
        assert_eq!(socket_inode("socket:[]"), None);
    }

    #[test]
    fn lsof_attributes_every_name_to_the_pid_above_it() {
        let text = "p4711\nn127.0.0.1:3000\nn*:8080\np4712\nn[::1]:9229\n";
        assert_eq!(
            lsof(text),
            vec![
                LsofRow { pid: 4711, port: 3000, loopback: true },
                LsofRow { pid: 4711, port: 8080, loopback: false },
                LsofRow { pid: 4712, port: 9229, loopback: true },
            ]
        );
    }

    #[test]
    fn lsof_names_without_an_owner_are_dropped() {
        // Defensive: a truncated read could start mid-record.
        assert!(lsof("n127.0.0.1:3000\n").is_empty());
    }

    #[test]
    fn lsof_refuses_a_connection_row() {
        // If `-sTCP:LISTEN` ever stops filtering, rsplit_once would read the
        // *remote* port as a local one — a wrong port is worse than no port.
        assert!(lsof("p4711\nn127.0.0.1:3000->127.0.0.1:51000\n").is_empty());
    }

    #[test]
    fn lsof_ignores_field_tags_it_was_not_asked_for() {
        assert_eq!(
            lsof("p4711\ncnode\nfcwd\nn127.0.0.1:3000\n"),
            vec![LsofRow { pid: 4711, port: 3000, loopback: true }]
        );
    }

    #[test]
    fn a_port_that_does_not_fit_a_u16_is_not_a_port() {
        assert!(lsof("p4711\nn127.0.0.1:99999\n").is_empty());
        assert!(lsof("p4711\nn127.0.0.1:http\n").is_empty());
    }
}
