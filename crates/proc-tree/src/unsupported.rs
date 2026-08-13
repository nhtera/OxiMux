//! Fallback for platforms with no process-introspection path wired yet.
//!
//! Reporting "no descendants" degrades cleanly: the caller shows whatever the
//! window title and the status sideband reveal, exactly as it did before this
//! crate existed.

pub(crate) fn children_of(_pid: u32) -> Vec<u32> {
    Vec::new()
}

pub(crate) fn name_of_pid(_pid: u32) -> Option<String> {
    None
}

pub(crate) fn argv_of_pid(_pid: u32) -> Option<Vec<String>> {
    None
}
