//! Flows (v30): recording an agent's verbs, approving a run's state changes
//! once, and exact selectors — plus the one text form a step takes, shared by
//! the host that records it and the CLI that runs and exports it.
//!
//! Re-exported from [`crate::simulator`]. Every type is new in v30, so its
//! shape is fixed from its first release (postcard: structs by field
//! position, enums by ordinal). Grow by minting a new type or appending a
//! variant at the end, never by adding a field.
//!
//! A step is one `oximux sim` invocation without the prefix: `tap --label
//! "Sign in" --exact --settle`. [`render_step`] writes one from a verb;
//! [`words::split`] reads a line back into the words the CLI's own parser
//! takes, so there is one grammar, not two.

use serde::{Deserialize, Serialize};

use crate::simulator::{
    SimButtonWire, SimCheckWire, SimCmdWire, SimLocationWire, SimOrientationWire, SimPrivacyActionWire, SimStateCmdWire,
    SimTargetWire,
};

/// The first line of every flow file.
pub const FLOW_HEADER: &str = "# oximux-flow v1";

/// A recording's or a run's control verbs (consent applies, as for any
/// control verb). They reach an agent's own recording only: the panel's is
/// the user's, out of any verb's reach.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SimFlowCmdWire {
    /// Record this worktree's agent verbs on its device, from now.
    Start,
    /// Stop and return the steps; `save` also keeps them in the app's data
    /// (private), whose path comes back.
    Stop { save: bool },
    /// Whether a recording runs, and its steps so far.
    Status,
    /// A run is about to change device state with these steps: ask the user
    /// once before step 1.
    ApproveRun { lines: Vec<String> },
}

/// A recording, as [`SimFlowCmdWire`] reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimFlowWire {
    pub recording: bool,
    /// The flow file: its header comments, then one step per line.
    pub steps: String,
    pub count: u32,
    /// What was not recorded, or needs a look before the file is shared.
    pub warnings: Vec<String>,
    /// Where the host saved it (`Stop { save: true }`).
    pub saved: Option<String>,
}

/// An element named exactly: a label and/or identifier equal to these (no
/// substring fallback), of `role` when given; the `nth` such element on
/// screen, from 0, in tree order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimSelectorWire {
    pub label: Option<String>,
    pub id: Option<String>,
    pub role: Option<String>,
    pub nth: u32,
}

/// Splitting a step into words and quoting a word so it splits back whole.
///
/// The grammar is a small, strict piece of the shell's: whitespace separates
/// words; `'…'` is literal; `"…"` takes `\"`, `\\`, `\n`, `\r`, `\t`; outside
/// quotes `\` takes the next character as is. No expansion of any kind, and
/// a raw control character (a line break above all) is refused: one line is
/// one step, whatever its arguments hold.
pub mod words {
    /// `line`'s words.
    pub fn split(line: &str) -> Result<Vec<String>, String> {
        if let Some(c) = line.chars().find(|c| c.is_control() && *c != '\t') {
            return Err(format!("a control character ({:?}) in a step", c));
        }
        let (mut out, mut word, mut in_word) = (Vec::new(), String::new(), false);
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => {
                    if in_word {
                        out.push(std::mem::take(&mut word));
                        in_word = false;
                    }
                }
                '\'' => {
                    in_word = true;
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(c) => word.push(c),
                            None => return Err("a ' is not closed".into()),
                        }
                    }
                }
                '"' => {
                    in_word = true;
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some('\\') => match chars.next() {
                                Some('n') => word.push('\n'),
                                Some('r') => word.push('\r'),
                                Some('t') => word.push('\t'),
                                Some(c @ ('"' | '\\')) => word.push(c),
                                Some(c) => {
                                    word.push('\\');
                                    word.push(c);
                                }
                                None => return Err("a \" is not closed".into()),
                            },
                            Some(c) => word.push(c),
                            None => return Err("a \" is not closed".into()),
                        }
                    }
                }
                '\\' => {
                    in_word = true;
                    word.push(chars.next().ok_or("a \\ ends the line")?);
                }
                c => {
                    in_word = true;
                    word.push(c);
                }
            }
        }
        if in_word {
            out.push(word);
        }
        Ok(out)
    }

    /// `s` as one word: bare when it is plain, else double-quoted with its
    /// line breaks and tabs escaped. A control character [`split`] cannot
    /// carry is refused.
    pub fn quote(s: &str) -> Result<String, String> {
        let plain = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:@%+,=".contains(c));
        if plain {
            return Ok(s.to_owned());
        }
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => return Err(format!("a control character ({c:?}) cannot be written in a step")),
                c => out.push(c),
            }
        }
        out.push('"');
        Ok(out)
    }
}

/// A step being written: its verb, flags and positional words.
struct Line {
    words: Vec<String>,
    flags: Vec<String>,
    /// Each quoted, and whether the value itself starts with `-`.
    args: Vec<(String, bool)>,
}

impl Line {
    fn new(verb: &str) -> Self {
        Self { words: verb.split(' ').map(str::to_owned).collect(), flags: Vec::new(), args: Vec::new() }
    }

    fn flag(mut self, name: &str) -> Self {
        self.flags.push(format!("--{name}"));
        self
    }

    /// `--name value`, or `--name=value` for a value starting with `-` (so
    /// it stays a value).
    fn opt(mut self, name: &str, value: &str) -> Result<Self, String> {
        let quoted = words::quote(value)?;
        if value.starts_with('-') {
            self.flags.push(format!("--{name}={quoted}"));
        } else {
            self.flags.push(format!("--{name}"));
            self.flags.push(quoted);
        }
        Ok(self)
    }

    fn arg(mut self, value: &str) -> Result<Self, String> {
        self.args.push((words::quote(value)?, value.starts_with('-')));
        Ok(self)
    }

    fn num(self, x: f64) -> Result<Self, String> {
        self.arg(&num(x))
    }

    /// The line: positionals before flags, or after `--` when one starts
    /// with `-` (a negative coordinate, a text like "-5").
    fn done(self) -> String {
        let mut out = self.words;
        // The value, not its quoting: `"-5 items"` is a dash value too.
        let dash = self.args.iter().any(|(_, dash)| *dash);
        let args = self.args.into_iter().map(|(a, _)| a);
        if dash {
            out.extend(self.flags);
            out.push("--".into());
            out.extend(args);
        } else {
            out.extend(args);
            out.extend(self.flags);
        }
        out.join(" ")
    }
}

/// A number as a step writes it: at most one decimal, none when whole.
pub fn num(x: f64) -> String {
    let r = (x * 10.0).round() / 10.0;
    if r == r.trunc() { format!("{}", r as i64) } else { format!("{r:.1}") }
}

/// The step lines that redo `cmd` (one verb is usually one line; typing into
/// an element is a tap, then the typing). Empty for a verb that is not a
/// step (a read: status, a screenshot, the tree, logs). An error when the
/// verb cannot be written down: an `@eN` ref (resolve it first), a push (its
/// payload is a file the step would have to carry).
pub fn render_step(cmd: &SimCmdWire) -> Result<Vec<String>, String> {
    Ok(lines(cmd)?.into_iter().map(Line::done).collect())
}

fn lines(cmd: &SimCmdWire) -> Result<Vec<Line>, String> {
    Ok(match cmd {
        SimCmdWire::AsCaller { cmd, .. } => return lines(cmd),
        SimCmdWire::Settled { action, timeout_ms } => {
            let mut out = lines(action)?;
            if settles(action)
                && let Some(last) = out.pop()
            {
                let last = last.flag("settle");
                // 0: the verb's own default.
                out.push(if *timeout_ms > 0 { last.opt("settle-timeout", &timeout_ms.to_string())? } else { last });
            }
            out
        }
        SimCmdWire::Tap(target) => vec![target_words(Line::new("tap"), target, true)?],
        SimCmdWire::Swipe { from, to, duration_ms } => {
            let line = Line::new("swipe").num(from.x)?.num(from.y)?.num(to.x)?.num(to.y)?;
            vec![if *duration_ms != 300 { line.opt("duration", &duration_ms.to_string())? } else { line }]
        }
        SimCmdWire::Type { text, paste } => vec![typing(text, *paste)?],
        SimCmdWire::TypeInto { target, text, paste } => vec![target_words(Line::new("tap"), target, true)?, typing(text, *paste)?],
        SimCmdWire::Button(b) => vec![Line::new("button").arg(button(*b))?],
        SimCmdWire::Rotate(o) => vec![Line::new("rotate").arg(orientation(*o))?],
        SimCmdWire::Launch { bundle_id, relaunch } => {
            let line = Line::new("launch").arg(bundle_id)?;
            vec![if *relaunch { line.flag("relaunch") } else { line }]
        }
        SimCmdWire::OpenUrl { url } => vec![Line::new("open-url").arg(url)?],
        SimCmdWire::Install { path } => vec![Line::new("install").arg(path)?],
        SimCmdWire::Wait { target, gone, timeout_ms } => {
            let line = target_words(Line::new("wait"), target, false)?;
            let line = if *gone { line.flag("gone") } else { line };
            vec![if *timeout_ms != 10_000 { line.opt("timeout-ms", &timeout_ms.to_string())? } else { line }]
        }
        SimCmdWire::Assert { target, check } => {
            let line = target_words(Line::new("assert"), target, false)?;
            vec![match check {
                SimCheckWire::Exists => line,
                SimCheckWire::Gone => line.flag("gone"),
                SimCheckWire::Enabled => line.flag("enabled"),
                SimCheckWire::Text(t) => line.opt("text", t)?,
            }]
        }
        SimCmdWire::DeviceState(state) => vec![device_state(state)?],
        SimCmdWire::Status
        | SimCmdWire::Devices
        | SimCmdWire::Attach { .. }
        | SimCmdWire::Detach
        | SimCmdWire::Screenshot { .. }
        | SimCmdWire::Ax { .. }
        | SimCmdWire::AxRefs { .. }
        | SimCmdWire::Shutdown { .. }
        | SimCmdWire::Logs(_)
        | SimCmdWire::Crashes(_)
        | SimCmdWire::Flow(_) => Vec::new(),
        // A flow never runs one (`sim run` refuses it).
        SimCmdWire::Exec(_) => return Err("a raw exec command is not a flow step".into()),
    })
}

/// Whether `cmd` takes `--settle`.
fn settles(cmd: &SimCmdWire) -> bool {
    matches!(
        cmd,
        SimCmdWire::Tap(_)
            | SimCmdWire::Swipe { .. }
            | SimCmdWire::Type { .. }
            | SimCmdWire::TypeInto { .. }
            | SimCmdWire::Button(_)
            | SimCmdWire::Rotate(_)
            | SimCmdWire::Launch { .. }
            | SimCmdWire::OpenUrl { .. }
    )
}

fn typing(text: &str, paste: bool) -> Result<Line, String> {
    let line = Line::new("type").arg(text)?;
    Ok(if paste { line.flag("paste") } else { line })
}

/// `target`'s words on `line`: a point (only where `point` is allowed), a
/// label, an identifier, or an exact selector. A ref has no meaning outside
/// the screen it was read from.
fn target_words(line: Line, target: &SimTargetWire, point: bool) -> Result<Line, String> {
    match target {
        SimTargetWire::Point(p) if point => line.num(p.x)?.num(p.y),
        SimTargetWire::Point(_) => Err("this verb names an element, not a point".into()),
        SimTargetWire::Label(label) => line.opt("label", label),
        SimTargetWire::Id(id) => line.opt("id", id),
        SimTargetWire::Exact(sel) => {
            let mut line = line;
            if let Some(label) = &sel.label {
                line = line.opt("label", label)?;
            }
            if let Some(id) = &sel.id {
                line = line.opt("id", id)?;
            }
            if let Some(role) = &sel.role {
                line = line.opt("role", role)?;
            }
            if sel.nth > 0 {
                line = line.opt("nth", &sel.nth.to_string())?;
            }
            if sel.label.is_none() && sel.id.is_none() && sel.role.is_none() {
                return Err("an exact selector names nothing".into());
            }
            // The CLI takes one or the other.
            if sel.label.is_some() && sel.id.is_some() {
                return Err("an exact selector with both a label and an id".into());
            }
            Ok(if sel.label.is_some() || sel.id.is_some() { line.flag("exact") } else { line })
        }
        SimTargetWire::Ref(r) => Err(format!("@e{r} is a ref to one screen; a step names the element itself")),
    }
}

fn device_state(state: &SimStateCmdWire) -> Result<Line, String> {
    Ok(match state {
        SimStateCmdWire::Location(SimLocationWire::Set { lat, lon }) => {
            // Coordinates keep their precision: a tenth of a degree is 11 km.
            Line::new("location set").arg(&lat.to_string())?.arg(&lon.to_string())?
        }
        SimStateCmdWire::Location(SimLocationWire::Clear) => Line::new("location clear"),
        SimStateCmdWire::Appearance { dark } => Line::new("appearance").arg(if *dark { "dark" } else { "light" })?,
        SimStateCmdWire::Privacy { action, service, app } => {
            let action = match action {
                SimPrivacyActionWire::Grant => "grant",
                SimPrivacyActionWire::Revoke => "revoke",
                SimPrivacyActionWire::Reset => "reset",
            };
            Line::new("privacy").arg(action)?.arg(service)?.opt("app", app)?
        }
        SimStateCmdWire::StatusBar { clean } => Line::new("status-bar").arg(if *clean { "clean" } else { "clear" })?,
        SimStateCmdWire::Erase { force } => {
            let line = Line::new("erase");
            if *force { line.flag("force") } else { line }
        }
        SimStateCmdWire::Push { app, .. } => return Err(format!("a push to {app} carries its payload; it is not written as a step")),
        SimStateCmdWire::Net(_) => return Err("network shaping is not written as a step".into()),
    })
}

/// A button as `sim button` names it.
pub fn button(b: SimButtonWire) -> &'static str {
    match b {
        SimButtonWire::Home => "home",
        SimButtonWire::Lock => "lock",
        SimButtonWire::Siri => "siri",
        SimButtonWire::SideButton => "side-button",
        SimButtonWire::AppSwitcher => "app-switcher",
        SimButtonWire::Back => "back",
        SimButtonWire::VolumeUp => "volume-up",
        SimButtonWire::VolumeDown => "volume-down",
    }
}

fn orientation(o: SimOrientationWire) -> &'static str {
    match o {
        SimOrientationWire::Portrait => "portrait",
        SimOrientationWire::LandscapeLeft => "landscape-left",
        SimOrientationWire::LandscapeRight => "landscape-right",
        SimOrientationWire::UpsideDown => "upside-down",
    }
}

/// A flow file's header: the format, then what a replay or an export needs
/// to know (`platform`: ios or android; `screen`: points, as recorded).
pub fn header(platform: &str, screen: Option<(f64, f64)>, app: Option<&str>) -> String {
    let mut out = format!("{FLOW_HEADER}\n# platform {platform}\n");
    if let Some((w, h)) = screen {
        out.push_str(&format!("# screen {}x{}\n", num(w), num(h)));
    }
    if let Some(app) = app {
        out.push_str(&format!("# app {app}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator::SimPointWire;

    fn one(cmd: SimCmdWire) -> String {
        let lines = render_step(&cmd).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        lines.into_iter().next().unwrap()
    }

    #[test]
    fn a_word_splits_back_whole() {
        for s in ["plain", "Sign in", "it's", "say \"hi\"", "a\\b", "two\nlines", "tab\there", "-5 items", "émoji 🙂", "${SECRET_1}"] {
            let q = words::quote(s).unwrap();
            assert_eq!(words::split(&q).unwrap(), vec![s.to_owned()], "{s:?} → {q}");
        }
        assert_eq!(words::split("tap --label 'Sign in' --exact").unwrap(), ["tap", "--label", "Sign in", "--exact"]);
        assert_eq!(words::split("a\\ b \"c\"d").unwrap(), ["a b", "cd"]);
        assert!(words::split("tap \"open").is_err());
        assert!(words::split("tap\nassert").is_err(), "a raw line break never makes two steps");
        assert!(words::quote("bell\u{7}").is_err());
    }

    #[test]
    fn each_verb_is_written_as_the_cli_takes_it() {
        let exact = SimTargetWire::Exact(SimSelectorWire { label: Some("Sign in".into()), id: None, role: None, nth: 0 });
        let settled = |action| SimCmdWire::Settled { action: Box::new(action), timeout_ms: 0 };
        assert_eq!(one(settled(SimCmdWire::Tap(exact.clone()))), "tap --label \"Sign in\" --exact --settle");
        let capped = SimCmdWire::Settled { action: Box::new(SimCmdWire::Button(SimButtonWire::Home)), timeout_ms: 900 };
        assert_eq!(one(capped), "button home --settle --settle-timeout 900", "0 is the verb's default; anything else is kept");
        assert_eq!(one(SimCmdWire::Tap(SimTargetWire::Point(SimPointWire { x: 201.04, y: 622.0 }))), "tap 201 622");
        assert_eq!(one(SimCmdWire::Tap(SimTargetWire::Point(SimPointWire { x: -3.0, y: 5.25 }))), "tap -- -3 5.3");
        let nth = SimTargetWire::Exact(SimSelectorWire { label: None, id: None, role: Some("Button".into()), nth: 2 });
        assert_eq!(one(SimCmdWire::Tap(nth)), "tap --role Button --nth 2");
        assert_eq!(one(SimCmdWire::Type { text: "-5".into(), paste: false }), "type -- -5");
        assert_eq!(one(SimCmdWire::Type { text: "-5 items".into(), paste: false }), "type -- \"-5 items\"", "a quoted dash value too");
        assert_eq!(one(SimCmdWire::Type { text: "ada@example.com".into(), paste: true }), "type ada@example.com --paste");
        let swipe = SimCmdWire::Swipe { from: SimPointWire { x: 200.0, y: 700.0 }, to: SimPointWire { x: 200.0, y: 200.0 }, duration_ms: 500 };
        assert_eq!(one(swipe), "swipe 200 700 200 200 --duration 500");
        assert_eq!(one(SimCmdWire::Button(SimButtonWire::VolumeUp)), "button volume-up");
        assert_eq!(one(settled(SimCmdWire::Launch { bundle_id: "com.a.b".into(), relaunch: true })), "launch com.a.b --relaunch --settle");
        let wait = SimCmdWire::Wait { target: SimTargetWire::Label("Home".into()), gone: true, timeout_ms: 3000 };
        assert_eq!(one(wait), "wait --label Home --gone --timeout-ms 3000");
        let assert = SimCmdWire::Assert { target: exact.clone(), check: SimCheckWire::Text("Hi there".into()) };
        assert_eq!(one(assert), "assert --label \"Sign in\" --exact --text \"Hi there\"");
        let into = SimCmdWire::TypeInto { target: SimTargetWire::Id("email".into()), text: "a b".into(), paste: false };
        assert_eq!(render_step(&settled(into)).unwrap(), ["tap --id email", "type \"a b\" --settle"]);
        let loc = SimCmdWire::DeviceState(SimStateCmdWire::Location(SimLocationWire::Set { lat: -33.8688, lon: 151.2093 }));
        assert_eq!(one(loc), "location set -- -33.8688 151.2093");
        let privacy = SimStateCmdWire::Privacy { action: SimPrivacyActionWire::Grant, service: "camera".into(), app: "com.a.b".into() };
        assert_eq!(one(SimCmdWire::DeviceState(privacy)), "privacy grant camera --app com.a.b");
        assert_eq!(one(SimCmdWire::Tap(SimTargetWire::Label("-5 items".into()))), "tap --label=\"-5 items\"");
    }

    #[test]
    fn reads_are_not_steps_and_refs_are_refused() {
        assert!(render_step(&SimCmdWire::Screenshot { full: false }).unwrap().is_empty());
        assert!(render_step(&SimCmdWire::Tap(SimTargetWire::Ref(3))).is_err());
        let push = SimStateCmdWire::Push { app: "com.a.b".into(), payload: "{}".into() };
        assert!(render_step(&SimCmdWire::DeviceState(push)).is_err());
        assert!(render_step(&SimCmdWire::Type { text: "bell\u{7}".into(), paste: false }).is_err());
    }

    #[test]
    fn a_header_says_what_a_replay_needs() {
        assert_eq!(header("ios", Some((402.0, 874.0)), Some("com.a.b")), "# oximux-flow v1\n# platform ios\n# screen 402x874\n# app com.a.b\n");
        assert_eq!(num(0.04), "0");
        assert_eq!(num(12.25), "12.3");
    }
}
