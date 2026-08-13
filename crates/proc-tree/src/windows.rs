//! Windows process introspection via a Toolhelp process snapshot.
//!
//! Windows has no "children of this pid" query — the parent pid is a field on
//! every row of a whole-table snapshot. Taking one snapshot per generation of
//! a walk would mean re-reading the entire table a dozen times, so the table
//! is cached per thread for [`SNAPSHOT_TTL`]: one walk pays for one snapshot,
//! and several terminals scanning in the same frame share it.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// How long a cached process table stays authoritative. Long enough that one
/// walk (and the other terminals scanning in the same frame) reuse it, short
/// enough that an agent starting or exiting shows up on the next poll rather
/// than a poll later.
const SNAPSHOT_TTL: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct Row {
    pid: u32,
    ppid: u32,
    name: String,
}

thread_local! {
    static TABLE: RefCell<Option<(Instant, Vec<Row>)>> = const { RefCell::new(None) };
}

pub(crate) fn children_of(pid: u32) -> Vec<u32> {
    with_table(|rows| {
        rows.iter()
            // The idle process reports itself as its own parent; without this
            // guard a walk rooted there would never terminate.
            .filter(|r| r.ppid == pid && r.pid != pid)
            .map(|r| r.pid)
            .collect()
    })
}

pub(crate) fn name_of_pid(pid: u32) -> Option<String> {
    with_table(|rows| {
        rows.iter()
            .find(|r| r.pid == pid)
            .map(|r| r.name.clone())
    })
}

/// Reading another process's argument vector means reaching into its address
/// space through `NtQueryInformationProcess`, which needs `PROCESS_VM_READ`
/// and a bitness-matched `PEB` walk. Callers fall back to matching
/// [`crate::ProcInfo::name`] — weaker in general, but sound here: Windows has
/// no symlinked-launcher convention, so `szExeFile` is the true image name.
pub(crate) fn argv_of_pid(_pid: u32) -> Option<Vec<String>> {
    None
}

fn with_table<T>(f: impl FnOnce(&[Row]) -> T) -> T {
    TABLE.with(|cell| {
        let mut cell = cell.borrow_mut();
        let fresh = cell
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < SNAPSHOT_TTL);
        if !fresh {
            *cell = Some((Instant::now(), snapshot()));
        }
        let (_, rows) = cell.as_ref().expect("table was just populated");
        f(rows)
    })
}

fn snapshot() -> Vec<Row> {
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // Process32FirstW fails when the snapshot is empty, which is not an error
    // worth distinguishing from "nothing matched".
    if unsafe { Process32FirstW(handle, &mut entry) } != 0 {
        loop {
            rows.push(Row {
                pid: entry.th32ProcessID,
                ppid: entry.th32ParentProcessID,
                name: exe_name(&entry.szExeFile),
            });
            if unsafe { Process32NextW(handle, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(handle) };
    rows
}

/// `szExeFile` is a fixed-width, nul-padded UTF-16 buffer; decode up to the
/// first nul. Windows records the executable with its extension (`node.exe`),
/// which the caller's registry matching tolerates.
fn exe_name(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}
