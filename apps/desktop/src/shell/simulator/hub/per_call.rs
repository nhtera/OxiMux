//! Questions asked every time an agent wants something done to a device,
//! whatever was allowed before: approval to control a phone is not approval
//! to put apps on it, and approval to drive a simulator is not approval to
//! wipe it. Installing on a real device, erasing, a push notification, and
//! every change to a real device's state ask here.
//!
//! Each belongs to the worktree whose agent asked, like a consent request: it
//! shows in that worktree's panel and nowhere else, and is dropped when the
//! worktree detaches or changes device.

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_remote_host::SimPrincipal;
use oximux_simulator::DeviceId;
use oximux_simulator::registry::WorktreeKey;

use super::{HubEvent, SimulatorHub};

/// How long a question waits for the user.
pub(crate) const ASK_TTL: Duration = Duration::from_secs(90);

/// What an agent asks to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AskKind {
    /// Put an app (named by its file) on a real device.
    Install { app: String },
    Erase,
    /// Deliver a push notification to an app.
    Push { app: String },
    /// Change a real device's state, in words ("switch to dark mode").
    PhysicalState { what: String },
    /// Run a file of steps, these of which change the device's state.
    FlowRun { lines: Vec<String> },
    /// Run this raw `simctl`/`adb` command line (`sim exec`), shown whole;
    /// `real_device`: on someone's own phone.
    Exec { command: String, real_device: bool },
}

/// How many of a run's steps the question lists.
const FLOW_LINES_SHOWN: usize = 8;

impl AskKind {
    /// What the agent wants, for the toast ("An agent in … wants to …").
    pub(crate) fn what(&self, device: &str) -> String {
        match self {
            Self::Install { app } => format!("install {app} on {device}"),
            Self::Erase => format!("erase {device}"),
            Self::Push { app } => format!("send a notification to {app} on {device}"),
            Self::PhysicalState { what } => format!("{what} on {device}"),
            Self::FlowRun { lines } if lines.len() == 1 => format!("run 1 step that changes {device}"),
            Self::FlowRun { lines } => format!("run {} steps that change {device}", lines.len()),
            Self::Exec { .. } => format!("run a raw command on {device}"),
        }
    }

    /// [`Self::question`], worded for whoever asked: an outside MCP client is
    /// named as one, never as "an agent".
    pub(crate) fn question_from(&self, device: &str, principal: SimPrincipal) -> (String, String, (&'static str, &'static str)) {
        let (title, body, answers) = self.question(device);
        match principal {
            SimPrincipal::Agent => (title, body, answers),
            SimPrincipal::Mcp => (
                title.replace("Let an agent ", "Let an outside MCP client "),
                body.replace("An agent in this worktree", "An outside MCP client (connected with `oximux mcp`)"),
                answers,
            ),
        }
    }

    /// The banner: its title, why it is asked, and its two answers.
    pub(crate) fn question(&self, device: &str) -> (String, String, (&'static str, &'static str)) {
        let (title, body, answers) = match self {
            Self::Install { app } => (
                format!("Install {app} on {device}?"),
                "An agent in this worktree wants to install this app on your real device. Allow it only if you expected this build.",
                ("Don't install", "Install"),
            ),
            Self::Erase => (
                format!("Erase {device}?"),
                "An agent in this worktree wants to erase this device: every app and its data are deleted, and it starts again from factory settings.",
                ("Don't erase", "Erase"),
            ),
            Self::Push { app } => (
                format!("Send a notification to {app}?"),
                "An agent in this worktree wants to deliver a push notification to this app on the device.",
                ("Don't send", "Send"),
            ),
            Self::PhysicalState { what } => (
                format!("Let an agent {what} on {device}?"),
                "An agent in this worktree wants to change a setting on your real device. Asked every time.",
                ("Don't allow", "Allow"),
            ),
            Self::FlowRun { lines } => {
                let mut body = "An agent in this worktree wants to run a flow with these steps, which change the device:".to_owned();
                for line in lines.iter().take(FLOW_LINES_SHOWN) {
                    // One line each, whatever a client sent.
                    let line: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
                    body.push_str(&format!("\n• {}", crate::shell::simulator::agent_ops::clamp(&line, 120)));
                }
                if lines.len() > FLOW_LINES_SHOWN {
                    body.push_str(&format!("\n…and {} more", lines.len() - FLOW_LINES_SHOWN));
                }
                body.push_str("\nErase and notifications still ask when their turn comes.");
                return (format!("Run this flow on {device}?"), body, ("Don't run", "Run"));
            }
            Self::Exec { command, real_device } => {
                let phone = if *real_device { " This is a real phone: the command can read and change everything on it." } else { "" };
                let body = format!(
                    "An agent in this worktree wants to run this command, outside its sandbox. Asked every time.{phone}\n{command}"
                );
                return (format!("Run a command on {device}?"), body, ("Don't run", "Run"));
            }
        };
        (title, body.to_owned(), answers)
    }
}

/// An agent's question, waiting for the user's yes.
struct Ask {
    id: u64,
    /// Who asked: the banner names them.
    principal: SimPrincipal,
    udid: DeviceId,
    worktree: WorktreeKey,
    device_name: String,
    kind: AskKind,
    asked: Instant,
    answer: Option<bool>,
}

/// Where a question stands, for the waiting agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PerCallAnswer {
    Pending,
    Allowed,
    Refused,
    /// Expired or dropped (the worktree moved on): not allowed.
    Gone,
}

/// The open questions (see [`Ask`]).
#[derive(Default)]
pub(crate) struct PerCall {
    asks: Vec<Ask>,
    next: u64,
}

impl PerCall {
    /// Drop `worktree`'s questions. Returns whether any went.
    pub(super) fn forget_worktree(&mut self, worktree: &WorktreeKey) -> bool {
        let asks = self.asks.len();
        self.asks.retain(|a| a.worktree != *worktree);
        self.asks.len() != asks
    }

    /// Drop questions nobody answered in time — or whose answer no agent
    /// came back for (its CLI gave up). Returns whether any went.
    pub(super) fn expire(&mut self) -> bool {
        let asks = self.asks.len();
        // An answer gets a little longer than the question: the waiting agent
        // reads it within a quarter second.
        let grace = |a: &Ask| if a.answer.is_some() { Duration::from_secs(15) } else { Duration::ZERO };
        self.asks.retain(|a| a.asked.elapsed() < ASK_TTL + grace(a));
        self.asks.len() != asks
    }

    /// Every open question, asked `by` earlier than it was.
    #[cfg(test)]
    pub(super) fn age_for_test(&mut self, by: Duration) {
        for ask in &mut self.asks {
            ask.asked -= by;
        }
    }
}

impl SimulatorHub {
    /// Ask the user, in `worktree`'s panel, whether an agent may do `kind` to
    /// `udid`. Returns the question's number, for [`Self::per_call_answer`].
    pub(crate) fn ask_per_call(
        &mut self,
        principal: SimPrincipal,
        udid: &DeviceId,
        worktree: &Path,
        device_name: &str,
        kind: AskKind,
        cx: &mut Context<Self>,
    ) -> u64 {
        let per_call = &mut self.agent.per_call;
        per_call.next += 1;
        let id = per_call.next;
        per_call.asks.push(Ask {
            id,
            principal,
            udid: udid.clone(),
            worktree: WorktreeKey::from_path(worktree),
            device_name: device_name.to_owned(),
            kind,
            asked: Instant::now(),
            answer: None,
        });
        cx.emit(HubEvent::Consent);
        id
    }

    /// The user's answer to question `id`, once (an answered or expired
    /// question is dropped as it is read).
    pub(crate) fn per_call_answer(&mut self, id: u64) -> PerCallAnswer {
        let asks = &mut self.agent.per_call.asks;
        let Some(at) = asks.iter().position(|a| a.id == id) else { return PerCallAnswer::Gone };
        let answer = match asks[at].answer {
            Some(true) => PerCallAnswer::Allowed,
            Some(false) => PerCallAnswer::Refused,
            None if asks[at].asked.elapsed() >= ASK_TTL => PerCallAnswer::Gone,
            None => return PerCallAnswer::Pending,
        };
        asks.remove(at);
        answer
    }

    /// The question `worktree`'s agent is waiting on about the device
    /// attached there now: its number, the device's name, and what it asks.
    pub(crate) fn per_call_request(&self, worktree: &Path) -> Option<(u64, String, AskKind, SimPrincipal)> {
        let (udid, key) = (self.device_for(worktree)?, WorktreeKey::from_path(worktree));
        let ask = self.agent.per_call.asks.iter().find(|a| a.worktree == key && a.udid == udid && a.answer.is_none())?;
        Some((ask.id, ask.device_name.clone(), ask.kind.clone(), ask.principal))
    }

    /// The banner's two answers.
    pub(crate) fn answer_per_call(&mut self, id: u64, allow: bool, cx: &mut Context<Self>) {
        if let Some(ask) = self.agent.per_call.asks.iter_mut().find(|a| a.id == id) {
            ask.answer = Some(allow);
            cx.emit(HubEvent::Consent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_question_says_what_it_asks() {
        assert_eq!(AskKind::Erase.what("iPhone 17"), "erase iPhone 17");
        let (title, body, (deny, allow)) = AskKind::Erase.question("iPhone 17");
        assert_eq!((title.as_str(), deny, allow), ("Erase iPhone 17?", "Don't erase", "Erase"));
        assert!(body.contains("factory settings"));
        let dark = AskKind::PhysicalState { what: "switch to dark mode".into() };
        assert_eq!(dark.question("Pixel 8").0, "Let an agent switch to dark mode on Pixel 8?");
        assert_eq!(AskKind::Install { app: "a.apk".into() }.question("Pixel 8").0, "Install a.apk on Pixel 8?");
        let lines: Vec<String> = (0..10).map(|i| format!("privacy grant camera --app com.a{i}")).collect();
        let (title, body, (_, allow)) = AskKind::FlowRun { lines }.question("iPhone 17");
        assert_eq!((title.as_str(), allow), ("Run this flow on iPhone 17?", "Run"));
        assert_eq!(AskKind::FlowRun { lines: vec!["erase".into()] }.what("iPhone 17"), "run 1 step that changes iPhone 17");
        assert!(body.contains("• privacy grant camera --app com.a0") && body.contains("…and 2 more"), "{body}");
        let exec = AskKind::Exec { command: "xcrun simctl ui U appearance dark".into(), real_device: false };
        let (title, body, (deny, allow)) = exec.question("iPhone 17");
        assert_eq!((title.as_str(), deny, allow), ("Run a command on iPhone 17?", "Don't run", "Run"));
        assert!(body.ends_with("\nxcrun simctl ui U appearance dark") && body.contains("outside its sandbox"), "{body}");
        assert!(!body.contains("real phone"));
        let phone = AskKind::Exec { command: "adb -s R58 shell ls".into(), real_device: true };
        assert!(phone.question("Pixel 8").1.contains("This is a real phone"));
        for kind in [AskKind::Install { app: "a.apk".into() }, AskKind::PhysicalState { what: "switch to dark mode".into() }, AskKind::Erase] {
            let (title, body, _) = kind.question_from("Pixel 8", SimPrincipal::Mcp);
            assert!(!title.contains("an agent") && !body.contains("An agent"), "{title} / {body}");
            assert!(body.contains("outside MCP client"), "{body}");
        }
    }
}
