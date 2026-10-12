//! `oximux sim run` and `oximux sim flow`: one text format for a batch, a
//! recording, and an export (see [`steps`]). A run and an export are the
//! CLI's alone; a recording is the host's (it sees every verb the agent
//! sends), started, read and stopped through `SimCmdWire::Flow`.

pub mod maestro;
pub mod run;
pub mod steps;

use std::path::Path;
use std::time::Duration;

use oximux_remote_proto::simulator::{SimCmdWire, SimFlowCmdWire, SimFlowWire, SimReplyWire};
use serde_json::{Value, json};

use crate::cli::{FlowCommand, exit};
use crate::client::Client;
use crate::commands::sim::{Outcome, call, unexpected};
use crate::output::Failure;

/// `flow start`, `flow stop`, `flow status` (`flow export` needs no host).
pub async fn flow(client: &Client, worktree: &Path, command: FlowCommand) -> Outcome {
    let (cmd, out) = match command {
        FlowCommand::Start => (SimFlowCmdWire::Start, None),
        FlowCommand::Status => (SimFlowCmdWire::Status, None),
        FlowCommand::Stop { out } => (SimFlowCmdWire::Stop { save: out.is_none() }, out),
        FlowCommand::Export { file, out, app_id, .. } => return export(&file, out.as_deref(), app_id.as_deref()),
    };
    let starting = cmd == SimFlowCmdWire::Start;
    let SimReplyWire::Flow(flow) = call(client, worktree, SimCmdWire::Flow(cmd), Duration::ZERO).await? else {
        return Err(unexpected("Flow"));
    };
    let mut value = flow_json(&flow);
    let mut human = if starting {
        "recording this worktree's agent steps — `oximux sim flow stop` ends it".to_owned()
    } else if flow.recording {
        format!("recording: {} step{} so far\n{}", flow.count, plural(flow.count), flow.steps.trim_end())
    } else {
        format!("{} step{}", flow.count, plural(flow.count))
    };
    if let Some(path) = out {
        write_private(&path, &flow.steps)?;
        human = format!("wrote {} step{} to {}", flow.count, plural(flow.count), path.display());
        value["path"] = json!(path);
    } else if let Some(saved) = &flow.saved {
        human = format!("saved {} step{} to {saved}\nreplay: oximux sim run {}", flow.count, plural(flow.count), shell_path(saved));
    }
    for warning in &flow.warnings {
        human.push_str(&format!("\nwarning: {warning}"));
    }
    Ok((value, human))
}

/// `flow export FILE --maestro`: the YAML on stdout (or in --out), the
/// warnings on stderr.
pub fn export(file: &Path, out: Option<&Path>, app_id: Option<&str>) -> Outcome {
    let src = steps::read(file)?;
    let (header, all) = steps::parse(&src)?;
    let mut inputs = Vec::new();
    for step in &all {
        let command = steps::command(&step.words)
            .map_err(|why| Failure::new("usage", exit::USAGE, format!("step {} (line {}): {why}: {}", step.n, step.line, step.text)))?;
        inputs.push(maestro::Input { n: step.n, text: &step.text, cmd: steps::wire(command).ok() });
    }
    let exported = maestro::export(&header, &inputs, app_id).map_err(|why| Failure::new("export", exit::ERROR, why))?;
    for warning in &exported.warnings {
        eprintln!("warning: {warning}");
    }
    if !exported.warnings.is_empty() {
        eprintln!("{} warning{}", exported.warnings.len(), plural(exported.warnings.len() as u32));
    }
    let value = json!({ "yaml": exported.yaml, "warnings": exported.warnings });
    match out {
        Some(path) => {
            std::fs::write(path, &exported.yaml)
                .map_err(|e| Failure::new("write", exit::ERROR, format!("cannot write {}: {e}", path.display())))?;
            Ok((value, format!("wrote {}", path.display())))
        }
        None => Ok((value, exported.yaml.trim_end().to_owned())),
    }
}

fn flow_json(flow: &SimFlowWire) -> Value {
    json!({
        "recording": flow.recording,
        "count": flow.count,
        "steps": flow.steps,
        "warnings": flow.warnings,
        "saved": flow.saved,
    })
}

/// A flow file can hold what the user typed: the owner's alone.
fn write_private(path: &Path, text: &str) -> Result<(), Failure> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let failed = |e: std::io::Error| Failure::new("write", exit::ERROR, format!("cannot write {}: {e}", path.display()));
    let mut file = options.open(path).map_err(failed)?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(failed)
}

fn shell_path(path: &str) -> String {
    oximux_remote_proto::simulator::words::quote(path).unwrap_or_else(|_| path.to_owned())
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}
