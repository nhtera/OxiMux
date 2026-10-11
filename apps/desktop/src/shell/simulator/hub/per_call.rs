//! Questions asked every time an agent wants something done to a device,
//! whatever was allowed before: approval to control a phone is not approval
//! to put apps on it.
//!
//! Each belongs to the worktree whose agent asked, like a consent request: it
//! shows in that worktree's panel and nowhere else, and is dropped when the
//! worktree detaches or changes device.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_simulator::DeviceId;
use oximux_simulator::registry::WorktreeKey;

use super::{HubEvent, SimulatorHub};

/// How long an "Install … on this phone?" question waits for the user.
pub(crate) const INSTALL_ASK_TTL: Duration = Duration::from_secs(90);

/// An agent's install on a real device, waiting for the user's yes. Asked
/// every time: approval to control a phone is not approval to put apps on it.
struct InstallAsk {
    id: u64,
    udid: DeviceId,
    worktree: WorktreeKey,
    device_name: String,
    app: String,
    asked: Instant,
    answer: Option<bool>,
}

/// Where an install question stands, for the waiting agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallAnswer {
    Pending,
    Allowed,
    Refused,
    /// Expired or dropped (the worktree moved on): not allowed.
    Gone,
}

/// The open questions (see [`InstallAsk`]).
#[derive(Default)]
pub(crate) struct PerCall {
    installs: Vec<InstallAsk>,
    next_install: u64,
}

impl PerCall {
    /// Drop `worktree`'s questions. Returns whether any went.
    pub(super) fn forget_worktree(&mut self, worktree: &WorktreeKey) -> bool {
        let asks = self.installs.len();
        self.installs.retain(|a| a.worktree != *worktree);
        self.installs.len() != asks
    }

    /// Drop questions nobody answered in time — or whose answer no agent
    /// came back for (its CLI gave up). Returns whether any went.
    pub(super) fn expire(&mut self) -> bool {
        let asks = self.installs.len();
        // An answer gets a little longer than the question: the waiting agent
        // reads it within a quarter second.
        let grace = |a: &InstallAsk| if a.answer.is_some() { Duration::from_secs(15) } else { Duration::ZERO };
        self.installs.retain(|a| a.asked.elapsed() < INSTALL_ASK_TTL + grace(a));
        self.installs.len() != asks
    }

    /// Every open question, asked `by` earlier than it was.
    #[cfg(test)]
    pub(super) fn age_for_test(&mut self, by: Duration) {
        for ask in &mut self.installs {
            ask.asked -= by;
        }
    }
}

impl SimulatorHub {
    /// Ask the user whether an agent may install `app` on the real device
    /// `udid`. Returns the question's number, for [`Self::install_answer`].
    pub(crate) fn ask_install(&mut self, udid: &DeviceId, worktree: &Path, device_name: &str, app: &str, cx: &mut Context<Self>) -> u64 {
        let per_call = &mut self.agent.per_call;
        per_call.next_install += 1;
        let id = per_call.next_install;
        per_call.installs.push(InstallAsk {
            id,
            udid: udid.clone(),
            worktree: WorktreeKey::from_path(worktree),
            device_name: device_name.to_owned(),
            app: app.to_owned(),
            asked: Instant::now(),
            answer: None,
        });
        cx.emit(HubEvent::Consent);
        id
    }

    /// The user's answer to install question `id`, once (an answered or
    /// expired question is dropped as it is read).
    pub(crate) fn install_answer(&mut self, id: u64) -> InstallAnswer {
        let installs = &mut self.agent.per_call.installs;
        let Some(at) = installs.iter().position(|a| a.id == id) else { return InstallAnswer::Gone };
        let answer = match installs[at].answer {
            Some(true) => InstallAnswer::Allowed,
            Some(false) => InstallAnswer::Refused,
            None if installs[at].asked.elapsed() >= INSTALL_ASK_TTL => InstallAnswer::Gone,
            None => return InstallAnswer::Pending,
        };
        installs.remove(at);
        answer
    }

    /// The install question `worktree`'s agent is waiting on about the device
    /// attached there now: its number, the device's name and the app's.
    pub fn install_request(&self, worktree: &Path) -> Option<(u64, String, String)> {
        let (udid, key) = (self.device_for(worktree)?, WorktreeKey::from_path(worktree));
        let ask = self.agent.per_call.installs.iter().find(|a| a.worktree == key && a.udid == udid && a.answer.is_none())?;
        Some((ask.id, ask.device_name.clone(), ask.app.clone()))
    }

    /// The banner's Install / Don't install.
    pub fn answer_install(&mut self, id: u64, allow: bool, cx: &mut Context<Self>) {
        if let Some(ask) = self.agent.per_call.installs.iter_mut().find(|a| a.id == id) {
            ask.answer = Some(allow);
            cx.emit(HubEvent::Consent);
        }
    }
}
