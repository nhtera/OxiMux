//! Which children a restart's recovery turns may start, and when.
//!
//! After OxiMux restarts in the middle of a child's turn, the child is told
//! what was cut short by a recovery turn of its own (its parent is waiting on
//! it; a root instead gets the note on the user's next message). Each recovery
//! turn spawns an agent process and is billed, so a restart that cut twelve
//! children short must not start twelve at once: at most `cap` run together,
//! the rest wait in arrival order.
//!
//! The decision is pure; the host drives it. A recovery turn is sent under a
//! continuation id derived from the child and the restart it recovers from,
//! so a second pass over the same restart (a lock won back, a double boot)
//! finds the id already used and sends nothing.

use std::collections::VecDeque;

use sha2::{Digest, Sha256};

/// Concurrent recovery turns when nothing is configured.
pub const DEFAULT_MAX_RESUMES: u32 = 3;

/// The id a child's recovery from one restart is sent under: the same for
/// every pass over that restart, different for the next restart.
/// `restart_epoch` is when the host that was cut short started (seconds).
pub fn continuation_id(child: &str, restart_epoch: u64) -> String {
    let digest = Sha256::digest(format!("resume\0{child}\0{restart_epoch}").as_bytes());
    format!("resume-{}", hex(&digest[..16]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What [`ResumeGovernor::request`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// Start its recovery turn now.
    Granted,
    /// Wait; `0` is next in line.
    Queued(usize),
    /// Already running or waiting: nothing changes.
    Known,
}

/// The cap and the line behind it.
#[derive(Debug)]
pub struct ResumeGovernor {
    cap: usize,
    running: Vec<String>,
    waiting: VecDeque<String>,
}

impl ResumeGovernor {
    /// A governor letting `cap` recovery turns run together (at least one).
    pub fn new(cap: u32) -> Self {
        Self { cap: cap.max(1) as usize, running: Vec::new(), waiting: VecDeque::new() }
    }

    /// Ask to start `child`'s recovery turn.
    pub fn request(&mut self, child: &str) -> Grant {
        if self.running.iter().chain(&self.waiting).any(|c| c == child) {
            return Grant::Known;
        }
        if self.running.len() < self.cap {
            self.running.push(child.to_string());
            return Grant::Granted;
        }
        self.waiting.push_back(child.to_string());
        Grant::Queued(self.waiting.len() - 1)
    }

    /// `child`'s recovery turn is over (or never started, or it was dropped
    /// from the line). Returns the children granted in its place, in order.
    pub fn release(&mut self, child: &str) -> Vec<String> {
        self.waiting.retain(|c| c != child);
        let before = self.running.len();
        self.running.retain(|c| c != child);
        if self.running.len() == before {
            return Vec::new();
        }
        let mut granted = Vec::new();
        while self.running.len() < self.cap {
            let Some(next) = self.waiting.pop_front() else { break };
            self.running.push(next.clone());
            granted.push(next);
        }
        granted
    }

    /// Children whose recovery turn is running.
    pub fn running(&self) -> &[String] {
        &self.running
    }

    /// Where `child` waits in line, if it does (`0` is next).
    pub fn position(&self, child: &str) -> Option<usize> {
        self.waiting.iter().position(|c| c == child)
    }

    /// Nothing running and nobody waiting.
    pub fn is_idle(&self) -> bool {
        self.running.is_empty() && self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_continuation_id_is_fixed_per_child_and_restart() {
        assert_eq!(continuation_id("c", 7), continuation_id("c", 7));
        assert_ne!(continuation_id("c", 7), continuation_id("c", 8), "the next restart is a new recovery");
        assert_ne!(continuation_id("c", 7), continuation_id("d", 7));
        assert!(continuation_id("c", 7).len() <= 128, "fits an idempotency key");
    }

    #[test]
    fn five_children_three_run_and_two_wait_in_order() {
        let mut g = ResumeGovernor::new(3);
        let got: Vec<Grant> = ["a", "b", "c", "d", "e"].iter().map(|c| g.request(c)).collect();
        assert_eq!(got, [Grant::Granted, Grant::Granted, Grant::Granted, Grant::Queued(0), Grant::Queued(1)]);
        assert_eq!(g.release("b"), ["d"]);
        assert_eq!(g.position("e"), Some(0));
        assert_eq!(g.release("a"), ["e"]);
        assert_eq!(g.release("c"), Vec::<String>::new());
        assert_eq!(g.running(), ["d", "e"]);
    }

    #[test]
    fn asking_twice_changes_nothing() {
        let mut g = ResumeGovernor::new(1);
        assert_eq!(g.request("a"), Grant::Granted);
        assert_eq!(g.request("b"), Grant::Queued(0));
        assert_eq!(g.request("a"), Grant::Known);
        assert_eq!(g.request("b"), Grant::Known);
        assert_eq!(g.running(), ["a"]);
    }

    #[test]
    fn a_waiting_child_dropped_from_the_line_frees_nothing() {
        let mut g = ResumeGovernor::new(1);
        g.request("a");
        g.request("b");
        assert!(g.release("b").is_empty());
        assert_eq!(g.position("b"), None);
        assert!(g.release("unknown").is_empty());
        assert_eq!(g.release("a"), Vec::<String>::new());
        assert!(g.is_idle());
    }

    #[test]
    fn a_zero_cap_still_lets_one_run() {
        let mut g = ResumeGovernor::new(0);
        assert_eq!(g.request("a"), Grant::Granted);
        assert_eq!(g.request("b"), Grant::Queued(0));
    }
}
