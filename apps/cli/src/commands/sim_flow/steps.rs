//! A flow file: header comments (`# oximux-flow v1`, `# platform ios`,
//! `# screen 402x874`, `# app com.example`), then one step per line — an
//! `oximux sim` command without the prefix, read by the CLI's own parser.
//! `#` lines are comments, blank lines are skipped.

use std::path::Path;

use clap::Parser as _;
use oximux_remote_proto::simulator::{SimCmdWire, words};

use crate::cli::{Cli, Command, SimCommand, exit};
use crate::output::Failure;

/// What the header comments say.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Header {
    pub platform: Option<String>,
    /// The screen in points, as recorded.
    pub screen: Option<(f64, f64)>,
    pub app: Option<String>,
}

/// One step: its number (from 1), its line in the file, the line itself,
/// and its words.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub n: usize,
    pub line: usize,
    pub text: String,
    pub words: Vec<String>,
}

/// The file at `path`, or stdin for `-`.
pub fn read(path: &Path) -> Result<String, Failure> {
    let failed = |e: std::io::Error| Failure::new("read", exit::ERROR, format!("cannot read {}: {e}", path.display()));
    if path == Path::new("-") {
        let mut src = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut src).map_err(failed)?;
        return Ok(src);
    }
    std::fs::read_to_string(path).map_err(failed)
}

/// `src`'s header and steps; a line that does not split (an open quote, a
/// control character) is refused with its line number.
pub fn parse(src: &str) -> Result<(Header, Vec<Step>), Failure> {
    let (mut header, mut steps) = (Header::default(), Vec::new());
    for (i, raw) in src.lines().enumerate() {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(comment) = text.strip_prefix('#') {
            let mut kv = comment.trim().splitn(2, ' ');
            match (kv.next(), kv.next().map(str::trim)) {
                (Some("platform"), Some(p)) => header.platform = Some(p.to_owned()),
                (Some("app"), Some(a)) => header.app = Some(a.to_owned()),
                (Some("screen"), Some(s)) => header.screen = screen(s),
                _ => {}
            }
            continue;
        }
        let words = words::split(text).map_err(|e| usage(format!("line {}: {e}", i + 1)))?;
        steps.push(Step { n: steps.len() + 1, line: i + 1, text: text.to_owned(), words });
    }
    Ok((header, steps))
}

/// `402x874` → (402, 874).
fn screen(s: &str) -> Option<(f64, f64)> {
    let (w, h) = s.split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0.0 && h > 0.0).then_some((w, h))
}

/// The command `words` spell, as `oximux sim …` would parse them. Verbs that
/// would nest a run, or wait on a person, are refused; so are the global
/// flags, which belong to the run.
pub fn command(words: &[String]) -> Result<SimCommand, String> {
    let argv = ["oximux", "sim"].into_iter().map(str::to_owned).chain(words.iter().cloned());
    // `try_parse_from`: a bad line is an error to report, never an exit.
    let cli = Cli::try_parse_from(argv).map_err(|e| e.render().to_string().lines().next().unwrap_or_default().trim_start_matches("error: ").to_owned())?;
    if cli.json || cli.dir.is_some() || cli.host.is_some() || cli.timeout != 10 {
        return Err("--json, --dir, --host and --timeout belong to the run, not a step".into());
    }
    let Command::Sim { worktree, command } = cli.command else { return Err("not a sim command".into()) };
    if worktree.is_some() {
        return Err("--worktree belongs to the run, not a step".into());
    }
    match command {
        SimCommand::Run(_) => Err("a step cannot run another file".into()),
        SimCommand::Flow(_) => Err("a step cannot record or export flows".into()),
        SimCommand::WaitConsent { .. } => Err("a step cannot wait for the user (exit 7 says to resume with --from)".into()),
        command => Ok(command),
    }
}

/// `command`'s text values with each `${SECRET_…}` replaced by that
/// environment variable: how a recorded secret (`type "${SECRET_1}"`) is
/// supplied at replay. After parsing, so a value stays a value whatever it
/// holds (a password starting with `-` is no flag), and a parse error can
/// only ever show the placeholder. Only `SECRET_` names are read — a flow
/// file cannot reach any other variable (a token in an `open-url`) — and
/// `$${` is a literal `${`.
///
/// Returns the values put in, so an error about the step can blank them.
pub fn substitute(command: &mut SimCommand) -> Result<Vec<String>, String> {
    let used = std::cell::RefCell::new(Vec::new());
    let var = |name: &str| {
        let value = std::env::var(name).ok();
        used.borrow_mut().extend(value.clone());
        value
    };
    let sub = |s: &mut String| -> Result<(), String> {
        *s = substitute_word(s, &var)?;
        Ok(())
    };
    let path = |p: &mut std::path::PathBuf| -> Result<(), String> {
        let text = p.to_string_lossy().into_owned();
        *p = substitute_word(&text, &var)?.into();
        Ok(())
    };
    let element = |e: &mut crate::cli_sim::ElementArgs| -> Result<(), String> {
        [&mut e.label, &mut e.id, &mut e.exact.role].into_iter().flatten().try_for_each(sub)
    };
    match command {
        SimCommand::Type { text, .. } => text.iter_mut().try_for_each(sub),
        SimCommand::Tap { label, id, exact, .. } => [label, id, &mut exact.role].into_iter().flatten().try_for_each(sub),
        SimCommand::Wait(w) => element(&mut w.element),
        SimCommand::Assert(a) => {
            element(&mut a.element)?;
            a.text.iter_mut().try_for_each(sub)
        }
        SimCommand::OpenUrl { url, .. } => sub(url),
        SimCommand::Launch { bundle_id, .. } => sub(bundle_id),
        SimCommand::Install { path: p } => path(p),
        SimCommand::Push(push) => {
            sub(&mut push.app)?;
            path(&mut push.file)
        }
        SimCommand::Privacy(privacy) => {
            sub(&mut privacy.service)?;
            sub(&mut privacy.app)
        }
        SimCommand::Attach { device, .. } => device.iter_mut().try_for_each(sub),
        // The rest take numbers, names from a fixed list, or nothing.
        _ => Ok(()),
    }?;
    Ok(used.into_inner())
}

/// `value` with each of `secrets` blanked in every string it holds (keys
/// too): on the values themselves, not their escaped JSON text.
pub fn redact_json(value: serde_json::Value, secrets: &[String]) -> serde_json::Value {
    use serde_json::Value;
    if secrets.is_empty() {
        return value;
    }
    match value {
        Value::String(s) => Value::String(redact(&s, secrets)),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| redact_json(v, secrets)).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (redact(&k, secrets), redact_json(v, secrets))).collect()),
        other => other,
    }
}

/// `message` with each of `secrets` blanked.
pub fn redact(message: &str, secrets: &[String]) -> String {
    secrets.iter().filter(|s| !s.is_empty()).fold(message.to_owned(), |m, s| m.replace(s.as_str(), "[redacted]"))
}

/// What the CLI checks of `command` before sending it (argument shapes
/// clap cannot express, a payload file that must exist), so a bad step 9
/// fails before step 1 runs.
pub fn check(command: SimCommand) -> Result<(), String> {
    match command {
        SimCommand::Type { text, .. } if text.len() > 1 => match crate::cli_sim::parse_ref(&text[0]) {
            Some(_) => Ok(()),
            None => Err("`type` takes TEXT, or an @eN ref then TEXT".into()),
        },
        SimCommand::Status
        | SimCommand::Devices { .. }
        | SimCommand::Screenshot { .. }
        | SimCommand::Ax { .. }
        | SimCommand::Wait(_)
        | SimCommand::Assert(_)
        | SimCommand::Logs(_)
        | SimCommand::Crashes(_)
        | SimCommand::WaitConsent { .. }
        | SimCommand::Run(_)
        | SimCommand::Flow(_) => Ok(()),
        SimCommand::Attach { platform: Some(_), .. } => Ok(()),
        other => {
            let cwd = std::env::current_dir().unwrap_or_default();
            super::super::sim::request_for(other, &cwd).map(drop).map_err(|f| f.message)
        }
    }
}

fn substitute_word(word: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(word.len());
    let mut rest = word;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix("${") {
            out.push_str("${");
            rest = tail;
            continue;
        }
        let name = after.strip_prefix('{').and_then(|a| a.find('}').map(|end| &a[..end]));
        match name.filter(|n| n.len() > 7 && n.starts_with("SECRET_") && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
            Some(name) => {
                out.push_str(&var(name).ok_or_else(|| format!("set ${name} in the environment (the flow types it)"))?);
                rest = &after[name.len() + 2..];
            }
            None => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The verb `command` sends, as an export reads it.
pub fn wire(command: SimCommand) -> Result<SimCmdWire, String> {
    let settle = command.settle_args().cloned().unwrap_or_default();
    let cmd = match command {
        SimCommand::Wait(w) => SimCmdWire::Wait { target: w.element.target(), gone: w.gone, timeout_ms: w.timeout_ms },
        SimCommand::Assert(a) => SimCmdWire::Assert { target: a.element.target(), check: a.check() },
        SimCommand::Type { text, .. } if text.len() > 1 => return Err("`type @eN` names a ref, which means nothing outside its screen".into()),
        SimCommand::Status
        | SimCommand::Devices { .. }
        | SimCommand::Screenshot { .. }
        | SimCommand::Ax { .. }
        | SimCommand::Logs(_)
        | SimCommand::Crashes(_)
        | SimCommand::WaitConsent { .. }
        | SimCommand::Run(_)
        | SimCommand::Flow(_) => return Err("a read, not an action".into()),
        other => {
            let cwd = std::env::current_dir().unwrap_or_default();
            super::super::sim::request_for(other, &cwd).map_err(|f| f.message)?.0
        }
    };
    // Not `sim_verify::wrap`: no caller envelope in a file.
    Ok(if settle.settle { SimCmdWire::Settled { action: Box::new(cmd), timeout_ms: settle.settle_timeout.unwrap_or(0) } } else { cmd })
}

fn usage(message: String) -> Failure {
    Failure::new("usage", exit::USAGE, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_remote_proto::simulator::{SimSelectorWire, SimTargetWire, render_step};

    fn words(line: &str) -> Vec<String> {
        words::split(line).unwrap()
    }

    #[test]
    fn a_file_has_a_header_and_numbered_steps() {
        let src = "# oximux-flow v1\n# platform ios\n# screen 402x874\n# app com.example.app\n\n# sign in\nlaunch com.example.app --relaunch --settle\n  tap --label \"Sign in\" --exact\n";
        let (header, steps) = parse(src).unwrap();
        assert_eq!(header, Header { platform: Some("ios".into()), screen: Some((402.0, 874.0)), app: Some("com.example.app".into()) });
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[1].n, steps[1].line, steps[1].words.clone()), (2, 8, vec!["tap".into(), "--label".into(), "Sign in".into(), "--exact".into()]));
        assert!(parse("tap \"open\n").unwrap_err().message.starts_with("line 1:"));
    }

    #[test]
    fn a_step_is_parsed_as_the_cli_parses_it() {
        assert!(matches!(command(&words("tap 10 20")), Ok(SimCommand::Tap { .. })));
        assert!(command(&words("tap --bogus")).unwrap_err().contains("--bogus"), "clap's error, not an exit");
        for refused in ["run x.flow", "flow start", "wait-consent", "--json tap 1 2", "tap 1 2 --worktree /w", "tap 1 2 --timeout 5"] {
            assert!(command(&words(refused)).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_secret_is_put_in_after_parsing_and_a_bad_step_is_caught_before_any_runs() {
        // SAFETY: a variable only this test reads.
        unsafe { std::env::set_var("SECRET_STEPS_TEST", "--paste") };
        let mut cmd = command(&words("type \"${SECRET_STEPS_TEST}\"")).unwrap();
        assert_eq!(substitute(&mut cmd).unwrap(), ["--paste"]);
        assert_eq!(redact("cannot read --paste/x", &["--paste".into()]), "cannot read [redacted]/x");
        let tricky = "a \"b\"\nc\\".to_owned();
        let value = serde_json::json!({ "node": { "value": format!("x{tricky}y"), "list": [tricky.clone()] } });
        let clean = redact_json(value, std::slice::from_ref(&tricky));
        assert_eq!(clean, serde_json::json!({ "node": { "value": "x[redacted]y", "list": ["[redacted]"] } }), "quotes, newlines, backslashes");
        match cmd {
            SimCommand::Type { text, paste, .. } => assert_eq!((text, paste), (vec!["--paste".to_owned()], false), "a value, not a flag"),
            other => panic!("{other:?}"),
        }
        assert!(check(command(&words("tap 1 2 --exact")).unwrap()).is_err(), "--exact on a point");
        assert!(check(command(&words("type hello world")).unwrap()).is_err());
        assert!(check(command(&words("tap 1 2")).unwrap()).is_ok());
        assert!(check(command(&words("push --app com.a.b /no/such/payload.json")).unwrap()).is_err());
    }

    #[test]
    fn a_secret_comes_from_the_environment() {
        let var = |name: &str| matches!(name, "SECRET_1" | "HOME").then(|| "hunter2".to_owned());
        assert_eq!(substitute_word("${SECRET_1}", &var).unwrap(), "hunter2");
        assert_eq!(substitute_word("pre-${SECRET_1}-post", &var).unwrap(), "pre-hunter2-post");
        assert!(substitute_word("${SECRET_2}", &var).unwrap_err().contains("SECRET_2"));
        assert_eq!(substitute_word("cost ${1x} and ${", &var).unwrap(), "cost ${1x} and ${", "not a name: kept");
        assert_eq!(substitute_word("https://x/?t=${HOME}", &var).unwrap(), "https://x/?t=${HOME}", "only SECRET_ names are read");
        assert_eq!(substitute_word("$${SECRET_1} $5", &var).unwrap(), "${SECRET_1} $5", "a doubled $ is a literal one");
    }

    /// What the host writes, the CLI reads back to the same verb.
    #[test]
    fn a_rendered_step_parses_back_to_its_verb() {
        let exact = SimTargetWire::Exact(SimSelectorWire { label: Some("Sign \"in\"".into()), id: None, role: Some("Button".into()), nth: 1 });
        let cases = [
            SimCmdWire::Settled { action: Box::new(SimCmdWire::Tap(exact.clone())), timeout_ms: 0 },
            SimCmdWire::Settled { action: Box::new(SimCmdWire::Button(oximux_remote_proto::simulator::SimButtonWire::Home)), timeout_ms: 900 },
            SimCmdWire::Tap(SimTargetWire::Label("-5 items".into())),
            SimCmdWire::Type { text: "two\nlines and \\ back".into(), paste: false },
            SimCmdWire::Type { text: "-dash".into(), paste: true },
            SimCmdWire::Wait { target: exact.clone(), gone: true, timeout_ms: 3000 },
            SimCmdWire::Launch { bundle_id: "com.a.b".into(), relaunch: true },
        ];
        for cmd in cases {
            let line = render_step(&cmd).unwrap().remove(0);
            let back = wire(command(&words::split(&line).unwrap()).unwrap()).unwrap();
            assert_eq!(back, cmd, "{line}");
        }
    }

    /// No argument, however odd, turns one line into two steps.
    #[test]
    fn no_argument_makes_a_second_step() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "ab \"'\\\n\r\t$#-{}é🙂\u{7}\u{1b};|&".chars().collect();
        for _ in 0..2000 {
            let len = (next() % 12) as usize;
            let text: String = (0..len).map(|_| alphabet[(next() % alphabet.len() as u64) as usize]).collect();
            let cmd = SimCmdWire::Type { text: text.clone(), paste: false };
            match render_step(&cmd) {
                Ok(lines) => {
                    assert_eq!(lines.len(), 1);
                    assert!(!lines[0].contains('\n') && !lines[0].contains('\r'), "{:?}", lines[0]);
                    let back = words::split(&lines[0]).unwrap();
                    assert_eq!(back.last(), Some(&text), "{:?}", lines[0]);
                }
                Err(_) => assert!(text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')), "{text:?}"),
            }
        }
    }
}
