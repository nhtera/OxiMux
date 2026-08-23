//! Fallback for platforms with no socket-table path wired yet.
//!
//! Reporting "nothing listening" degrades cleanly: the panel shows its empty
//! state, which is the same thing it shows on a supported platform where
//! nothing happens to be listening. There is no third answer to invent.

use crate::ListeningPort;

pub(crate) fn listening_ports_of(_pids: &[u32]) -> Vec<ListeningPort> {
    Vec::new()
}
