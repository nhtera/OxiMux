//! The six tools of `oximux mcp`, each the CLI's own `sim` verb(s): the
//! arguments become the words a user would type, parsed by the CLI's own
//! parser, gated by its version table and run by [`super::super::sim::run`].
//! So a tool's text is the CLI's, word for word.
//!
//! Every call has [`BUDGET`]: below the usual client timeout, so a client
//! never gives up on a call the host is still running. A `wait` that would
//! not fit is refused up front; a call that runs out is dropped, and with it
//! its connection, which the host takes as "stop the verb".

use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use clap::Parser;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::Ctx;
use crate::cli::{Command, SimCommand};
use crate::client::Client;
use crate::output::Failure;

/// The most one tool call may take.
pub(super) const BUDGET: Duration = Duration::from_secs(25);
/// The longest `sim_wait_consent` wait (the rest of the budget is the
/// connection and the answer).
const MAX_CONSENT_WAIT: u64 = 20;
/// Kept back from a `wait` step's share of the budget: the verb's own
/// round trip.
const WAIT_SLACK: Duration = Duration::from_secs(2);

/// The CLI's `sim` words, parsed as the CLI parses them.
#[derive(Parser)]
#[command(no_binary_name = true)]
struct Words {
    #[command(subcommand)]
    command: SimCommand,
}

/// `tools/list`.
pub(super) fn list() -> Value {
    let target = json!({
        "type": "object",
        "description": "One of: ref (\"@e3\" from sim_observe), label (+ exact), id (accessibility id), or x and y (points).",
        "properties": {
            "ref": { "type": "string" }, "label": { "type": "string" }, "exact": { "type": "boolean" },
            "id": { "type": "string" }, "x": { "type": "number" }, "y": { "type": "number" }
        }
    });
    let point = json!({ "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } }, "required": ["x", "y"] });
    json!([
        tool("sim_status", "The worktree's device, whether it streams, and whether the user allowed this client (oximux sim status).", json!({})),
        tool(
            "sim_observe",
            "The screen as accessibility elements with @eN refs to act on (oximux sim ax -i); interactive=false lists every element. screenshot=true adds an image.",
            json!({ "interactive": { "type": "boolean", "default": true }, "max": { "type": "integer" }, "screenshot": { "type": "boolean", "default": false } }),
        ),
        tool_req(
            "sim_act",
            "Run steps in order and report what each changed (settle diff). Stops at the first failure. Steps: tap, swipe, type, button, rotate, wait, assert.",
            json!({
                "steps": { "type": "array", "items": {
                    "type": "object",
                    "description": "Exactly one of the keys.",
                    "properties": {
                        "tap": target,
                        "swipe": { "type": "object", "properties": { "from": point, "to": point, "duration_ms": { "type": "integer" } }, "required": ["from", "to"] },
                        "type": { "type": "object", "properties": { "text": { "type": "string" }, "target": target }, "required": ["text"] },
                        "button": { "type": "string", "enum": ["home", "lock", "siri", "side-button", "app-switcher", "back", "volume-up", "volume-down"] },
                        "rotate": { "type": "string", "enum": ["portrait", "landscape-left", "landscape-right", "upside-down"] },
                        "wait": { "type": "object", "properties": { "target": target, "gone": { "type": "boolean" }, "timeout_ms": { "type": "integer" } }, "required": ["target"] },
                        "assert": { "type": "object", "properties": { "target": target, "check": { "anyOf": [
                            { "type": "string", "enum": ["exists", "gone", "enabled"] },
                            { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }
                        ] } }, "required": ["target"] }
                    }
                } },
                "settle": { "type": "boolean", "default": true }
            }),
            &["steps"],
        ),
        tool_req(
            "sim_app",
            "Launch an app (bundle_id; relaunch restarts it), install one (path to .app or .apk), or open a URL (url) on the device.",
            json!({
                "action": { "type": "string", "enum": ["launch", "install", "open_url"] },
                "bundle_id": { "type": "string" }, "relaunch": { "type": "boolean" }, "path": { "type": "string" }, "url": { "type": "string" }
            }),
            &["action"],
        ),
        tool(
            "sim_screenshot",
            "A screenshot, 1 px per point (tap coordinates read straight off it), as an image, with the file it was saved to.",
            json!({ "output": { "type": "string", "enum": ["image", "path"], "default": "image" } }),
        ),
        tool(
            "sim_wait_consent",
            "Wait (up to timeout_s, at most 20) for the user to answer this client's request in OxiMux. Call again while it is still waiting.",
            json!({ "timeout_s": { "type": "integer", "default": 20 } }),
        ),
    ])
}

fn tool(name: &str, description: &str, properties: Value) -> Value {
    tool_req(name, description, properties, &[])
}

fn tool_req(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({ "name": name, "description": description, "inputSchema": { "type": "object", "properties": properties, "required": required } })
}

/// `tools/call`: the result, errors included (`isError`).
pub(super) async fn call(ctx: &Ctx, name: &str, args: Value) -> Value {
    let deadline = Instant::now() + BUDGET;
    let outcome = match tokio::time::timeout(BUDGET, run(ctx, name, &args, deadline)).await {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "stopped waiting after {} s, this call's limit: a look or a wait was stopped; a step that changes the device (an install, a launch) may still finish. Call sim_observe to see where the device is",
            BUDGET.as_secs()
        )),
    };
    match outcome {
        Ok(content) => json!({ "content": content, "isError": false }),
        Err(text) => json!({ "content": [text_item(&text)], "isError": true }),
    }
}

async fn run(ctx: &Ctx, name: &str, args: &Value, deadline: Instant) -> Result<Vec<Value>, String> {
    let client = Client::connect_mcp(ctx.dir.clone(), BUDGET.as_secs()).await.map_err(|f| failure_text(&f))?;
    client
        .require_version(oximux_remote_proto::proto::SIMULATOR_MCP_MIN_VERSION, "oximux mcp")
        .map_err(|f| failure_text(&f))?;
    let flag = |key: &str| args.get(key).and_then(Value::as_bool);
    match name {
        "sim_status" => Ok(vec![text_item(&sim(&client, ctx, words(["status"])).await?.1)]),
        "sim_observe" => {
            let mut w = words(["ax"]);
            if flag("interactive").unwrap_or(true) {
                w.push("-i".into());
            }
            if let Some(max) = args.get("max").and_then(Value::as_u64) {
                w.extend(["--max".into(), max.to_string()]);
            }
            let mut out = vec![text_item(&sim(&client, ctx, w).await?.1)];
            if flag("screenshot").unwrap_or(false) {
                out.extend(screenshot(&client, ctx, true).await?);
            }
            Ok(out)
        }
        "sim_act" => act(&client, ctx, args, deadline).await.map(|text| vec![text_item(&text)]),
        "sim_app" => Ok(vec![text_item(&sim(&client, ctx, app_words(args, &ctx.worktree)?).await?.1)]),
        "sim_screenshot" => screenshot(&client, ctx, args.get("output").and_then(Value::as_str) != Some("path")).await,
        "sim_wait_consent" => {
            let secs = args.get("timeout_s").and_then(Value::as_u64).unwrap_or(MAX_CONSENT_WAIT).clamp(1, MAX_CONSENT_WAIT);
            Ok(vec![text_item(&sim(&client, ctx, vec!["wait-consent".into(), "--max-wait".into(), secs.to_string()]).await?.1)])
        }
        other => Err(format!("no tool `{other}`")),
    }
}

fn words<const N: usize>(w: [&str; N]) -> Vec<String> {
    w.iter().map(|s| (*s).to_owned()).collect()
}

/// One `sim` verb, as the CLI runs it: parsed, version-gated, run.
async fn sim(client: &Client, ctx: &Ctx, words: Vec<String>) -> Result<(Value, String), String> {
    let command = Words::try_parse_from(&words).map_err(|e| format!("bad step `{}`: {}", words.join(" "), first_line(&e.to_string())))?.command;
    let command = Command::Sim { worktree: Some(ctx.worktree.clone()), command };
    if let Some((needed, verb)) = crate::required_version(&command) {
        client.require_version(needed, verb).map_err(|f| failure_text(&f))?;
    }
    let Command::Sim { worktree, command } = command else { unreachable!("built just above") };
    super::super::sim::run(client, worktree, command).await.map_err(|f| failure_text(&f))
}

/// Each step's words and text, in order; the first failure ends the run,
/// its text after what the steps before it did.
async fn act(client: &Client, ctx: &Ctx, args: &Value, deadline: Instant) -> Result<String, String> {
    let steps = args.get("steps").and_then(Value::as_array).filter(|s| !s.is_empty()).ok_or("`steps` is a non-empty list")?;
    let settle = args.get("settle").and_then(Value::as_bool).unwrap_or(true);
    let mut done = Vec::new();
    for (n, step) in steps.iter().enumerate() {
        let n = n + 1;
        let fail = |done: &[String], why: String| {
            let mut out = done.to_vec();
            out.push(format!("step {n} failed: {why}"));
            out.join("\n")
        };
        let runs = step_words(step, settle).map_err(|why| fail(&done, why))?;
        if let Some(ms) = step.pointer("/wait/timeout_ms").and_then(Value::as_u64).or(step.get("wait").map(|_| 10_000)) {
            let left = deadline.saturating_duration_since(Instant::now()).saturating_sub(WAIT_SLACK);
            if Duration::from_millis(ms) > left {
                return Err(fail(&done, format!(
                    "this wait ({ms} ms) does not fit in the {} ms left of this call: split the wait across calls",
                    left.as_millis()
                )));
            }
        }
        for words in runs {
            let verb = words[0].clone();
            // Within what is left of the call: running out keeps the steps
            // already done in the answer, so none is repeated blindly.
            let ran = tokio::time::timeout_at(deadline - WAIT_SLACK, sim(client, ctx, words)).await.map_err(|_| {
                fail(&done, "it did not finish within this call's time; it may still complete on the device. Call sim_observe before going on".into())
            })?;
            let (_, text) = ran.map_err(|why| fail(&done, why))?;
            done.push(format!("{n}. {verb}: {text}"));
        }
    }
    Ok(done.join("\n"))
}

/// The CLI words for one step (a `type` into a labelled element is a tap,
/// then the typing).
fn step_words(step: &Value, settle: bool) -> Result<Vec<Vec<String>>, String> {
    let object = step.as_object().filter(|o| o.len() == 1).ok_or("a step is an object with exactly one key")?;
    let (verb, body) = object.iter().next().expect("one key");
    let settled = |mut w: Vec<String>| {
        if settle {
            w.push("--settle".into());
        }
        w
    };
    Ok(match verb.as_str() {
        "tap" => vec![settled([vec!["tap".into()], target_words(body, true)?].concat())],
        "swipe" => {
            let p = |key: &str, axis: &str| body.pointer(&format!("/{key}/{axis}")).and_then(Value::as_f64).ok_or(format!("swipe needs {key}.{axis}"));
            let mut w = vec!["swipe".into(), num(p("from", "x")?), num(p("from", "y")?), num(p("to", "x")?), num(p("to", "y")?)];
            if let Some(ms) = body.get("duration_ms").and_then(Value::as_u64) {
                w.extend(["--duration".into(), ms.to_string()]);
            }
            vec![settled(w)]
        }
        "type" => {
            let text = body.get("text").and_then(Value::as_str).ok_or("type needs text")?.to_owned();
            // Flags before `--`: after it, every word is the text.
            let typing = |w: Vec<String>| {
                let mut w = settled(w);
                if text.starts_with('-') {
                    w.push("--".into());
                }
                w.push(text.clone());
                w
            };
            match body.get("target") {
                None => vec![typing(vec!["type".into()])],
                Some(t) if t.get("ref").is_some() => vec![typing(vec!["type".into(), reference(&t["ref"])?])],
                Some(t) => vec![[vec!["tap".into()], target_words(t, true)?].concat(), typing(vec!["type".into()])],
            }
        }
        "button" => vec![settled(vec!["button".into(), body.as_str().ok_or("button is a name")?.into()])],
        "rotate" => vec![settled(vec!["rotate".into(), body.as_str().ok_or("rotate is an orientation")?.into()])],
        "wait" => {
            let mut w = [vec!["wait".into()], target_words(body.get("target").ok_or("wait needs a target")?, false)?].concat();
            if body.get("gone").and_then(Value::as_bool) == Some(true) {
                w.push("--gone".into());
            }
            if let Some(ms) = body.get("timeout_ms").and_then(Value::as_u64) {
                w.extend(["--timeout-ms".into(), ms.to_string()]);
            }
            vec![w]
        }
        "assert" => {
            let mut w = [vec!["assert".into()], target_words(body.get("target").ok_or("assert needs a target")?, false)?].concat();
            match body.get("check") {
                None => {}
                Some(Value::String(c)) if c == "exists" => {}
                Some(Value::String(c)) if c == "gone" || c == "enabled" => w.push(format!("--{c}")),
                Some(c) if c.get("text").and_then(Value::as_str).is_some() => w.push(format!("--text={}", c["text"].as_str().unwrap_or_default())),
                Some(other) => return Err(format!("check is \"exists\", \"gone\", \"enabled\" or {{\"text\": …}}, not {other}")),
            }
            vec![w]
        }
        other => return Err(format!("no step `{other}` (tap, swipe, type, button, rotate, wait, assert)")),
    })
}

/// A target's words: a ref, `--label=` (+ `--exact`), `--id=`, or (a tap's
/// only) `X Y`. Values ride `--flag=value`, so one that starts with `-` is
/// never read as a flag.
fn target_words(t: &Value, point_ok: bool) -> Result<Vec<String>, String> {
    if let Some(r) = t.get("ref") {
        return Ok(vec![reference(r)?]);
    }
    if let Some(label) = t.get("label").and_then(Value::as_str) {
        let mut w = vec![format!("--label={label}")];
        if t.get("exact").and_then(Value::as_bool) == Some(true) {
            w.push("--exact".into());
        }
        return Ok(w);
    }
    if let Some(id) = t.get("id").and_then(Value::as_str) {
        return Ok(vec![format!("--id={id}")]);
    }
    match (t.get("x").and_then(Value::as_f64), t.get("y").and_then(Value::as_f64)) {
        (Some(x), Some(y)) if point_ok => Ok(vec![num(x), num(y)]),
        (Some(_), Some(_)) => Err("wait and assert name an element (ref, label or id), not a point".into()),
        _ => Err("a target is one of: ref, label, id, or x and y".into()),
    }
}

/// `@e3` from `"@e3"`, `"e3"` or `3`.
fn reference(r: &Value) -> Result<String, String> {
    let n = match r {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim_start_matches('@').trim_start_matches('e').parse().ok(),
        _ => None,
    };
    n.map(|n| format!("@e{n}")).ok_or_else(|| format!("`{r}` is not a ref (\"@e3\")"))
}

fn num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { x.to_string() }
}

/// `sim_app`'s words; a relative install path is the worktree's.
fn app_words(args: &Value, worktree: &Path) -> Result<Vec<String>, String> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_owned).ok_or(format!("`{key}` is required"));
    Ok(match args.get("action").and_then(Value::as_str) {
        Some("launch") => {
            let mut w = vec!["launch".into()];
            if args.get("relaunch").and_then(Value::as_bool) == Some(true) {
                w.push("--relaunch".into());
            }
            // `--`: the value is never a flag.
            w.extend(["--".into(), text("bundle_id")?]);
            w
        }
        Some("install") => {
            let path = text("path")?;
            let path = if Path::new(&path).is_absolute() { path } else { worktree.join(path).to_string_lossy().into_owned() };
            vec!["install".into(), "--".into(), path]
        }
        Some("open_url") => vec!["open-url".into(), "--".into(), text("url")?],
        _ => return Err("action is launch, install or open_url".into()),
    })
}

/// The default screenshot (1 px per point), saved where `oximux sim
/// screenshot` saves; as an image unless only the path is wanted.
async fn screenshot(client: &Client, ctx: &Ctx, image: bool) -> Result<Vec<Value>, String> {
    let (data, text) = sim(client, ctx, words(["screenshot"])).await?;
    let mut out = vec![text_item(&text)];
    if image && let Some(path) = data.get("path").and_then(Value::as_str) {
        let png = tokio::fs::read(path).await.map_err(|e| format!("cannot read the screenshot {path}: {e}"))?;
        out.insert(0, json!({ "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(png), "mimeType": "image/png" }));
    }
    Ok(out)
}

fn text_item(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or(s).trim_start_matches("error: ")
}

/// A failure as the agent should read it: the CLI's own words, with the
/// consent steps named as this server's tools.
fn failure_text(f: &Failure) -> String {
    match f.code {
        "consent-pending" => "Waiting for the user to allow this MCP client to control the device in OxiMux. Call sim_wait_consent, then retry.".into(),
        "timeout" if f.message.contains("answer") => "The user has not answered yet. Call sim_wait_consent again.".into(),
        "not-asked" => "Nothing is waiting for the user's answer: call the tool you need (it asks the user), then sim_wait_consent.".into(),
        _ => {
            let steps = f.next_steps.iter().map(|s| format!("\nNext step: {s}")).collect::<String>();
            format!("error: {}{steps}", f.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(step: Value, settle: bool) -> Vec<String> {
        step_words(&step, settle).unwrap().into_iter().map(|w| w.join(" ")).collect()
    }

    #[test]
    fn steps_become_the_clis_words() {
        assert_eq!(w(json!({ "tap": { "ref": "@e3" } }), true), ["tap @e3 --settle"]);
        assert_eq!(w(json!({ "tap": { "label": "Sign in", "exact": true } }), false), ["tap --label=Sign in --exact"]);
        assert_eq!(w(json!({ "tap": { "x": 120, "y": 44.5 } }), false), ["tap 120 44.5"]);
        assert_eq!(w(json!({ "type": { "text": "a@b.co", "target": { "ref": 2 } } }), true), ["type @e2 --settle a@b.co"]);
        assert_eq!(w(json!({ "type": { "text": "-1", "target": { "ref": 2 } } }), true), ["type @e2 --settle -- -1"]);
        assert_eq!(w(json!({ "type": { "text": "-1", "target": { "label": "Qty" } } }), false), ["tap --label=Qty", "type -- -1"]);
        assert_eq!(w(json!({ "swipe": { "from": { "x": 1, "y": 2 }, "to": { "x": 3, "y": 4 }, "duration_ms": 500 } }), false), ["swipe 1 2 3 4 --duration 500"]);
        assert_eq!(w(json!({ "wait": { "target": { "label": "Home" }, "gone": true, "timeout_ms": 3000 } }), true), ["wait --label=Home --gone --timeout-ms 3000"]);
        assert_eq!(w(json!({ "assert": { "target": { "id": "title" }, "check": { "text": "Hi" } } }), true), ["assert --id=title --text=Hi"]);
        assert_eq!(w(json!({ "button": "home" }), true), ["button home --settle"]);
        for bad in [json!({ "tap": {} }), json!({ "wait": { "target": { "x": 1, "y": 2 } } }), json!({ "fly": 1 }), json!({ "tap": {}, "type": {} })] {
            assert!(step_words(&bad, true).is_err(), "{bad}");
        }
    }

    /// Every step's words parse with the CLI's own parser.
    #[test]
    fn each_steps_words_parse_as_the_cli_does() {
        let steps = [
            json!({ "tap": { "ref": 3 } }),
            json!({ "type": { "text": "hello", "target": { "ref": 1 } } }),
            json!({ "type": { "text": "-5", "target": { "ref": 1 } } }),
            json!({ "type": { "text": "--settle" } }),
            json!({ "swipe": { "from": { "x": 1, "y": 2 }, "to": { "x": 3, "y": 4 } } }),
            json!({ "wait": { "target": { "ref": "e4" } } }),
            json!({ "assert": { "target": { "label": "Pay" }, "check": "enabled" } }),
            json!({ "rotate": "landscape-left" }),
        ];
        for step in steps {
            for words in step_words(&step, true).unwrap() {
                assert!(Words::try_parse_from(&words).is_ok(), "{words:?}");
            }
        }
        let launch = app_words(&json!({ "action": "launch", "bundle_id": "com.a.b", "relaunch": true }), Path::new("/w")).unwrap();
        assert!(Words::try_parse_from(&launch).is_ok(), "{launch:?}");
        assert_eq!(app_words(&json!({ "action": "install", "path": "build/A.app" }), Path::new("/w")).unwrap()[2], "/w/build/A.app");
        // A value that looks like a flag stays a value.
        for step in [json!({ "tap": { "label": "--help" } }), json!({ "assert": { "target": { "id": "-x" }, "check": { "text": "--worktree=/etc" } } })] {
            for words in step_words(&step, true).unwrap() {
                let parsed = Words::try_parse_from(&words).unwrap_or_else(|e| panic!("{words:?}: {e}"));
                assert!(!matches!(parsed.command, SimCommand::Status), "{words:?}");
            }
        }
        for args in [json!({ "action": "open_url", "url": "--help" }), json!({ "action": "launch", "bundle_id": "-x" })] {
            assert!(Words::try_parse_from(app_words(&args, Path::new("/w")).unwrap()).is_ok(), "{args}");
        }
    }

    #[test]
    fn six_tools_none_destructive_and_short() {
        let tools = list();
        let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["sim_status", "sim_observe", "sim_act", "sim_app", "sim_screenshot", "sim_wait_consent"]);
        assert!(tools.as_array().unwrap().iter().all(|t| t["description"].as_str().unwrap().len() <= 300));
        let schema = tools.to_string();
        for verb in ["erase", "push", "exec"] {
            assert!(!schema.contains(&format!("\"{verb}\"")), "{verb} is not exposed");
        }
        // About 4 characters a token: within the 2.5k-token budget.
        assert!(schema.len() < 10_000, "{} chars", schema.len());
    }

    #[test]
    fn consent_is_explained_in_this_servers_terms() {
        let pending = Failure::new("consent-pending", crate::cli::exit::PENDING, "waiting");
        assert!(failure_text(&pending).contains("sim_wait_consent"));
        let other = Failure::new("stale-ref", crate::cli::exit::ERROR, "@e9 is gone").with_steps(["look again".into()]);
        assert_eq!(failure_text(&other), "error: @e9 is gone\nNext step: look again");
    }
}
