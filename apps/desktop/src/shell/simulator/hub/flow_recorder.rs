//! Flow recordings: the steps that redo what was done on a device, as a
//! flow file `oximux sim run` replays.
//!
//! Two sources, never mixed: an agent's recording (started, read and stopped
//! by `oximux sim flow …`) holds the verbs that agent sent; the panel's
//! (the Device menu's Record flow) holds the user's own input, and no verb
//! can start, read or stop it. Each belongs to a worktree's device and ends
//! when the worktree detaches or changes device. Process lifetime only:
//! saving is an explicit stop.

use std::collections::HashMap;
use std::path::PathBuf;

use oximux_remote_proto::simulator::{
    SimButtonWire, SimCmdWire, SimFlowWire, SimOrientationWire, SimPointWire, SimSelectorWire, SimTargetWire, flow, render_step,
};
use oximux_simulator::DeviceId;
use oximux_simulator::registry::WorktreeKey;

use super::SimulatorHub;

/// Whose input a recording holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RecSource {
    Agent,
    Panel,
}

pub(crate) type RecKey = (WorktreeKey, DeviceId, RecSource);

/// One piece of the user's input on the panel, as a recording takes it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PanelStep {
    /// A tap at `at` (points), on the element `target` names exactly when
    /// one was read in time (`secure`: a password field).
    Tap { at: (f64, f64), target: Option<(SimSelectorWire, bool)> },
    Swipe { from: (f64, f64), to: (f64, f64), duration_ms: u32 },
    /// Typed characters.
    Text(String),
    Backspace,
    Button(SimButtonWire),
    Rotate(SimOrientationWire),
}

/// Typing into one field, kept as one step while it goes on.
#[derive(Debug, Default)]
struct Typing {
    text: String,
    /// Typed into a password field: the step holds a placeholder.
    secret: bool,
}

/// One recording in progress.
#[derive(Debug, Default)]
pub(crate) struct Recording {
    platform: &'static str,
    /// The screen in points when it started (what a point step is scaled by
    /// on export).
    screen: Option<(f64, f64)>,
    app: Option<String>,
    steps: Vec<String>,
    warnings: Vec<String>,
    /// How many secrets were typed (each a `${SECRET_N}`).
    secrets: u32,
    /// Typing in progress (the last step is its line).
    typing: Option<Typing>,
    /// The last tap landed on a password field.
    secure_field: bool,
}

impl Recording {
    /// The flow file so far.
    pub(crate) fn text(&self) -> String {
        let mut out = flow::header(self.platform, self.screen, self.app.as_deref());
        for step in &self.steps {
            out.push_str(step);
            out.push('\n');
        }
        out
    }

    pub(crate) fn count(&self) -> u32 {
        u32::try_from(self.steps.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn wire(&self, recording: bool, saved: Option<String>) -> SimFlowWire {
        SimFlowWire { recording, steps: self.text(), count: self.count(), warnings: self.warnings.clone(), saved }
    }

    pub(crate) fn push(&mut self, lines: Vec<String>) {
        // The app a flow drives: the first one it launches.
        if self.app.is_none()
            && let Some(app) = lines.iter().find_map(|l| l.strip_prefix("launch ").and_then(|rest| rest.split(' ').next()))
        {
            self.app = Some(app.trim_matches('"').to_owned());
        }
        self.typing = None;
        self.steps.extend(lines);
    }

    /// The user's input on the panel, settled after each step (the next one
    /// acts on what it left).
    pub(crate) fn panel(&mut self, step: PanelStep) {
        let settled = |cmd| SimCmdWire::Settled { action: Box::new(cmd), timeout_ms: 0 };
        let point = |(x, y): (f64, f64)| SimPointWire { x, y };
        let cmd = match step {
            PanelStep::Tap { at, target } => {
                self.secure_field = target.as_ref().is_some_and(|(_, secure)| *secure);
                let aim = match target {
                    Some((sel, _)) => SimTargetWire::Exact(sel),
                    None => {
                        self.warn("a tap was recorded as a point (no element read in time): it may miss on another device".into());
                        SimTargetWire::Point(point(at))
                    }
                };
                settled(SimCmdWire::Tap(aim))
            }
            PanelStep::Swipe { from, to, duration_ms } => settled(SimCmdWire::Swipe { from: point(from), to: point(to), duration_ms }),
            PanelStep::Button(b) => settled(SimCmdWire::Button(b)),
            PanelStep::Rotate(o) => settled(SimCmdWire::Rotate(o)),
            PanelStep::Text(text) => return self.typed(Some(&text)),
            PanelStep::Backspace => return self.typed(None),
        };
        self.add(&cmd);
    }

    /// `text` typed (`None`: a backspace), into the field the last tap hit.
    fn typed(&mut self, text: Option<&str>) {
        if self.typing.is_none() {
            let secret = self.secure_field;
            if secret {
                let name = self.next_secret();
                self.warn(format!("typing into a password field was recorded as {name}: set it in the environment to replay"));
                let typing = Typing { text: name, secret };
                self.add(&SimCmdWire::Settled { action: Box::new(SimCmdWire::Type { text: typing.text.clone(), paste: false }), timeout_ms: 0 });
                self.typing = Some(typing);
                return;
            }
            if text.is_none() {
                // A backspace with nothing typed here: it edits text from
                // before the recording.
                self.warn("a backspace in text typed before recording was not recorded".into());
                return;
            }
            self.steps.push(String::new());
            self.typing = Some(Typing::default());
        }
        let Some(typing) = self.typing.as_mut() else { return };
        if typing.secret {
            return;
        }
        match text {
            Some(text) => typing.text.push_str(text),
            None => {
                typing.text.pop();
            }
        }
        let cmd = SimCmdWire::Settled { action: Box::new(SimCmdWire::Type { text: typing.text.clone(), paste: false }), timeout_ms: 0 };
        let line = render_step(&cmd).ok().and_then(|mut l| l.pop());
        let empty = typing.text.is_empty();
        match (line, self.steps.last_mut()) {
            (Some(line), Some(last)) if !empty => *last = line,
            (_, Some(_)) => {
                self.steps.pop();
                self.typing = None;
            }
            _ => {}
        }
    }

    /// `cmd`'s steps, ending any typing.
    fn add(&mut self, cmd: &SimCmdWire) {
        match render_step(cmd) {
            Ok(lines) => self.push(lines),
            Err(why) => self.warn(format!("not recorded: {why}")),
        }
    }

    pub(crate) fn warn(&mut self, warning: String) {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }

    /// The next secret's placeholder: `${SECRET_1}`, `${SECRET_2}`, …
    pub(crate) fn next_secret(&mut self) -> String {
        self.secrets += 1;
        format!("${{SECRET_{}}}", self.secrets)
    }
}

/// Every recording in progress.
#[derive(Debug, Default)]
pub(crate) struct FlowRecorder {
    recordings: HashMap<RecKey, Recording>,
}

impl FlowRecorder {
    /// Start recording under `key`; false when one already runs.
    pub(crate) fn start(&mut self, key: RecKey, platform: &'static str, screen: Option<(f64, f64)>) -> bool {
        if self.recordings.contains_key(&key) {
            return false;
        }
        self.recordings.insert(key, Recording { platform, screen, ..Recording::default() });
        true
    }

    pub(crate) fn get(&self, key: &RecKey) -> Option<&Recording> {
        self.recordings.get(key)
    }

    pub(crate) fn get_mut(&mut self, key: &RecKey) -> Option<&mut Recording> {
        self.recordings.get_mut(key)
    }

    pub(crate) fn stop(&mut self, key: &RecKey) -> Option<Recording> {
        self.recordings.remove(key)
    }

    /// Drop `worktree`'s recordings (it detached or changed device).
    /// Returns whether any went.
    pub(crate) fn forget_worktree(&mut self, worktree: &WorktreeKey) -> bool {
        let before = self.recordings.len();
        self.recordings.retain(|(w, _, _), _| w != worktree);
        self.recordings.len() != before
    }
}

impl SimulatorHub {
    pub(crate) fn flows(&self) -> &FlowRecorder {
        &self.flows
    }

    pub(crate) fn flows_mut(&mut self) -> &mut FlowRecorder {
        &mut self.flows
    }

    /// A worktree's recordings end when it detaches or changes device.
    pub(super) fn forget_flows(&mut self, worktree: &WorktreeKey, cx: &mut gpui::Context<Self>) {
        if self.flows.forget_worktree(worktree) {
            cx.emit(super::HubEvent::Flow);
        }
    }
}

/// Where a stopped recording is kept when no file was named: the app's data,
/// one folder per worktree, the file the owner's alone. Returns its path.
pub(crate) fn save(worktree: &WorktreeKey, text: &str) -> Result<PathBuf, String> {
    let root = crate::app_paths::data_dir().ok_or("no app data folder")?;
    let dir = root.join("simulator").join("flows").join(hash(worktree));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    for attempt in 0..100 {
        let suffix = if attempt == 0 { String::new() } else { format!("-{attempt}") };
        let path = dir.join(format!("flow-{stamp}{suffix}.flow"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        match options.open(&path) {
            Ok(mut file) => {
                std::io::Write::write_all(&mut file, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot write {}: {e}", path.display())),
        }
    }
    Err(format!("no free flow name in {}", dir.display()))
}

/// A worktree's folder name: its path hashed (FNV-1a), so no path leaks
/// into a folder name.
fn hash(worktree: &WorktreeKey) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in worktree.path().to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: RecSource) -> RecKey {
        (WorktreeKey::from_path(std::path::Path::new("/w")), DeviceId("U".into()), source)
    }

    #[test]
    fn agent_and_panel_recordings_are_apart() {
        let mut flows = FlowRecorder::default();
        assert!(flows.start(key(RecSource::Agent), "ios", Some((402.0, 874.0))));
        assert!(!flows.start(key(RecSource::Agent), "ios", None), "one at a time");
        assert!(flows.start(key(RecSource::Panel), "ios", None));
        flows.get_mut(&key(RecSource::Agent)).unwrap().push(vec!["launch com.a.b --relaunch".into(), "tap 1 2".into()]);
        let agent = flows.get(&key(RecSource::Agent)).unwrap();
        assert_eq!(agent.text(), "# oximux-flow v1\n# platform ios\n# screen 402x874\n# app com.a.b\nlaunch com.a.b --relaunch\ntap 1 2\n");
        assert_eq!(flows.get(&key(RecSource::Panel)).unwrap().count(), 0, "the panel's holds none of the agent's");
        assert!(flows.forget_worktree(&key(RecSource::Agent).0));
        assert!(flows.get(&key(RecSource::Panel)).is_none());
    }

    fn exact(label: &str) -> SimSelectorWire {
        SimSelectorWire { label: Some(label.into()), id: None, role: None, nth: 0 }
    }

    #[test]
    fn panel_input_becomes_settled_steps_with_typing_kept_together() {
        let mut r = Recording { platform: "ios", ..Recording::default() };
        r.panel(PanelStep::Tap { at: (100.0, 200.0), target: Some((exact("Email"), false)) });
        for c in ["a", "d", "x"] {
            r.panel(PanelStep::Text(c.into()));
        }
        r.panel(PanelStep::Backspace);
        r.panel(PanelStep::Text("a@b.c".into()));
        r.panel(PanelStep::Tap { at: (100.0, 260.0), target: Some((exact("Password"), true)) });
        r.panel(PanelStep::Text("hunter2".into()));
        r.panel(PanelStep::Text("!".into()));
        r.panel(PanelStep::Tap { at: (201.0, 700.4), target: None });
        r.panel(PanelStep::Button(SimButtonWire::Home));
        assert_eq!(
            r.steps,
            [
                "tap --label Email --exact --settle",
                "type ada@b.c --settle",
                "tap --label Password --exact --settle",
                "type \"${SECRET_1}\" --settle",
                "tap 201 700.4 --settle",
                "button home --settle",
            ]
        );
        assert!(!r.text().contains("hunter2"), "a secret is never kept");
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    }

    #[test]
    fn backspacing_everything_typed_leaves_no_step() {
        let mut r = Recording::default();
        r.panel(PanelStep::Text("x".into()));
        r.panel(PanelStep::Backspace);
        assert!(r.steps.is_empty());
        r.panel(PanelStep::Backspace);
        assert!(r.steps.is_empty() && r.warnings.len() == 1);
    }

    #[test]
    fn secrets_are_numbered_and_warnings_kept_once() {
        let mut r = Recording::default();
        assert_eq!((r.next_secret(), r.next_secret()), ("${SECRET_1}".to_owned(), "${SECRET_2}".to_owned()));
        r.warn("a".into());
        r.warn("a".into());
        assert_eq!(r.warnings, ["a"]);
    }

    #[test]
    fn a_saved_flow_is_private_and_never_overwritten() {
        let w = WorktreeKey::from_path(std::path::Path::new("/some/worktree"));
        let (a, b) = (save(&w, "one").unwrap(), save(&w, "two").unwrap());
        assert_ne!(a, b);
        assert!(!a.to_string_lossy().contains("worktree"), "the path is hashed");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
