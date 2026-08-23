//! Windows listening sockets via the IP Helper extended TCP table.
//!
//! Windows is the easy platform here: `GetExtendedTcpTable` has a table class
//! that returns *only* listeners and carries the owning pid as a column, so
//! there is no state filter to get wrong and no inode indirection to resolve.
//!
//! The two-call sizing dance is the documented contract — the first call
//! reports how large the buffer needs to be, the second fills it — and it can
//! genuinely race: a server starting between the two calls grows the table and
//! the second call refuses again. Hence [`ATTEMPTS`].

use std::ptr;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

use crate::ListeningPort;

/// How many times a sizing round-trip is retried before giving up.
///
/// One retry covers the ordinary race (the table grew between the sizing call
/// and the filling one). Looping harder would only turn a busy machine into a
/// slow poll, and a poll that returns nothing is already a supported answer.
const ATTEMPTS: usize = 3;

pub(crate) fn listening_ports_of(pids: &[u32]) -> Vec<ListeningPort> {
    let mut out = v4(pids);
    out.extend(v6(pids));
    out
}

fn v4(pids: &[u32]) -> Vec<ListeningPort> {
    let buf = table(AF_INET as u32);
    // SAFETY: `table` returns either an empty buffer or one the kernel filled
    // for this family and class, which is `MIB_TCPTABLE_OWNER_PID` by
    // definition. The buffer is a `Vec<u32>`, so its alignment satisfies the
    // struct's (every field is 4-byte). `rows` bounds the count by the bytes
    // actually returned rather than trusting `dwNumEntries` alone.
    let Some(rows) = (unsafe { rows::<MIB_TCPTABLE_OWNER_PID, MIB_TCPROW_OWNER_PID>(&buf) }) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| pids.contains(&row.dwOwningPid))
        .map(|row| ListeningPort {
            pid: row.dwOwningPid,
            port: port_of(row.dwLocalPort),
            loopback: is_v4_loopback(row.dwLocalAddr),
        })
        .collect()
}

fn v6(pids: &[u32]) -> Vec<ListeningPort> {
    let buf = table(AF_INET6 as u32);
    // SAFETY: as `v4`, for the v6 table class.
    let Some(rows) = (unsafe { rows::<MIB_TCP6TABLE_OWNER_PID, MIB_TCP6ROW_OWNER_PID>(&buf) })
    else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| pids.contains(&row.dwOwningPid))
        .map(|row| ListeningPort {
            pid: row.dwOwningPid,
            port: port_of(row.dwLocalPort),
            loopback: is_v6_loopback(&row.ucLocalAddr),
        })
        .collect()
}

/// Whether a `dwLocalAddr` names the loopback interface.
///
/// The field is network byte order held in a little-endian word, so the low
/// byte is the *first* octet of the dotted quad — 127.0.0.1 arrives as
/// `0x0100007F`. Reading that byte answers the question without
/// reconstructing the address.
fn is_v4_loopback(addr: u32) -> bool {
    addr & 0xFF == 127
}

/// `::1`, and the IPv4-mapped loopback a dual-stack listener can report.
fn is_v6_loopback(addr: &[u8; 16]) -> bool {
    const LOOPBACK: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
    const V4_MAPPED: [u8; 12] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF];
    *addr == LOOPBACK || (addr[..12] == V4_MAPPED && addr[12] == 127)
}

/// The port from a `dwLocalPort`, which holds it in network byte order in the
/// low two bytes of a host-order word — so the two bytes need swapping back.
fn port_of(raw: u32) -> u16 {
    u16::from_be_bytes([(raw & 0xFF) as u8, ((raw >> 8) & 0xFF) as u8])
}

/// Fetch one extended TCP table as raw words, or empty if the kernel refuses.
///
/// Returned as `Vec<u32>` rather than `Vec<u8>` so the buffer is 4-byte
/// aligned: the row structs are, and `Vec<u8>` promises nothing about
/// alignment even where the allocator happens to deliver it.
fn table(family: u32) -> Vec<u32> {
    let mut size: u32 = 0;
    for _ in 0..ATTEMPTS {
        // A null buffer is how the sizing call is spelled; it is expected to
        // fail, and only `size` is worth reading from it.
        unsafe {
            GetExtendedTcpTable(
                ptr::null_mut(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u32; size.div_ceil(4) as usize];
        let rc = unsafe {
            GetExtendedTcpTable(
                buf.as_mut_ptr().cast(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if rc == 0 {
            return buf;
        }
        // Anything else — including the table having grown under us — is
        // retried with the size the kernel just reported.
    }
    Vec::new()
}

/// View a filled table buffer as its row slice.
///
/// # Safety
///
/// `buf` must be either empty or a buffer the kernel filled with a `T` whose
/// first field is `dwNumEntries` followed by a trailing array of `R`.
unsafe fn rows<T, R>(buf: &[u32]) -> Option<&[R]> {
    let bytes = std::mem::size_of_val(buf);
    if bytes < std::mem::size_of::<T>() {
        return None;
    }
    let table = buf.as_ptr().cast::<T>();
    // `dwNumEntries` is the first field of every `*_OWNER_PID` table struct.
    let claimed = unsafe { *table.cast::<u32>() } as usize;
    // The trailing array starts where the declared one-element array does.
    let first = unsafe { table.cast::<u8>().add(std::mem::size_of::<T>() - size_of::<R>()) };
    // Believe the kernel's count only as far as the bytes it returned. A
    // mismatch would be a kernel bug, but reading past the buffer on the
    // strength of a number *from* the buffer is not a risk worth taking for
    // nothing.
    let fits = (bytes - (std::mem::size_of::<T>() - size_of::<R>())) / size_of::<R>();
    Some(unsafe { std::slice::from_raw_parts(first.cast::<R>(), claimed.min(fits)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_byte_swapped_out_of_its_word() {
        // 0x1F90 is 8080; the kernel hands it over as 0x901F.
        assert_eq!(port_of(0x901F), 8080);
        assert_eq!(port_of(0xB80B), 3000);
    }

    #[test]
    fn the_v4_loopback_test_reads_the_first_octet() {
        // 127.0.0.1 as a little-endian word holding network byte order.
        assert!(is_v4_loopback(0x0100_007F));
        // 127.0.0.53, a resolver stub — still loopback.
        assert!(is_v4_loopback(0x3500_007F));
        // 0.0.0.0 is every interface, and 192.168.1.10 is a real one.
        assert!(!is_v4_loopback(0));
        assert!(!is_v4_loopback(0x0A01_A8C0));
    }

    #[test]
    fn v6_loopback_covers_both_spellings() {
        let mut addr = [0u8; 16];
        addr[15] = 1;
        assert!(is_v6_loopback(&addr), "::1");

        let mut mapped = [0u8; 16];
        mapped[10] = 0xFF;
        mapped[11] = 0xFF;
        mapped[12] = 127;
        mapped[15] = 1;
        assert!(is_v6_loopback(&mapped), "::ffff:127.0.0.1");

        assert!(!is_v6_loopback(&[0u8; 16]), ":: is every interface");
    }

    /// The real table, on this machine. Not an assertion about which ports are
    /// open — it asserts the FFI struct layouts and the sizing dance survive
    /// contact with the kernel, which is the half that silently rots.
    #[test]
    fn our_own_listener_is_found_in_the_kernels_table() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let me = std::process::id();

        let found = listening_ports_of(&[me]);
        let ours = found
            .iter()
            .find(|p| p.port == port)
            .expect("the socket this test is holding open must be in the table");
        assert_eq!(ours.pid, me);
        assert!(ours.loopback, "bound to 127.0.0.1");
    }

    #[test]
    fn another_processs_ports_are_not_ours() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        // Scoped to a pid that is not ours, the socket we are holding must not
        // come back — the filter is the whole contract of this crate.
        assert!(!listening_ports_of(&[u32::MAX]).iter().any(|p| p.port == port));
    }
}
