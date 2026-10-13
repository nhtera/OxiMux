//! `oximux sim exec -- <args…>` (v31): a raw `simctl` or `adb` command on the
//! attached device. The host decides everything (whether exec is on, what may
//! run, which device) and asks the user each time; this prints what it ran.

use std::path::Path;

use oximux_remote_proto::simulator::{SimCmdWire, SimExecOutputWire, SimExecWire, SimReplyWire};
use serde_json::json;

use super::sim::{APP, Outcome, call, unexpected};
use crate::cli::exit;
use crate::cli_sim_exec::ExecArgs;
use crate::client::Client;
use crate::output::Failure;

pub(super) async fn exec(client: &Client, worktree: &Path, args: ExecArgs) -> Outcome {
    let reply = call(client, worktree, SimCmdWire::Exec(SimExecWire { argv: args.argv }), APP).await?;
    let SimReplyWire::ExecOutput(out) = reply else {
        return Err(unexpected("ExecOutput"));
    };
    outcome(out)
}

/// Exit 0 with its output when the command did; otherwise exit 1, the
/// output carried in the failure.
fn outcome(out: SimExecOutputWire) -> Outcome {
    let json = json!({ "status": out.status, "stdout": out.stdout, "stderr": out.stderr, "truncated": out.truncated });
    let mut human = out.stdout.trim_end().to_owned();
    let stderr = out.stderr.trim_end();
    if !stderr.is_empty() {
        if !human.is_empty() {
            human.push('\n');
        }
        human.push_str(stderr);
    }
    match out.status {
        Some(0) => Ok((json, human)),
        status => {
            let why = status.map_or_else(|| "the command was stopped".to_owned(), |code| format!("the command exited {code}"));
            let message = if human.is_empty() { why } else { format!("{human}\n({why})") };
            Err(Failure::new("exec-failed", exit::ERROR, message).with_data(json))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(status: Option<i32>, stdout: &str, stderr: &str) -> SimExecOutputWire {
        SimExecOutputWire { status, stdout: stdout.into(), stderr: stderr.into(), truncated: false }
    }

    #[test]
    fn a_command_that_failed_exits_1_with_its_output() {
        let (json, human) = outcome(out(Some(0), "ok\n", "")).unwrap();
        assert_eq!((json["status"].as_i64(), human.as_str()), (Some(0), "ok"));
        let failed = outcome(out(Some(3), "partial\n", "boom\n")).unwrap_err();
        assert_eq!((failed.exit, failed.message.as_str()), (exit::ERROR, "partial\nboom\n(the command exited 3)"));
        let stopped = outcome(out(None, "", "[oximux: stopped after 30 s]")).unwrap_err();
        assert!(stopped.message.ends_with("(the command was stopped)"), "{}", stopped.message);
    }
}
