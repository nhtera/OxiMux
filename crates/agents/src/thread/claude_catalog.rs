//! The installed `claude`'s own model catalog, asked for over stream-json.
//!
//! The CLI has no `models` subcommand, but it answers a `list_models` control
//! request on stdin with the same rows its `/model` picker shows — wire value,
//! display name, description, effort levels, fast-mode support — without
//! starting an API turn. That reply is the source of truth for the chat's model
//! picker; the static list in `claude_stream_json.rs` is the seed shown until a
//! probe lands and the fallback when one never does.
//!
//! Everything here is gpui-free: the request/args builders and the parser are
//! pure, the probe is a blocking subprocess call meant for a plain
//! `std::thread`, and the shared slot lets the live connection (and through it
//! the session registry and the phone) read the catalog with no plumbing.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::connect::ProbedCatalog;
use super::connection::ModelChoice;

/// One pickable row of the CLI's `/model` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeListedModel {
    /// The `--model` value, exactly as the CLI's own picker sends it
    /// (`opus[1m]`, `claude-fable-5-1[1m]`, `sonnet`).
    pub wire: String,
    /// The picker name (`Opus (1M context)`, `Fable`).
    pub label: String,
    /// The muted blurb (`Fable 5.1 · Most capable for …`).
    pub description: Option<String>,
    /// The concrete model id the alias resolves to (`claude-opus-5[1m]`).
    pub resolved: Option<String>,
    /// `supportedEffortLevels` when `supportsEffort`; empty otherwise (Haiku).
    pub effort_levels: Vec<String>,
    /// `supportsFastMode` — drives the composer's fast-mode toggle.
    pub supports_fast_mode: bool,
}

/// The parsed catalog: the pickable rows plus which of them the CLI's
/// `Default (recommended)` row resolves to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCatalog {
    pub models: Vec<ClaudeListedModel>,
    /// The wire of the listed row that shares the `default` row's
    /// `resolvedModel`. `None` when the reply had no default row or nothing
    /// matched it.
    pub default_wire: Option<String>,
}

impl ClaudeCatalog {
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// The rows as picker choices.
    pub fn model_choices(&self) -> Vec<ModelChoice> {
        self.models
            .iter()
            .map(|m| ModelChoice {
                wire: m.wire.clone(),
                label: m.label.clone(),
                description: m.description.clone(),
            })
            .collect()
    }

    /// The row for `wire`, if listed.
    pub fn model(&self, wire: &str) -> Option<&ClaudeListedModel> {
        self.models.iter().find(|m| m.wire == wire)
    }

    /// The effort levels the CLI offers for `wire`. `None` when the wire is
    /// not a catalog row (the caller falls back to its static list); an empty
    /// `Some` means the model takes no effort flag at all (Haiku).
    pub fn effort_levels_for(&self, wire: &str) -> Option<&[String]> {
        self.model(wire).map(|m| m.effort_levels.as_slice())
    }

    /// Whether the CLI marks `wire` as accepting fast mode.
    pub fn supports_fast_mode(&self, wire: &str) -> bool {
        self.model(wire).is_some_and(|m| m.supports_fast_mode)
    }
}

impl From<&ClaudeCatalog> for ProbedCatalog {
    fn from(c: &ClaudeCatalog) -> Self {
        ProbedCatalog { models: c.model_choices(), default_model: c.default_wire.clone() }
    }
}

/// The stdin line that asks the CLI for its model list. A fresh uuid per
/// request, like the CLI's own SDK; nothing correlates the reply because the
/// probe reads stdout to EOF and takes the first well-formed answer.
pub fn list_models_request_json() -> Value {
    json!({
        "type": "control_request",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request": {"subtype": "list_models"},
    })
}

/// argv for the probe process.
///
/// `--setting-sources ""` is deliberate: a probe must not run the user's
/// SessionStart hooks (ten hook lines observed without it), and OxiMux's own
/// status hook would otherwise paint a phantom session on the rail. Verified
/// accepted on 2.1.245 and 2.1.260. No `--permission-prompt-tool`, no
/// `--include-partial-messages`: nothing here starts a turn.
pub fn list_models_args() -> Vec<String> {
    [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--setting-sources",
        "",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Parse the probe's stdout into a catalog.
///
/// Scans lines for the first `control_response` whose `response.subtype` is
/// `success` and whose payload carries a `models` array. Anything else — an
/// `error` subtype from a CLI that predates `list_models`, hook chatter, an
/// empty stream — yields an empty catalog, so the caller's seed-preservation
/// rule applies rather than a failure.
///
/// Rows are filtered and reshaped the way the CLI's own picker does it:
/// - `disabled` rows and rows with an empty `value` are dropped (2.1.245 lists
///   a disabled `cc-update-required-1` placeholder);
/// - the `default` row is not a pickable model. It is removed, and the listed
///   row sharing its `resolvedModel` becomes [`ClaudeCatalog::default_wire`];
/// - duplicates by wire keep their first occurrence. Two rows may share a
///   label (`Fable` twice on 2.1.245); the descriptions differ, so both stay.
pub fn parse_list_models(stdout: &str) -> ClaudeCatalog {
    let Some(rows) = stdout.lines().find_map(list_models_rows) else {
        return ClaudeCatalog::default();
    };
    let mut default_resolved: Option<String> = None;
    let mut models: Vec<ClaudeListedModel> = Vec::new();
    for row in &rows {
        let wire = row["value"].as_str().unwrap_or("").trim();
        if wire.is_empty() || row["disabled"].as_bool().unwrap_or(false) {
            continue;
        }
        let resolved = row["resolvedModel"].as_str().map(str::to_string);
        if wire == "default" {
            default_resolved = resolved;
            continue;
        }
        if models.iter().any(|m| m.wire == wire) {
            continue;
        }
        let effort_levels = if row["supportsEffort"].as_bool().unwrap_or(false) {
            row["supportedEffortLevels"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        models.push(ClaudeListedModel {
            wire: wire.to_string(),
            label: row["displayName"].as_str().unwrap_or(wire).to_string(),
            description: row["description"].as_str().map(str::to_string),
            resolved,
            effort_levels,
            supports_fast_mode: row["supportsFastMode"].as_bool().unwrap_or(false),
        });
    }
    let default_wire = default_resolved.and_then(|target| {
        models.iter().find(|m| m.resolved.as_deref() == Some(target.as_str())).map(|m| m.wire.clone())
    });
    ClaudeCatalog { models, default_wire }
}

/// The `models` array of one line, if that line is a successful `list_models`
/// reply.
fn list_models_rows(line: &str) -> Option<Vec<Value>> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v["type"].as_str()? != "control_response" {
        return None;
    }
    let response = &v["response"];
    if response["subtype"].as_str()? != "success" {
        return None;
    }
    response["response"]["models"].as_array().cloned()
}

/// Upper bound on one probe. Measured 1.5s to 5.9s on this host across two CLI
/// builds; the bound only exists so a wedged CLI cannot pin the probe thread.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Ask the installed `claude` for its model catalog.
///
/// Spawns the CLI through the same PATH resolution the chat session uses (a
/// GUI launch inherits no shell PATH), writes one request line, closes stdin so
/// the CLI exits at EOF, and reads stdout to the end under a deadline. Blocking
/// by design, like `probe_catalog`: callers run it on a plain `std::thread`.
///
/// An old CLI that does not know `list_models` answers with an `error` subtype
/// and exits 0; that is `Ok(empty)`, not an error, so the caller keeps whatever
/// seed it has. `Err` is reserved for a CLI that could not be spawned, hung
/// past the deadline, or produced unreadable output.
pub fn probe_claude_catalog() -> Result<ClaudeCatalog> {
    let mut cmd = Command::new(crate::cli::program_for_spawn("claude"));
    cmd.args(list_models_args());
    probe_command(cmd)
}

/// [`probe_claude_catalog`] over an already-built command, so tests can drive
/// it with a fake CLI.
pub fn probe_command(mut cmd: Command) -> Result<ClaudeCatalog> {
    use oximux_no_window::NoWindow as _;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).no_window();
    let mut child = cmd.spawn().context("spawn claude for list_models")?;
    let mut stdin = child.stdin.take().context("claude stdin missing")?;
    let mut stdout = child.stdout.take().context("claude stdout missing")?;

    // Write the one request and close the pipe: the CLI reads stdin as a stream
    // and exits at EOF, which is what ends the probe.
    let request = list_models_request_json();
    // A CLI that exits before reading (a broken install) closes the pipe under
    // us; the read below then sees EOF and the parse yields empty, which is the
    // right answer. So the write error is not fatal on its own.
    let _ = writeln!(stdin, "{request}").and_then(|()| stdin.flush());
    drop(stdin);

    // Read on a helper thread so the deadline can be enforced here: a blocking
    // `read_to_string` has no timeout of its own.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let read = stdout.read_to_string(&mut out).map(|_| out);
        let _ = tx.send(read);
    });
    let started = Instant::now();
    let output = match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::Error::new(e).context("read claude list_models reply"));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("claude list_models probe timed out after {:?}", started.elapsed());
        }
    };
    // stdout is closed, so the CLI is exiting or gone; reap it so the probe
    // never leaves a zombie behind. Its exit code carries no signal: an old
    // CLI answers `error` and still exits 0.
    let _ = child.wait();
    Ok(parse_list_models(&output))
}

// --- Shared slot ---------------------------------------------------------
// One process-wide catalog, published by whoever ran the probe (the desktop's
// catalog fold, `serve`) and read by every live Claude connection. A slot
// rather than a field on the connection because the connection that needs it
// most is the one already open when the probe lands.

fn slot() -> &'static RwLock<Option<Arc<ClaudeCatalog>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<ClaudeCatalog>>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// The most recently published catalog, if any.
pub fn shared_claude_catalog() -> Option<Arc<ClaudeCatalog>> {
    slot().read().ok()?.clone()
}

/// Make `catalog` the one every Claude connection reads. An empty catalog is
/// ignored: the seed it would replace is better than nothing, and an old CLI's
/// `error` reply must not blank a list the desktop already painted.
pub fn publish_claude_catalog(catalog: ClaudeCatalog) {
    if catalog.is_empty() {
        return;
    }
    if let Ok(mut guard) = slot().write() {
        *guard = Some(Arc::new(catalog));
    }
}

/// Test-only: empty the slot so one test's publish cannot leak into another.
/// Tests that touch the slot also hold [`slot_test_lock`].
#[cfg(test)]
pub(crate) fn clear_claude_catalog_for_test() {
    if let Ok(mut guard) = slot().write() {
        *guard = None;
    }
}

/// Serialises every test that reads or writes the process-wide slot. A `static`
/// shared across `cargo test`'s parallel threads needs one, or a publish in one
/// test is observed by another mid-assertion.
#[cfg(test)]
pub(crate) fn slot_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

#[cfg(test)]
pub(crate) const FIXTURE_2_1_260: &str = include_str!("testdata/claude_list_models_2_1_260.jsonl");
#[cfg(test)]
pub(crate) const FIXTURE_2_1_245: &str = include_str!("testdata/claude_list_models_2_1_245.jsonl");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_current_cli_reply() {
        let c = parse_list_models(FIXTURE_2_1_260);
        let wires: Vec<&str> = c.models.iter().map(|m| m.wire.as_str()).collect();
        assert_eq!(wires, vec!["opus[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku"]);
        // The `default` row is gone from the list but names the preselection.
        assert_eq!(c.default_wire.as_deref(), Some("opus[1m]"));
        let opus = c.model("opus[1m]").unwrap();
        assert_eq!(opus.label, "Opus (1M context)");
        assert!(opus.supports_fast_mode);
        assert_eq!(opus.effort_levels, vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(opus.resolved.as_deref(), Some("claude-opus-5[1m]"));
        // Haiku takes no effort flag and no fast mode.
        let haiku = c.model("haiku").unwrap();
        assert!(haiku.effort_levels.is_empty());
        assert!(!haiku.supports_fast_mode);
        assert_eq!(c.effort_levels_for("haiku"), Some(&[][..]));
        assert_eq!(c.effort_levels_for("sonnet").map(|e| e.len()), Some(5));
        assert_eq!(c.effort_levels_for("gpt"), None);
        let fable = c.model("claude-fable-5-1[1m]").unwrap();
        assert!(fable.description.as_deref().unwrap().starts_with("Fable 5.1 ·"));
    }

    #[test]
    fn parses_the_older_cli_reply() {
        let c = parse_list_models(FIXTURE_2_1_245);
        let wires: Vec<&str> = c.models.iter().map(|m| m.wire.as_str()).collect();
        // Disabled placeholder and the `default` row are gone; both Fable
        // wires stay even though they share a label.
        assert_eq!(
            wires,
            vec!["opus[1m]", "claude-fable-5[1m]", "claude-fable-5-1[1m]", "sonnet", "haiku"]
        );
        assert!(!wires.contains(&"cc-update-required-1"));
        assert_eq!(c.default_wire.as_deref(), Some("opus[1m]"));
        assert_eq!(c.models.iter().filter(|m| m.label == "Fable").count(), 2);
    }

    #[test]
    fn unknown_request_and_garbage_yield_empty() {
        // A CLI that predates `list_models` answers with an error subtype.
        let old = r#"{"type":"control_response","response":{"subtype":"error","request_id":"x","error":"Unknown request subtype"}}"#;
        assert!(parse_list_models(old).is_empty());
        assert!(parse_list_models("").is_empty());
        assert!(parse_list_models("not json\n{\"type\":\"system\"}\n").is_empty());
        // A success reply with no models array is also nothing.
        let hollow = r#"{"type":"control_response","response":{"subtype":"success","request_id":"x","response":{}}}"#;
        assert!(parse_list_models(hollow).is_empty());
        assert_eq!(parse_list_models(old).default_wire, None);
    }

    #[test]
    fn hook_chatter_before_the_reply_is_skipped() {
        let noisy = format!(
            "{}\n{}",
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart"}"#,
            FIXTURE_2_1_260
        );
        assert_eq!(parse_list_models(&noisy).models.len(), 4);
    }

    #[test]
    fn rows_without_a_resolved_default_have_no_default_wire() {
        let reply = json!({"type":"control_response","response":{"subtype":"success","request_id":"x",
            "response":{"models":[
                {"value":"default","resolvedModel":"something-unlisted","displayName":"Default"},
                {"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet"},
                {"value":"sonnet","resolvedModel":"claude-sonnet-5","displayName":"Sonnet again"},
                {"value":"","displayName":"blank"}
            ]}}})
        .to_string();
        let c = parse_list_models(&reply);
        assert_eq!(c.default_wire, None);
        assert_eq!(c.models.len(), 1, "duplicate wire and blank wire dropped");
        assert_eq!(c.models[0].label, "Sonnet", "first occurrence wins");
    }

    #[test]
    fn request_and_args_shape() {
        let req = list_models_request_json();
        assert_eq!(req["type"], "control_request");
        assert_eq!(req["request"]["subtype"], "list_models");
        assert!(req["request_id"].as_str().is_some_and(|s| !s.is_empty()));

        let args = list_models_args();
        let i = args.iter().position(|a| a == "--setting-sources").expect("flag present");
        assert_eq!(args[i + 1], "", "hooks must not run on a probe");
        assert!(args.iter().any(|a| a == "stream-json"));
        assert!(!args.iter().any(|a| a == "--permission-prompt-tool"));
    }

    #[test]
    fn probed_catalog_conversion_carries_default() {
        let c = parse_list_models(FIXTURE_2_1_260);
        let p = ProbedCatalog::from(&c);
        assert_eq!(p.models.len(), 4);
        assert_eq!(p.default_model.as_deref(), Some("opus[1m]"));
        assert_eq!(p.models[0].label, "Opus (1M context)");
    }

    /// A fake CLI that echoes the fixture proves the spawn → write → read →
    /// parse path without a real `claude`.
    #[test]
    fn probe_command_reads_a_fake_cli() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/thread/testdata/claude_list_models_2_1_260.jsonl");
        let script = format!("cat >/dev/null; cat '{}'", path.display());
        let c = probe_command(crate::thread::sh_fixture::sh_script(&script)).expect("probe");
        assert_eq!(c.models.len(), 4);
        assert_eq!(c.default_wire.as_deref(), Some("opus[1m]"));
    }

    #[test]
    fn probe_command_treats_an_old_cli_as_empty() {
        let script = r#"cat >/dev/null; printf '%s\n' '{"type":"control_response","response":{"subtype":"error","request_id":"x","error":"nope"}}'"#;
        let c = probe_command(crate::thread::sh_fixture::sh_script(script)).expect("probe");
        assert!(c.is_empty(), "an old CLI is Ok(empty), not an error");
    }

    #[test]
    fn probe_command_fails_when_the_binary_is_missing() {
        let err = probe_command(Command::new("oximux-no-such-claude-binary-xyz"))
            .expect_err("spawn failure is an error");
        assert!(err.to_string().contains("spawn"), "{err}");
    }

    /// Live: the installed `claude` answers `list_models` with the same rows
    /// its `/model` picker shows. Ignored by default (spawns the real binary,
    /// ~2s); run with `cargo test -p oximux-agents -- --ignored live_probe`.
    #[test]
    #[ignore]
    fn live_probe_reads_the_installed_cli() {
        let c = probe_claude_catalog().expect("probe");
        assert!(!c.is_empty(), "the installed CLI should list models: {c:?}");
        assert!(c.default_wire.is_some(), "{c:?}");
        eprintln!("{c:#?}");
    }

    #[test]
    fn publish_ignores_empty_and_serves_the_last_catalog() {
        let _guard = slot_test_lock().lock().unwrap_or_else(|p| p.into_inner());
        clear_claude_catalog_for_test();
        assert!(shared_claude_catalog().is_none());
        publish_claude_catalog(ClaudeCatalog::default());
        assert!(shared_claude_catalog().is_none(), "an empty catalog is never published");
        publish_claude_catalog(parse_list_models(FIXTURE_2_1_260));
        assert_eq!(shared_claude_catalog().unwrap().models.len(), 4);
        publish_claude_catalog(ClaudeCatalog::default());
        assert_eq!(shared_claude_catalog().unwrap().models.len(), 4, "empty must not blank it");
        clear_claude_catalog_for_test();
    }
}
