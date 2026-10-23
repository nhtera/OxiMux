//! Ask a full-screen program to repaint for a newly attached client.
//!
//! A program in the alternate screen redraws its whole frame on `SIGWINCH`,
//! and that frame is exactly what the replay cannot carry once its entry has
//! left the ring. So an attach to such a PTY signals its foreground process
//! group — after checking the group really belongs to the session this PTY
//! spawned (the child `setsid()`s, so its pid is the session id), and never
//! more than once per [`MIN_INTERVAL`] per PTY, however many clients attach.
//! A gap resync never nudges.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use portable_pty::MasterPty;

/// One nudge per PTY per this window.
const MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Signal the PTY's foreground group to repaint, if it is safe and not too soon.
pub(crate) fn nudge(master: &Mutex<Box<dyn MasterPty + Send>>, child: Option<u32>, last: &Mutex<Option<Instant>>) {
    let now = Instant::now();
    {
        let mut last = last.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| now.duration_since(t) < MIN_INTERVAL) {
            return;
        }
        *last = Some(now);
    }
    #[cfg(unix)]
    {
        let pgid = master.lock().unwrap_or_else(|p| p.into_inner()).process_group_leader();
        if let Some(pgid) = pgid.filter(|&g| foreground_is_ours(g, child)) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pgid), nix::sys::signal::Signal::SIGWINCH);
        }
    }
    #[cfg(not(unix))]
    let _ = (master, child);
}

/// The foreground group may be signalled: a real group (not 0, not init's),
/// not the daemon's own, and in the session the PTY's child leads.
#[cfg(unix)]
fn foreground_is_ours(pgid: i32, child: Option<u32>) -> bool {
    use nix::unistd::{Pid, getpgrp, getsid};
    let Some(child) = child.and_then(|c| i32::try_from(c).ok()) else {
        return false;
    };
    pgid > 1
        && Pid::from_raw(pgid) != getpgrp()
        && getsid(Some(Pid::from_raw(pgid))).is_ok_and(|sid| sid == Pid::from_raw(child))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn only_a_group_in_the_childs_own_session_is_signalled() {
        // Our own group is never a target, nor 0/1, nor a missing child.
        assert!(!foreground_is_ours(getpgrp_raw(), Some(std::process::id())));
        assert!(!foreground_is_ours(1, Some(1)));
        assert!(!foreground_is_ours(0, Some(1)));
        assert!(!foreground_is_ours(12345, None));
    }

    fn getpgrp_raw() -> i32 {
        nix::unistd::getpgrp().as_raw()
    }
}
