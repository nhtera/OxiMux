//! macOS process introspection via libproc and `sysctl`.
//!
//! `proc_listpids(PROC_PPID_ONLY, ppid, …)` asks the kernel directly for one
//! process's children, so a walk costs a syscall per generation rather than a
//! scan of the whole process table.

use std::ffi::CStr;

/// `proc_listpids` selector meaning "pids whose parent is `typeinfo`".
/// From `<libproc.h>`; hard-coded for the same reason the `proc_pidinfo`
/// flavors are in the sibling cwd crate — one constant is cheaper than a
/// dependency.
const PROC_PPID_ONLY: u32 = 6;

/// `2 * MAXCOMLEN + 1` — the buffer `proc_name` writes into, from
/// `<sys/param.h>`. Names longer than this come back truncated, which is why
/// the caller matches on a prefix-tolerant registry rather than equality.
const PROC_NAME_BUF: usize = 2 * 16 + 1;

/// `KERN_PROCARGS2` from `<sys/sysctl.h>`: the argument vector of one pid,
/// prefixed with `argc`.
const KERN_PROCARGS2: libc::c_int = 49;

/// First buffer size tried for `KERN_PROCARGS2`. The block holds the whole
/// environment after the arguments, so it must be sized for both — a typical
/// login shell's lands a few KiB in, and this clears it on the first call.
const PROCARGS_FIRST_TRY: usize = 16 * 1024;

/// Ceiling on the `KERN_PROCARGS2` retry ladder. `KERN_ARGMAX` is 1 MiB, but
/// a process needing more than this has an environment far outside anything a
/// terminal spawns, and the caller degrades to matching the executable name
/// rather than allocating a megabyte inside a poll.
const PROCARGS_MAX: usize = 256 * 1024;

unsafe extern "C" {
    fn proc_listpids(
        r#type: u32,
        typeinfo: u32,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
    fn proc_name(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
}

pub(crate) fn children_of(pid: u32) -> Vec<u32> {
    // Sizing query first: a NULL buffer makes proc_listpids report the bytes
    // it would fill. A process with no children reports 0 and we allocate
    // nothing.
    let needed =
        unsafe { proc_listpids(PROC_PPID_ONLY, pid, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Vec::new();
    }
    let count = needed as usize / std::mem::size_of::<libc::pid_t>();
    let mut buf: Vec<libc::pid_t> = vec![0; count];
    let filled = unsafe {
        proc_listpids(
            PROC_PPID_ONLY,
            pid,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            needed,
        )
    };
    if filled <= 0 {
        return Vec::new();
    }
    let filled = (filled as usize / std::mem::size_of::<libc::pid_t>()).min(count);
    buf.truncate(filled);
    // The kernel pads the tail with zeroes when fewer children exist than the
    // sizing query predicted (a child can exit between the two calls), and
    // pid 0 is the kernel itself — never a descendant of a shell.
    buf.into_iter()
        .filter(|&p| p > 0)
        .map(|p| p as u32)
        .collect()
}

pub(crate) fn name_of_pid(pid: u32) -> Option<String> {
    let mut buf = [0u8; PROC_NAME_BUF];
    let n = unsafe {
        proc_name(
            pid as libc::c_int,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&buf).ok()?.to_str().ok()?;
    (!name.is_empty()).then(|| name.to_owned())
}

pub(crate) fn argv_of_pid(pid: u32) -> Option<Vec<String>> {
    // pid_t is i32; a u32 above i32::MAX would wrap negative and be read as a
    // selector rather than a pid.
    if pid > i32::MAX as u32 {
        return None;
    }
    let mut mib = [libc::CTL_KERN, KERN_PROCARGS2, pid as libc::c_int];
    let mut cap = PROCARGS_FIRST_TRY;
    loop {
        let mut buf = vec![0u8; cap];
        let mut len = cap;
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        // EINVAL for a dead pid, EPERM for another user's process — both mean
        // "no answer", which the caller reads as "match on the name instead".
        if rc != 0 || len < std::mem::size_of::<u32>() {
            return None;
        }
        // An undersized buffer does NOT fail and does NOT return a usable
        // prefix: the call succeeds and writes a block whose argument region
        // is blank. The one honest signal is that the kernel filled the
        // buffer exactly — a block that fit reports its true, smaller size.
        // (Measured against this kernel; a fixed buffer silently yielded
        // empty argument vectors for every process with a normal
        // environment.)
        if len < cap {
            buf.truncate(len);
            return parse_procargs2(&buf);
        }
        cap = cap.checked_mul(2).filter(|&c| c <= PROCARGS_MAX)?;
    }
}

/// Decode a `KERN_PROCARGS2` block into an argument vector.
///
/// Layout: `argc` as a native-endian `u32`, the executable path, nul padding
/// that aligns what follows, then `argc` nul-terminated arguments, then the
/// environment (which we stop before — it can hold secrets, and nothing here
/// needs it).
fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    let argc = u32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = buf.get(4..)?;
    // Step over the exec path, then over the run of nuls padding it. A block
    // that opens with a nul carries no exec path, which is what an undersized
    // read looks like — refuse it rather than reporting an empty argv.
    let path_end = rest.iter().position(|&b| b == 0).filter(|&n| n > 0)?;
    let mut cursor = path_end;
    while rest.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut args: Vec<String> = Vec::with_capacity(argc.min(8));
    for _ in 0..argc {
        let tail = rest.get(cursor..)?;
        if tail.is_empty() {
            break;
        }
        // A final argument truncated by our fixed buffer has no terminator;
        // take what is there and stop, rather than discarding the whole read.
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        args.push(String::from_utf8_lossy(&tail[..end]).into_owned());
        cursor += end + 1;
    }
    (!args.is_empty()).then_some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_argc_path_and_arguments() {
        let mut buf = 2u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/local/bin/node\0\0\0");
        buf.extend_from_slice(b"node\0");
        buf.extend_from_slice(b"/opt/homebrew/bin/gemini\0");
        buf.extend_from_slice(b"SOME_SECRET=hunter2\0");
        assert_eq!(
            parse_procargs2(&buf),
            Some(vec!["node".into(), "/opt/homebrew/bin/gemini".into()]),
            "the environment following argv must not be read"
        );
    }

    #[test]
    fn a_block_with_no_exec_path_is_rejected() {
        // The shape an undersized sysctl read returns: a successful call over
        // a blank region. Reporting an empty argv here would read as "this
        // process has no arguments" instead of "we could not tell".
        let mut buf = 3u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(&[0u8; 64]);
        assert_eq!(parse_procargs2(&buf), None);
    }

    #[test]
    fn a_tail_with_no_terminator_does_not_lose_the_whole_read() {
        // Defensive: a malformed block must still yield the leading
        // arguments (the ones that name the program) rather than nothing.
        let mut buf = 3u32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/bin/node\0");
        buf.extend_from_slice(b"node\0");
        buf.extend_from_slice(b"/path/to/pi\0");
        buf.extend_from_slice(b"--flag-cut-off-here");
        assert_eq!(
            parse_procargs2(&buf),
            Some(vec![
                "node".into(),
                "/path/to/pi".into(),
                "--flag-cut-off-here".into()
            ])
        );
    }

    #[test]
    fn a_block_too_short_to_hold_argc_is_rejected() {
        assert_eq!(parse_procargs2(&[1, 2]), None);
        assert_eq!(parse_procargs2(&0u32.to_ne_bytes()), None);
    }
}
