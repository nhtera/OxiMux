//! `oximux sim run FILE`: the steps in order, each exactly as the same
//! command typed by hand, stopping at the first that fails.
//!
//! Every step is parsed, and checked against the host's version, before the
//! first one runs: a typo in step 9 must not leave a half-driven app behind.
//! Steps that change device state (privacy, install, erase, push, location)
//! are put to the user once, as a list, before step 1; verbs that always ask
//! (erase, push, a real device's state) still ask when their turn comes.

use std::path::Path;
use std::time::Duration;

use oximux_remote_proto::proto::SIMULATOR_FLOW_MIN_VERSION;
use oximux_remote_proto::simulator::{SimCmdWire, SimFlowCmdWire, SimReplyWire};
use serde_json::json;

use super::steps::{self, Step};
use crate::cli::{Command, RunArgs, SimCommand, exit};
use crate::client::Client;
use crate::commands::sim::{Outcome, call, unexpected};
use crate::output::Failure;

/// The user reads the list and answers (the host waits up to 90 s).
const APPROVAL: Duration = Duration::from_secs(120);

pub async fn run(client: &Client, worktree: &Path, args: RunArgs) -> Outcome {
    let src = steps::read(&args.file)?;
    let (_, all) = steps::parse(&src)?;
    if all.is_empty() {
        return Err(Failure::new("usage", exit::USAGE, format!("{} has no steps", args.file.display())));
    }
    if args.from == 0 || args.from > all.len() {
        return Err(Failure::new("usage", exit::USAGE, format!("--from {}: the file has steps 1 to {}", args.from, all.len())));
    }
    let mut todo = Vec::new();
    for step in &all[args.from - 1..] {
        let bad = |why: String| Failure::new("usage", exit::USAGE, format!("step {} (line {}): {why}: {}", step.n, step.line, step.text));
        // Parsed as written (placeholders and all), then the secrets put in
        // — a value never becomes a flag — then checked with them (a payload
        // path from a secret must exist), an error naming none of them.
        let filled = || -> Result<(SimCommand, Vec<String>), String> {
            let mut command = steps::command(&step.words)?;
            let secrets = steps::substitute(&mut command)?;
            Ok((command, secrets))
        };
        let (checked, secrets) = filled().map_err(bad)?;
        steps::check(checked).map_err(|why| bad(steps::redact(&why, &secrets)))?;
        let (mut command, secrets) = filled().map_err(bad)?;
        if args.settle
            && let Some(settle) = command.settle_args_mut()
        {
            settle.settle = true;
        }
        // The version a step needs, as if it were typed alone.
        let command = Command::Sim { worktree: None, command };
        if let Some((needed, verb)) = crate::required_version(&command) {
            client.require_version(needed, verb).map_err(|f| at(f, step, &args.file))?;
        }
        let Command::Sim { command, .. } = command else { unreachable!("built just above") };
        todo.push((step, command, secrets));
    }
    let changes: Vec<String> = todo.iter().filter(|(_, c, _)| changes_state(c)).map(|(s, _, _)| s.text.clone()).collect();
    if !changes.is_empty() {
        client.require_version(SIMULATOR_FLOW_MIN_VERSION, "sim run with steps that change device state")?;
        match call(client, worktree, SimCmdWire::Flow(SimFlowCmdWire::ApproveRun { lines: changes }), APPROVAL).await? {
            SimReplyWire::Done => {}
            _ => return Err(unexpected("Flow")),
        }
    }
    let (mut ran, mut human) = (Vec::new(), Vec::new());
    for (step, command, secrets) in todo {
        // Boxed: `sim::run` is what dispatched here.
        match Box::pin(crate::commands::sim::run(client, Some(worktree.to_path_buf()), command)).await {
            Ok((value, said)) => {
                // A report can repeat what was typed or tapped ("opened …").
                let said = steps::redact(&said, &secrets);
                let value = steps::redact_json(value, &secrets);
                human.push(format!("{:>3}  {}\n     {}", step.n, step.text, said.replace('\n', "\n     ")));
                ran.push(json!({ "step": step.n, "line": step.text, "result": value }));
            }
            // The host may echo what it was given (a label): no secret in it.
            Err(failure) => {
                let message = steps::redact(&failure.message, &secrets);
                return Err(at(Failure { message, ..failure }, step, &args.file));
            }
        }
    }
    let count = ran.len();
    human.push(format!("ran {count} step{}", if count == 1 { "" } else { "s" }));
    Ok((json!({ "ran": count, "steps": ran }), human.join("\n")))
}

/// Whether a step changes the device's state (the run's one question).
fn changes_state(command: &SimCommand) -> bool {
    matches!(
        command,
        SimCommand::Privacy(_)
            | SimCommand::Install { .. }
            | SimCommand::Erase { .. }
            | SimCommand::Push(_)
            | SimCommand::Location(_)
            | SimCommand::Shutdown { .. }
    )
}

/// `failure` as step `step`'s: its words first, its exit code kept, and how
/// to go on when the step is waiting for the user.
fn at(mut failure: Failure, step: &Step, file: &Path) -> Failure {
    failure.message = format!("step {} failed: {}: {}", step.n, step.text, failure.message);
    if failure.exit == exit::PENDING {
        failure.next_steps.push(format!("then resume with `oximux sim run {} --from {}`", file.display(), step.n));
    }
    let mut data = match failure.data.take().map(|d| *d) {
        Some(serde_json::Value::Object(data)) => data,
        _ => serde_json::Map::new(),
    };
    data.insert("step".into(), step.n.into());
    failure.data = Some(Box::new(serde_json::Value::Object(data)));
    failure
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(n: usize, text: &str) -> Step {
        Step { n, line: n + 3, text: text.into(), words: Vec::new() }
    }

    #[test]
    fn a_failed_step_says_which_and_keeps_its_exit() {
        let f = at(Failure::new("consent-pending", exit::PENDING, "waiting"), &step(3, "tap 1 2"), Path::new("a.flow"));
        assert_eq!(f.exit, exit::PENDING);
        assert_eq!(f.message, "step 3 failed: tap 1 2: waiting");
        assert!(f.next_steps.iter().any(|s| s.contains("--from 3")), "{:?}", f.next_steps);
        assert_eq!(f.data.as_deref(), Some(&json!({ "step": 3 })));
        let f = at(Failure::new("assert-failed", exit::ERROR, "no").with_data(json!({ "x": 1 })), &step(1, "assert --label A"), Path::new("a"));
        assert_eq!(f.data.as_deref(), Some(&json!({ "x": 1, "step": 1 })));
        assert!(f.next_steps.is_empty());
    }

    #[test]
    fn only_state_changes_are_put_to_the_user() {
        let parse = |line: &str| steps::command(&oximux_remote_proto::simulator::words::split(line).unwrap()).unwrap();
        assert!(changes_state(&parse("privacy grant camera --app com.a.b")));
        assert!(changes_state(&parse("location set 1 2")));
        assert!(changes_state(&parse("erase")));
        assert!(!changes_state(&parse("tap 1 2")));
        assert!(!changes_state(&parse("appearance dark")));
    }
}
