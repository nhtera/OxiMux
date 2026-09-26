//! The simulator's commands — toolbar buttons, captured-keyboard shortcuts
//! and palette entries all end here, through the `Sim*` actions the window
//! root handles (so a shortcut, a click and the palette are one path).
//!
//! [`SimulatorPanel::run_command`] does what it can and tells the root what
//! is left: open a terminal tab, or explain why nothing happened.

use std::path::PathBuf;

use gpui::{Action, Context, Window};

use super::SimulatorPanel;
use crate::actions::{
    SimAnnotate, SimBack, SimDetach, SimHome, SimLock, SimOpenLogs, SimRecents, SimRotateCcw, SimRotateCw,
    SimScreenshot, SimShutdown, SimToggleKeyboard, SimToggleRecord,
};
use oximux_simulator::Platform;

use crate::shell::simulator::hub::is_udid;
use crate::shell::simulator::state::PanelState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimCommand {
    Home,
    Lock,
    /// Android only.
    Back,
    /// Android only: the app switcher.
    Recents,
    RotateCw,
    RotateCcw,
    Screenshot,
    ToggleRecord,
    Annotate,
    ToggleKeyboard,
    OpenLogs,
    Shutdown,
    Detach,
}

impl SimCommand {
    /// The action a toolbar button dispatches for this command.
    pub(crate) fn action(self) -> Box<dyn Action> {
        match self {
            Self::Home => Box::new(SimHome),
            Self::Lock => Box::new(SimLock),
            Self::Back => Box::new(SimBack),
            Self::Recents => Box::new(SimRecents),
            Self::RotateCw => Box::new(SimRotateCw),
            Self::RotateCcw => Box::new(SimRotateCcw),
            Self::Screenshot => Box::new(SimScreenshot),
            Self::ToggleRecord => Box::new(SimToggleRecord),
            Self::Annotate => Box::new(SimAnnotate),
            Self::ToggleKeyboard => Box::new(SimToggleKeyboard),
            Self::OpenLogs => Box::new(SimOpenLogs),
            Self::Shutdown => Box::new(SimShutdown),
            Self::Detach => Box::new(SimDetach),
        }
    }

    /// Needs the live stream (the helper), not just a booted device.
    fn needs_stream(self) -> bool {
        matches!(self, Self::Home | Self::Lock | Self::Back | Self::Recents | Self::RotateCw | Self::RotateCcw | Self::Annotate | Self::ToggleKeyboard)
    }
}

/// What the root does after [`SimulatorPanel::run_command`].
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Done,
    /// No device is attached to the active worktree.
    NoDevice,
    /// The command needs the live stream, which is not running (hidden,
    /// parked, starting): show the panel, which brings it up.
    NotStreaming,
    /// Stream the device log in a terminal tab.
    OpenLogs { cwd: PathBuf, title: String, script: String },
}

/// The device log, compact, at info level.
pub(crate) fn log_script(udid: &str) -> String {
    format!("xcrun simctl spawn {udid} log stream --level info --style compact")
}

impl SimulatorPanel {
    pub fn run_command(&mut self, command: SimCommand, window: &mut Window, cx: &mut Context<Self>) -> Outcome {
        let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) else { return Outcome::NoDevice };
        if command == SimCommand::Detach {
            self.detach(cx);
            return Outcome::Done;
        }
        if command.needs_stream() && !matches!(self.state(cx), PanelState::Streaming) {
            return Outcome::NotStreaming;
        }
        let sent = match command {
            SimCommand::Home => hub.read(cx).home(&udid),
            SimCommand::Lock => hub.read(cx).lock(&udid),
            SimCommand::Back => hub.read(cx).back(&udid),
            SimCommand::Recents => hub.read(cx).recents(&udid),
            SimCommand::RotateCw | SimCommand::RotateCcw => {
                hub.update(cx, |hub, cx| hub.rotate(&udid, command == SimCommand::RotateCw, cx))
            }
            SimCommand::Screenshot => {
                hub.update(cx, |hub, cx| hub.screenshot(&udid, cx));
                true
            }
            SimCommand::ToggleRecord => {
                hub.update(cx, |hub, cx| hub.toggle_recording(&udid, cx));
                true
            }
            SimCommand::Annotate => {
                self.start_annotating(window, cx);
                true
            }
            SimCommand::ToggleKeyboard => {
                if let Some(screen) = self.screen.clone() {
                    screen.update(cx, |screen, cx| screen.toggle_capture(window, cx));
                }
                true
            }
            SimCommand::OpenLogs => {
                let Some(cwd) = self.worktree.clone() else { return Outcome::NoDevice };
                let title = format!("{} log", self.device_name(cx));
                let script = match udid.platform() {
                    // The id lands in a shell command line: only a well-formed one.
                    Platform::Ios if is_udid(udid.as_str()) => log_script(udid.as_str()),
                    Platform::Ios => return Outcome::NoDevice,
                    Platform::Android => match hub.read(cx).logcat_script(&udid) {
                        Some(script) => script,
                        None => return Outcome::NotStreaming,
                    },
                };
                return Outcome::OpenLogs { cwd, title, script };
            }
            SimCommand::Shutdown => {
                self.confirm_shutdown = true;
                cx.notify();
                true
            }
            SimCommand::Detach => unreachable!("handled above"),
        };
        if sent { Outcome::Done } else { Outcome::NotStreaming }
    }

    /// The confirmation's answer.
    pub(super) fn answer_shutdown(&mut self, shut_down: bool, cx: &mut Context<Self>) {
        self.confirm_shutdown = false;
        if shut_down && let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) {
            // The movie is finalized before the device goes away, and agents
            // may not boot it again behind the user's back.
            hub.update(cx, |hub, cx| {
                hub.note_stopped_by_user(&udid);
                hub.shutdown_after_recording(&udid, cx);
            });
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_script_streams_the_device_log() {
        assert_eq!(log_script("ABCD"), "xcrun simctl spawn ABCD log stream --level info --style compact");
    }

    #[test]
    fn only_helper_commands_need_the_stream() {
        assert!(SimCommand::Home.needs_stream() && SimCommand::RotateCcw.needs_stream());
        for simctl_only in [SimCommand::Screenshot, SimCommand::ToggleRecord, SimCommand::OpenLogs, SimCommand::Shutdown] {
            assert!(!simctl_only.needs_stream(), "{simctl_only:?} works on a parked device");
        }
    }
}
