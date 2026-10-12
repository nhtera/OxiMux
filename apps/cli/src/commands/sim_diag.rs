//! `oximux sim logs` and `sim crashes` (v29): one bounded read each, printed
//! as text (or `--json`).

use std::path::Path;

use oximux_remote_proto::simulator::{SimCmdWire, SimCrashWire, SimCrashesWire, SimLogLevelWire, SimLogsWire, SimReplyWire};
use serde_json::json;

use super::sim::{APP, Outcome, call, unexpected};
use crate::cli_sim_diag::{CrashesArgs, LogLevelArg, LogsArgs};
use crate::client::Client;

pub(super) async fn logs(client: &Client, worktree: &Path, args: LogsArgs) -> Outcome {
    let since = args.since;
    let reply = call(client, worktree, SimCmdWire::Logs(logs_wire(args)), APP).await?;
    let SimReplyWire::Logs { lines, truncated } = reply else {
        return Err(unexpected("Logs"));
    };
    Ok((json!({ "lines": lines, "truncated": truncated }), logs_human(&lines, truncated, since)))
}

fn logs_wire(args: LogsArgs) -> SimLogsWire {
    SimLogsWire {
        since_secs: args.since,
        app: args.app,
        grep: args.grep,
        level: match args.level {
            None => SimLogLevelWire::Default,
            Some(LogLevelArg::Error) => SimLogLevelWire::Error,
            Some(LogLevelArg::Info) => SimLogLevelWire::Info,
            Some(LogLevelArg::Debug) => SimLogLevelWire::Debug,
        },
        lines: args.lines,
    }
}

fn logs_human(lines: &[String], truncated: bool, since: u32) -> String {
    if lines.is_empty() {
        return format!("(nothing logged in the last {})", window(since));
    }
    let mut out = lines.join("\n");
    if truncated {
        out.push_str(&format!("\n({} lines shown, the newest; narrow with --since, --app or --grep, or raise --lines)", lines.len()));
    }
    out
}

pub(super) async fn crashes(client: &Client, worktree: &Path, args: CrashesArgs) -> Outcome {
    let since = args.since;
    let cmd = SimCmdWire::Crashes(SimCrashesWire { since_secs: args.since, app: args.app });
    let SimReplyWire::Crashes(list) = call(client, worktree, cmd, APP).await? else {
        return Err(unexpected("Crashes"));
    };
    let json = json!({
        "crashes": list.crashes.iter().map(crash_json).collect::<Vec<_>>(),
        "warnings": list.warnings,
    });
    Ok((json, crashes_human(&list.crashes, &list.warnings, since)))
}

fn crash_json(c: &SimCrashWire) -> serde_json::Value {
    json!({ "id": c.id, "time": c.time, "app": c.app, "exception": c.exception, "reason": c.reason, "frames": c.frames })
}

fn crashes_human(crashes: &[SimCrashWire], warnings: &[String], since: u32) -> String {
    let mut out: Vec<String> = Vec::new();
    if crashes.is_empty() {
        out.push(format!("no crashes in the last {}", window(since)));
    }
    for c in crashes {
        out.push(format!("{}  {} crashed: {}", c.time, c.app, c.exception));
        if !c.reason.is_empty() {
            out.push(format!("  {}", c.reason));
        }
        out.extend(c.frames.iter().enumerate().map(|(i, f)| format!("    {i} {f}")));
    }
    if !warnings.is_empty() {
        out.push("warnings:".into());
        out.extend(warnings.iter().map(|w| format!("  {w}")));
    }
    out.join("\n")
}

/// `30s`, `5m`, `1h` back from seconds.
fn window(secs: u32) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_says_when_older_lines_were_left_out() {
        assert_eq!(logs_human(&[], false, 30), "(nothing logged in the last 30s)");
        let lines = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(logs_human(&lines, false, 300), "a\nb");
        assert!(logs_human(&lines, true, 300).ends_with("raise --lines)"));
    }

    #[test]
    fn a_crash_reads_as_what_why_and_where() {
        let crash = SimCrashWire {
            id: "1".into(),
            time: "2026-10-12 15:54:29".into(),
            app: "dev.oximux.crashy".into(),
            exception: "EXC_BREAKPOINT (SIGTRAP)".into(),
            reason: "Fatal error: Index out of range".into(),
            frames: vec!["libswiftCore.dylib _assertionFailure".into(), "Crashy closure #1".into()],
        };
        assert_eq!(
            crashes_human(&[crash], &["ANR in com.x".into()], 600),
            "2026-10-12 15:54:29  dev.oximux.crashy crashed: EXC_BREAKPOINT (SIGTRAP)\n  Fatal error: Index out of range\n    0 libswiftCore.dylib _assertionFailure\n    1 Crashy closure #1\nwarnings:\n  ANR in com.x"
        );
        assert_eq!(crashes_human(&[], &[], 3600), "no crashes in the last 1h");
    }

    #[test]
    fn the_level_defaults_to_the_platforms() {
        let args = LogsArgs { since: 30, app: None, grep: None, level: None, lines: 200 };
        assert_eq!(logs_wire(args.clone()).level, SimLogLevelWire::Default);
        assert_eq!(logs_wire(LogsArgs { level: Some(LogLevelArg::Error), ..args }).level, SimLogLevelWire::Error);
    }
}
