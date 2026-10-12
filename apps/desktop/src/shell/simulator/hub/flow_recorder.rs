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
    /// A tap at `at` (points, on a `screen` that size), on the element
    /// `target` names exactly when one was read in time, with what typing
    /// there goes into (see `ax::field_kind`).
    Tap { at: (f64, f64), screen: (f64, f64), target: Option<(SimSelectorWire, Option<bool>)> },
    Swipe { from: (f64, f64), to: (f64, f64), screen: (f64, f64), duration_ms: u32 },
    /// Typed characters.
    Text(String),
    Backspace,
    /// Return: ends the typing (focus may move to another field).
    Return,
    /// Input a flow cannot hold (an arrow key): said once in the warnings.
    Skipped(String),
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
    /// Which recording this is (a verb planned for one never lands in the
    /// next).
    id: u64,
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
    /// Whether the field typing goes into is a password field: `None` when
    /// it is not known (a tap kept as a point, a Return), which records
    /// typing as a secret too — a password is never written down by guess.
    field: Option<bool>,
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

    pub(crate) fn id(&self) -> u64 {
        self.id
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
            PanelStep::Tap { at, screen, target } => {
                self.fill_screen(Some(screen));
                self.field = target.as_ref().and_then(|(_, kind)| *kind);
                let aim = match target {
                    Some((sel, _)) => SimTargetWire::Exact(sel),
                    None => {
                        self.warn("a tap was recorded as a point (no element read in time): it may miss on another device".into());
                        SimTargetWire::Point(point(at))
                    }
                };
                settled(SimCmdWire::Tap(aim))
            }
            PanelStep::Swipe { from, to, screen, duration_ms } => {
                self.fill_screen(Some(screen));
                self.field = None;
                settled(SimCmdWire::Swipe { from: point(from), to: point(to), duration_ms })
            }
            PanelStep::Button(b) => {
                self.field = None;
                settled(SimCmdWire::Button(b))
            }
            PanelStep::Rotate(o) => settled(SimCmdWire::Rotate(o)),
            PanelStep::Text(text) => return self.typed(Some(&text)),
            PanelStep::Backspace => return self.typed(None),
            PanelStep::Return => {
                // Into plain typing; after a placeholder (a password, then
                // Return to submit), a step of its own.
                if self.typing.as_ref().is_some_and(|t| !t.secret) {
                    self.typed(Some("\n"));
                } else {
                    self.add(&settled(SimCmdWire::Type { text: "\n".into(), paste: false }));
                }
                self.typing = None;
                self.field = None;
                return;
            }
            PanelStep::Skipped(what) => {
                // A Tab or an arrow may have moved focus: typing after it
                // goes into a field not known.
                self.typing = None;
                self.field = None;
                return self.warn(format!("not recorded: {what}"));
            }
        };
        self.add(&cmd);
    }

    /// An agent's verb that succeeded, its targets already named: typing goes
    /// in as a placeholder unless the field it lands in is known not to be a
    /// password field (`field`: what the verb's own target is, when it has
    /// one — `Some(None)`: a target that could not be looked up).
    pub(crate) fn agent(&mut self, cmd: SimCmdWire, field: Option<Option<bool>>) {
        let cmd = match cmd {
            SimCmdWire::AsCaller { cmd, .. } => *cmd,
            other => other,
        };
        let (settle, inner) = match cmd {
            SimCmdWire::Settled { action, timeout_ms } => (Some(timeout_ms), *action),
            other => (None, other),
        };
        let inner = match inner {
            SimCmdWire::Type { text, paste } => {
                let text = self.guard_secret(text, self.field);
                SimCmdWire::Type { text, paste }
            }
            SimCmdWire::TypeInto { target, text, paste } => {
                let into = field.flatten();
                let text = self.guard_secret(text, into);
                self.field = into;
                SimCmdWire::TypeInto { target, text, paste }
            }
            other => {
                // Focus may have moved: only a tap says where to.
                self.field = match &other {
                    SimCmdWire::Tap(_) => field.flatten(),
                    _ => None,
                };
                other
            }
        };
        let cmd = match settle {
            Some(timeout_ms) => SimCmdWire::Settled { action: Box::new(inner), timeout_ms },
            None => inner,
        };
        self.add(&cmd);
    }

    /// `text` as a step may hold it: itself in a field known not to be a
    /// password field, else the next placeholder.
    fn guard_secret(&mut self, text: String, field: Option<bool>) -> String {
        if field == Some(false) {
            return text.replace("${", "$${");
        }
        let name = self.next_secret();
        self.warn(secret_warning(&name, field));
        name
    }

    /// `text` typed (`None`: a backspace), into the field the last tap hit.
    fn typed(&mut self, text: Option<&str>) {
        if self.typing.is_none() {
            if self.field != Some(false) && text.is_some() {
                let name = self.next_secret();
                self.warn(secret_warning(&name, self.field));
                let typing = Typing { text: name, secret: true };
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
            // A `${` typed is kept literal (`$${`): only a placeholder is
            // read from the environment at replay.
            Some(text) => typing.text.push_str(&text.replace("${", "$${")),
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

    /// The screen's size, once known (a point step is scaled by it).
    pub(crate) fn fill_screen(&mut self, screen: Option<(f64, f64)>) {
        if self.screen.is_none() {
            self.screen = screen;
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

fn secret_warning(name: &str, field: Option<bool>) -> String {
    match field {
        Some(true) => format!("typing into a password field was recorded as {name}: set it in the environment to replay"),
        _ => format!(
            "typing into a field that could not be read was recorded as {name} (it may be a password): set it to replay, or put the text back in the file if it is not secret"
        ),
    }
}

/// Every recording in progress.
#[derive(Debug, Default)]
pub(crate) struct FlowRecorder {
    recordings: HashMap<RecKey, Recording>,
    next: u64,
    /// Stopped recordings being saved: none starts under the key meanwhile,
    /// so a failed save can put its recording back.
    saving: std::collections::HashSet<RecKey>,
}

impl FlowRecorder {
    /// Start recording under `key`; false when one already runs.
    pub(crate) fn start(&mut self, key: RecKey, platform: &'static str, screen: Option<(f64, f64)>) -> bool {
        if self.recordings.contains_key(&key) || self.saving.contains(&key) {
            return false;
        }
        self.next += 1;
        self.recordings.insert(key, Recording { id: self.next, platform, screen, ..Recording::default() });
        true
    }

    /// Whether a stopped recording is being saved under `key`.
    pub(crate) fn is_saving(&self, key: &RecKey) -> bool {
        self.saving.contains(key)
    }

    /// Stop the recording under `key` to save it: until [`Self::saved`],
    /// nothing starts under the key.
    pub(crate) fn stop_to_save(&mut self, key: &RecKey) -> Option<Recording> {
        let recording = self.recordings.remove(key)?;
        self.saving.insert(key.clone());
        Some(recording)
    }

    /// The save of a recording stopped by [`Self::stop_to_save`] ended; a
    /// failed one (`Some`) puts it back.
    pub(crate) fn saved(&mut self, key: RecKey, failed: Option<Recording>) {
        self.saving.remove(&key);
        if let Some(recording) = failed {
            self.recordings.insert(key, recording);
        }
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

    /// End `worktree`'s recordings (it detached or changed device): they
    /// are returned, for what is worth keeping.
    pub(crate) fn forget_worktree(&mut self, worktree: &WorktreeKey) -> Vec<(RecKey, Recording)> {
        let keys: Vec<RecKey> = self.recordings.keys().filter(|(w, _, _)| w == worktree).cloned().collect();
        keys.into_iter().filter_map(|k| self.recordings.remove(&k).map(|r| (k, r))).collect()
    }
}

impl SimulatorHub {
    pub(crate) fn flows(&self) -> &FlowRecorder {
        &self.flows
    }

    pub(crate) fn flows_mut(&mut self) -> &mut FlowRecorder {
        &mut self.flows
    }

    /// A worktree's recordings end when it detaches or changes device. The
    /// user's own (the panel's) is saved, not lost, and they are told where;
    /// an agent's goes (its `flow stop` would have said so).
    pub(super) fn forget_flows(&mut self, worktree: &WorktreeKey, cx: &mut gpui::Context<Self>) {
        let ended = self.flows.forget_worktree(worktree);
        if ended.is_empty() {
            return;
        }
        cx.emit(super::HubEvent::Flow);
        for ((_, _, source), recording) in ended {
            if source != RecSource::Panel || recording.count() == 0 {
                continue;
            }
            // A few hundred bytes: written here, so the notice can say where.
            let notice = match save(worktree, &recording.text()) {
                Ok(path) => (super::NoticeKind::Success, format!("The flow recording ended with the device; saved: {}", path.display())),
                Err(why) => (super::NoticeKind::Error, format!("The flow recording ended with the device and could not be saved ({why})")),
            };
            cx.emit(super::HubEvent::FlowNotice(worktree.path().to_path_buf(), notice.0, notice.1));
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
        assert_eq!(flows.forget_worktree(&key(RecSource::Agent).0).len(), 2);
        assert!(flows.get(&key(RecSource::Panel)).is_none());
    }

    fn exact(label: &str) -> SimSelectorWire {
        SimSelectorWire { label: Some(label.into()), id: None, role: None, nth: 0 }
    }

    #[test]
    fn panel_input_becomes_settled_steps_with_typing_kept_together() {
        let mut r = Recording { platform: "ios", ..Recording::default() };
        let screen = (402.0, 874.0);
        r.panel(PanelStep::Tap { at: (100.0, 200.0), screen, target: Some((exact("Email"), Some(false))) });
        for c in ["a", "d", "x"] {
            r.panel(PanelStep::Text(c.into()));
        }
        r.panel(PanelStep::Backspace);
        r.panel(PanelStep::Text("a@b.c".into()));
        r.panel(PanelStep::Tap { at: (100.0, 260.0), screen, target: Some((exact("Password"), Some(true))) });
        r.panel(PanelStep::Text("hunter2".into()));
        r.panel(PanelStep::Text("!".into()));
        r.panel(PanelStep::Tap { at: (201.0, 700.4), screen, target: None });
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
        assert!(r.text().contains("# screen 402x874\n"), "a point step's screen is in the header");
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    }

    /// A field that could not be read is taken for a password: nothing
    /// typed is written down by guess — after a point tap, after Return.
    #[test]
    fn typing_into_an_unknown_field_is_kept_as_a_secret() {
        let mut r = Recording::default();
        let screen = (402.0, 874.0);
        r.panel(PanelStep::Tap { at: (100.0, 200.0), screen, target: Some((exact("Email"), Some(false))) });
        r.panel(PanelStep::Text("ada@b.c".into()));
        r.panel(PanelStep::Return);
        r.panel(PanelStep::Text("hunter2".into()));
        r.panel(PanelStep::Tap { at: (10.0, 10.0), screen, target: None });
        r.panel(PanelStep::Text("also-secret".into()));
        let text = r.text();
        assert!(!text.contains("hunter2") && !text.contains("also-secret"), "{text}");
        assert!(text.contains("type \"ada@b.c\\n\" --settle"), "{text}");
        assert!(text.contains("${SECRET_1}") && text.contains("${SECRET_2}"), "{text}");
    }

    /// An agent's typing: its own text only where the field is known not to
    /// be a password field.
    #[test]
    fn an_agents_typing_into_a_password_field_is_a_placeholder() {
        let mut r = Recording::default();
        let tap = |label: &str| SimCmdWire::Tap(SimTargetWire::Exact(exact(label)));
        r.agent(tap("Email"), Some(Some(false)));
        r.agent(SimCmdWire::Type { text: "ada@b.c".into(), paste: false }, None);
        r.agent(tap("Password"), Some(Some(true)));
        r.agent(SimCmdWire::Type { text: "hunter2".into(), paste: false }, None);
        let into = SimCmdWire::TypeInto { target: SimTargetWire::Exact(exact("Code")), text: "123456".into(), paste: false };
        r.agent(SimCmdWire::Settled { action: Box::new(into), timeout_ms: 0 }, Some(None));
        r.agent(SimCmdWire::Button(SimButtonWire::Home), None);
        r.agent(SimCmdWire::Type { text: "after-home".into(), paste: false }, None);
        let text = r.text();
        assert!(text.contains("type ada@b.c\n"), "{text}");
        for secret in ["hunter2", "123456", "after-home"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("tap --label Code --exact\ntype \"${SECRET_2}\" --settle"), "{text}");
    }

    /// Return after a password submits: its own step. A Tab may move focus:
    /// typing after it is a secret. A `${` typed stays literal.
    #[test]
    fn return_tab_and_a_typed_dollar_brace() {
        let mut r = Recording::default();
        let screen = (402.0, 874.0);
        r.panel(PanelStep::Tap { at: (1.0, 1.0), screen, target: Some((exact("Password"), Some(true))) });
        r.panel(PanelStep::Text("hunter2".into()));
        r.panel(PanelStep::Return);
        r.panel(PanelStep::Tap { at: (1.0, 1.0), screen, target: Some((exact("Note"), Some(false))) });
        r.panel(PanelStep::Text("cost ${X}".into()));
        r.panel(PanelStep::Skipped("arrow, tab, delete, home and end keys".into()));
        r.panel(PanelStep::Text("next-field".into()));
        let steps = r.steps.clone();
        assert_eq!(steps[1], "type \"${SECRET_1}\" --settle");
        assert_eq!(steps[2], "type \"\\n\" --settle", "Return submits on replay");
        assert_eq!(steps[4], "type \"cost $${X}\" --settle");
        assert_eq!(steps[5], "type \"${SECRET_2}\" --settle", "after a Tab the field is not known");
    }

    #[test]
    fn backspacing_everything_typed_leaves_no_step() {
        let mut r = Recording::default();
        r.panel(PanelStep::Tap { at: (1.0, 1.0), screen: (402.0, 874.0), target: Some((exact("Name"), Some(false))) });
        r.steps.clear();
        r.panel(PanelStep::Text("x".into()));
        r.panel(PanelStep::Backspace);
        assert!(r.steps.is_empty());
        r.panel(PanelStep::Backspace);
        assert!(r.steps.is_empty() && r.warnings.len() == 1);
    }

    #[test]
    fn nothing_starts_while_a_stopped_recording_is_saved() {
        let mut flows = FlowRecorder::default();
        let k = key(RecSource::Agent);
        flows.start(k.clone(), "ios", None);
        flows.get_mut(&k).unwrap().push(vec!["tap 1 2".into()]);
        let recording = flows.stop_to_save(&k).unwrap();
        assert!(!flows.start(k.clone(), "ios", None), "not while saving");
        flows.saved(k.clone(), Some(recording));
        assert_eq!(flows.get(&k).map(|r| r.count()), Some(1), "a failed save keeps the steps");
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
