//! `sim flow export --maestro`: a flow file in that YAML test-flow format,
//! a strict subset — every scalar double-quoted, labels and identifiers as
//! escaped regular expressions (that is how the format matches text),
//! points as whole percentages of the recorded screen (it takes no
//! fractions).
//!
//! A step with no counterpart is never dropped silently: it stays in the
//! YAML as `# unsupported: <step>` and is counted in the warnings.

use oximux_remote_proto::simulator::{
    SimButtonWire, SimCheckWire, SimCmdWire, SimLocationWire, SimPointWire, SimSelectorWire, SimStateCmdWire, SimTargetWire,
};

use super::steps::Header;

/// One step as the exporter takes it: its number, its line, and its verb
/// (`None` when it parsed but is no action, e.g. `screenshot`).
pub struct Input<'a> {
    pub n: usize,
    pub text: &'a str,
    pub cmd: Option<SimCmdWire>,
}

/// The YAML, and what did not carry over.
#[derive(Debug)]
pub struct Exported {
    pub yaml: String,
    pub warnings: Vec<String>,
}

/// Why a step was not exported: left in as a comment, or the whole export
/// fails (a point step with no screen size to scale it by).
enum Skip {
    Unsupported(String),
    Fatal(String),
}

pub fn export(header: &Header, steps: &[Input<'_>], app_id: Option<&str>) -> Result<Exported, String> {
    let first_launch = steps.iter().find_map(|s| match s.cmd.as_ref().map(unwrap) {
        Some(SimCmdWire::Launch { bundle_id, .. }) => Some(bundle_id.clone()),
        _ => None,
    });
    let app = app_id
        .map(str::to_owned)
        .or_else(|| header.app.clone())
        .or(first_launch)
        .ok_or("no app id: pass --app-id, or add `# app <id>` to the file")?;
    let mut yaml = format!("appId: {}\n---\n", q(&app));
    let mut warnings = Vec::new();
    for step in steps {
        let mut out = Vec::new();
        let result = match &step.cmd {
            Some(cmd) => command(cmd, header, &mut out, &mut warnings, step.n),
            None => Err(Skip::Unsupported("not an action".into())),
        };
        match result {
            Ok(()) => yaml.extend(out.into_iter().map(|l| l + "\n")),
            Err(Skip::Unsupported(why)) => {
                yaml.push_str(&format!("# unsupported: {}\n", comment(step.text)));
                warnings.push(format!("step {}: {why}: {}", step.n, step.text));
            }
            Err(Skip::Fatal(why)) => return Err(format!("step {}: {why}: {}", step.n, step.text)),
        }
    }
    Ok(Exported { yaml, warnings })
}

/// Envelopes off (a settle stays: it adds a wait).
fn unwrap(cmd: &SimCmdWire) -> &SimCmdWire {
    match cmd {
        SimCmdWire::AsCaller { cmd, .. } => unwrap(cmd),
        other => other,
    }
}

fn command(cmd: &SimCmdWire, header: &Header, out: &mut Vec<String>, warnings: &mut Vec<String>, n: usize) -> Result<(), Skip> {
    let unsupported = |why: &str| Err(Skip::Unsupported(why.to_owned()));
    match unwrap(cmd) {
        SimCmdWire::Settled { action, .. } => {
            command(action, header, out, warnings, n)?;
            out.push("- waitForAnimationToEnd".into());
        }
        SimCmdWire::Tap(SimTargetWire::Point(p)) => {
            out.push("- tapOn:".into());
            out.push(format!("    point: {}", q(&percent(*p, header)?)));
        }
        SimCmdWire::Tap(target) => {
            out.push("- tapOn:".into());
            selector(target, out, warnings, n, "    ")?;
        }
        SimCmdWire::Swipe { from, to, duration_ms } => {
            out.push("- swipe:".into());
            out.push(format!("    start: {}", q(&percent(*from, header)?)));
            out.push(format!("    end: {}", q(&percent(*to, header)?)));
            out.push(format!("    duration: {duration_ms}"));
        }
        SimCmdWire::Type { text, .. } => out.push(format!("- inputText: {}", q(text))),
        SimCmdWire::Button(b) => {
            let key = match b {
                SimButtonWire::Home => "Home",
                SimButtonWire::Lock => "Lock",
                SimButtonWire::Back => "Back",
                SimButtonWire::VolumeUp => "Volume Up",
                SimButtonWire::VolumeDown => "Volume Down",
                SimButtonWire::Siri | SimButtonWire::SideButton | SimButtonWire::AppSwitcher => return unsupported("no such key"),
            };
            if *b == SimButtonWire::Back && header.platform.as_deref() != Some("android") {
                out.push("# back: Android only".into());
                warnings.push(format!("step {n}: back is an Android key"));
            }
            out.push(format!("- pressKey: {}", q(key)));
        }
        SimCmdWire::Launch { bundle_id, relaunch } => {
            out.push("- launchApp:".into());
            out.push(format!("    appId: {}", q(bundle_id)));
            // A relaunch terminates first; app data is never cleared.
            out.push(format!("    stopApp: {relaunch}"));
        }
        SimCmdWire::OpenUrl { url } => out.push(format!("- openLink: {}", q(url))),
        SimCmdWire::Wait { target, gone, timeout_ms } => {
            out.push("- extendedWaitUntil:".into());
            out.push(format!("    {}:", if *gone { "notVisible" } else { "visible" }));
            selector(target, out, warnings, n, "      ")?;
            out.push(format!("    timeout: {timeout_ms}"));
        }
        SimCmdWire::Assert { target, check } => {
            let (verb, enabled) = match check {
                SimCheckWire::Exists => ("assertVisible", false),
                SimCheckWire::Enabled => ("assertVisible", true),
                SimCheckWire::Gone => ("assertNotVisible", false),
                SimCheckWire::Text(_) => return unsupported("a text check has no counterpart"),
            };
            out.push(format!("- {verb}:"));
            selector(target, out, warnings, n, "    ")?;
            if enabled {
                out.push("    enabled: true".into());
            }
        }
        SimCmdWire::DeviceState(SimStateCmdWire::Location(SimLocationWire::Set { lat, lon })) => {
            out.push("- setLocation:".into());
            out.push(format!("    latitude: {}", q(&lat.to_string())));
            out.push(format!("    longitude: {}", q(&lon.to_string())));
        }
        SimCmdWire::Rotate(_) => return unsupported("rotation has no counterpart"),
        SimCmdWire::TypeInto { .. } => return unsupported("typing into a ref"),
        SimCmdWire::Install { .. } => return unsupported("installing has no counterpart"),
        SimCmdWire::DeviceState(_) => return unsupported("only setting a location has a counterpart"),
        _ => return unsupported("no counterpart"),
    }
    Ok(())
}

/// A target's selector keys, at `indent`.
fn selector(target: &SimTargetWire, out: &mut Vec<String>, warnings: &mut Vec<String>, n: usize, indent: &str) -> Result<(), Skip> {
    match target {
        SimTargetWire::Label(label) => {
            // A plain --label also matches part of a label; exported, it is
            // the whole label.
            warnings.push(format!("step {n}: --label without --exact exports as an exact match"));
            out.push(format!("{indent}text: {}", q(&regex(label))));
        }
        SimTargetWire::Id(id) => out.push(format!("{indent}id: {}", q(&regex(id)))),
        SimTargetWire::Exact(SimSelectorWire { label, id, role, nth }) => {
            if label.is_none() && id.is_none() {
                return Err(Skip::Unsupported("a role alone has no counterpart".into()));
            }
            if let Some(label) = label {
                out.push(format!("{indent}text: {}", q(&regex(label))));
            }
            if let Some(id) = id {
                out.push(format!("{indent}id: {}", q(&regex(id))));
            }
            if *nth > 0 {
                out.push(format!("{indent}index: {nth}"));
            }
            if role.is_some() {
                warnings.push(format!("step {n}: --role is not exported (only the label or id and --nth are)"));
            }
        }
        SimTargetWire::Point(_) => return Err(Skip::Unsupported("a point where an element is named".into())),
        SimTargetWire::Ref(_) => return Err(Skip::Unsupported("an @eN ref".into())),
    }
    Ok(())
}

/// A point as `"x%,y%"` of the recorded screen.
fn percent(p: SimPointWire, header: &Header) -> Result<String, Skip> {
    let (w, h) = header.screen.ok_or_else(|| Skip::Fatal("a point step needs `# screen WxH` in the file".into()))?;
    Ok(format!("{:.0}%,{:.0}%", p.x / w * 100.0, p.y / h * 100.0))
}

/// `s` matched literally as a regular expression.
fn regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `s` safe in a `#` comment: anything YAML takes for a line break (a
/// control character, U+0085, U+2028, U+2029) becomes a space, so nothing
/// after it is read as YAML.
fn comment(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}') { ' ' } else { c }).collect()
}

/// `s` as a double-quoted YAML scalar.
fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() || matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}') => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::super::steps;
    use super::*;

    fn exported(src: &str, app_id: Option<&str>) -> Result<Exported, String> {
        let (header, all) = steps::parse(src).unwrap();
        let inputs: Vec<Input<'_>> =
            all.iter().map(|s| Input { n: s.n, text: &s.text, cmd: steps::command(&s.words).ok().and_then(|c| steps::wire(c).ok()) }).collect();
        export(&header, &inputs, app_id)
    }

    #[test]
    fn the_login_flow_matches_its_golden() {
        let out = exported(include_str!("../../../tests/fixtures/flows/login.flow"), None).unwrap();
        assert_eq!(out.yaml, include_str!("../../../tests/fixtures/flows/login.yaml"));
        assert_eq!(out.warnings.len(), 2, "{:?}", out.warnings);
        assert!(!out.yaml.contains("clearState"), "app data is never cleared");
    }

    #[test]
    fn labels_are_escaped_regexes_in_quoted_scalars() {
        assert_eq!(q(&regex("Price (USD)")), r#""Price \\(USD\\)""#);
        assert_eq!(q("say \"hi\"\n"), r#""say \"hi\"\n""#);
        assert_eq!(regex("a.b*"), r"a\.b\*");
        assert_eq!(q("a\u{2028}b"), r#""a\u2028b""#, "a line separator stays inside the scalar");
        assert_eq!(comment("tap\u{2028}- tapOn: x"), "tap - tapOn: x");
    }

    #[test]
    fn a_plain_launch_keeps_the_app_running_and_a_relaunch_stops_it() {
        let out = exported("launch com.a.b\nlaunch com.a.b --relaunch\n", None).unwrap();
        assert_eq!(
            out.yaml,
            "appId: \"com.a.b\"\n---\n- launchApp:\n    appId: \"com.a.b\"\n    stopApp: false\n- launchApp:\n    appId: \"com.a.b\"\n    stopApp: true\n"
        );
    }

    #[test]
    fn a_point_needs_the_screen_and_an_app_needs_an_id() {
        assert!(exported("tap 10 20\n", Some("com.a.b")).unwrap_err().contains("# screen"));
        assert!(exported("tap --label OK --exact\n", None).unwrap_err().contains("--app-id"));
        let out = exported("# screen 400x800\ntap 100 200\n", Some("x")).unwrap();
        assert!(out.yaml.contains("point: \"25%,25%\""), "{}", out.yaml);
    }
}
