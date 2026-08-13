//! Linux process introspection via `/proc`.
//!
//! `/proc/<pid>/task/<pid>/children` gives a process's children directly, but
//! it needs `CONFIG_PROC_CHILDREN` and is absent on some kernels, so a miss
//! falls back to one pass over `/proc/*/stat`.

use std::fs;

pub(crate) fn children_of(pid: u32) -> Vec<u32> {
    if let Some(kids) = children_from_task_file(pid) {
        return kids;
    }
    children_by_scanning_proc(pid)
}

/// The cheap path: one read of a file the kernel renders on demand.
/// `None` when the kernel was built without `CONFIG_PROC_CHILDREN` (the file
/// is missing) — distinct from "read fine, no children", which is `Some`.
fn children_from_task_file(pid: u32) -> Option<Vec<u32>> {
    let raw = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    Some(raw.split_ascii_whitespace().filter_map(|t| t.parse().ok()).collect())
}

/// The portable path: every numeric `/proc` entry whose stat reports `pid` as
/// its parent.
fn children_by_scanning_proc(pid: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&candidate| ppid_of(candidate) == Some(pid))
        .collect()
}

/// Parent pid from `/proc/<pid>/stat`.
///
/// The `comm` field is wrapped in parentheses and may itself contain spaces
/// and parentheses, so the fields after it are found from the *last* `)`
/// rather than by splitting the whole line.
fn ppid_of(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // Fields after comm: state, ppid, …
    after_comm.split_ascii_whitespace().nth(1)?.parse().ok()
}

pub(crate) fn name_of_pid(pid: u32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim_end_matches('\n');
    (!comm.is_empty()).then(|| comm.to_owned())
}

pub(crate) fn argv_of_pid(pid: u32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    // Arguments are nul-separated with a trailing nul; a kernel thread has an
    // empty cmdline, which is a miss rather than an empty argument vector.
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}
