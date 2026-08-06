//! The CLI's scripting contract, frozen.
//!
//! Everything here is a promise made to something that cannot complain: a shell
//! script, a CI job, an agent driving this binary from the `agent-context` dump.
//! Those callers branch on exit codes and read fixed JSON paths, so a change to
//! either is a silent breaking change — the caller does not fail to compile, it
//! quietly takes the wrong branch. These tests are what make such a change loud.
//!
//! Deliberately **not** a byte-for-byte golden of the whole command tree. That
//! would fail on every new flag, which is an additive, non-breaking change, and
//! a gate that cries wolf on safe edits gets regenerated without being read.
//! What is frozen is what a caller can actually depend on:
//!
//! - the exit-code numbers, and the names the dump gives them;
//! - the JSON envelope's shape, on success and on failure;
//! - `next_steps` being present and non-empty on *every* failure;
//! - the existence of the documented verbs (additions pass, removals and
//!   renames fail);
//! - the structural invariants of the `agent-context` dump itself.
//!
//! `cli_e2e` covers exit codes 3 and 5 against a live host. This suite covers
//! what needs no host at all, plus the parts of the contract that are about
//! *shape* rather than about any one verb.

use std::process::Command;

use serde_json::Value;

/// The binary with the runner's own credentials scrubbed, pointed at a
/// directory where nothing is serving — every failure here is provoked, not
/// inherited from the developer's machine.
fn bin(dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oximux-cli"));
    cmd.args(["--dir", dir.to_str().unwrap(), "--timeout", "5"]);
    cmd.env_remove(oximux_remote_local::SESSION_ENV_VAR);
    cmd.env_remove(oximux_remote_local::SESSION_TOKEN_ENV_VAR);
    cmd
}

fn json_stdout(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn dump() -> Value {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(dir.path()).arg("agent-context").output().expect("agent-context");
    assert_eq!(out.status.code(), Some(0), "agent-context must work with no host");
    json_stdout(&out)
}

/// The numbers themselves. Written as literals rather than by importing the
/// constants: importing them would make this test tautological — it would prove
/// the constants equal themselves. A script's `if [ $? -eq 3 ]` hard-codes the
/// number, so the number is what must be pinned.
#[test]
fn the_exit_codes_are_the_numbers_scripts_branch_on() {
    let codes = &dump()["exit_codes"];
    assert_eq!(codes["ok"], 0);
    assert_eq!(codes["error"], 1);
    assert_eq!(codes["usage"], 2);
    assert_eq!(codes["host_unreachable"], 3);
    assert_eq!(codes["timeout"], 4);
    assert_eq!(codes["access_denied"], 5);

    // And the dump names exactly these six — a seventh nobody documented, or a
    // renamed key, both break a caller reading the map by name.
    let named: Vec<&str> = codes.as_object().expect("a map").keys().map(String::as_str).collect();
    let mut sorted = named.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        ["access_denied", "error", "host_unreachable", "ok", "timeout", "usage"],
        "the exit-code vocabulary changed: {named:?}",
    );
}

/// Success envelope: `{"ok":true,"data":…}` and nothing else at the top level.
/// A third key would be read by nobody and would make the shape ambiguous for
/// anyone matching on it.
#[test]
fn a_successful_json_verb_carries_ok_and_data_only() {
    let dir = tempfile::tempdir().unwrap();
    let out = bin(dir.path()).args(["--json", "version"]).output().expect("version");
    assert_eq!(out.status.code(), Some(0));

    let v = json_stdout(&out);
    assert_eq!(v["ok"], true);
    assert!(v.get("data").is_some(), "a success carries data: {v}");
    assert!(v.get("error").is_none(), "a success carries no error: {v}");

    let keys: Vec<&str> = v.as_object().expect("an object").keys().map(String::as_str).collect();
    assert_eq!(keys, ["ok", "data"], "the success envelope grew a key: {keys:?}");
}

/// Failure envelope, for every class reachable without a host. One table rather
/// than three tests, because the point is that they are *the same shape* — a
/// caller writes one error handler, not one per verb.
#[test]
fn every_failure_carries_a_code_a_message_and_next_steps() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("nothing-serving-here");
    std::fs::create_dir_all(&nowhere).unwrap();

    // (argv, expected exit, expected error code)
    let cases: &[(&[&str], i32, &str)] = &[
        (&["status"], 3, "unreachable"),
        (&["ls"], 3, "unreachable"),
        (&["transcript", "no-such-session"], 3, "unreachable"),
    ];

    for (argv, code, error_code) in cases {
        let out = bin(&nowhere).arg("--json").args(*argv).output().expect("run");
        assert_eq!(
            out.status.code(),
            Some(*code),
            "`{}` should exit {code}\nstdout: {}\nstderr: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );

        let v = json_stdout(&out);
        assert_eq!(v["ok"], false, "`{}`: {v}", argv.join(" "));
        assert_eq!(v["error"]["code"], *error_code, "`{}`: {v}", argv.join(" "));

        let keys: Vec<&str> =
            v.as_object().expect("an object").keys().map(String::as_str).collect();
        assert_eq!(keys, ["ok", "error"], "the failure envelope grew a key: {keys:?}");

        let err = v["error"].as_object().expect("error is an object");
        let mut err_keys: Vec<&str> = err.keys().map(String::as_str).collect();
        err_keys.sort_unstable();
        assert_eq!(
            err_keys,
            ["code", "message", "next_steps"],
            "the error object's shape changed for `{}`",
            argv.join(" "),
        );

        assert!(
            !v["error"]["message"].as_str().expect("message is a string").is_empty(),
            "`{}` failed with an empty message",
            argv.join(" "),
        );
        let steps = v["error"]["next_steps"].as_array().expect("next_steps is an array");
        assert!(
            !steps.is_empty(),
            "`{}` failed with no next step — the field exists so a caller is never \
             told only that something broke",
            argv.join(" "),
        );
        assert!(
            steps.iter().all(|s| s.as_str().is_some_and(|s| !s.trim().is_empty())),
            "`{}` has a blank next step: {v}",
            argv.join(" "),
        );
    }
}

/// A usage error is clap's, and it stays exit 2 and off stdout. Scripts capture
/// stdout; a parser complaint landing there would be read as output.
#[test]
fn a_usage_error_is_exit_2_and_never_pollutes_stdout() {
    let dir = tempfile::tempdir().unwrap();

    for argv in [vec!["no-such-verb"], vec!["run"], vec!["--no-such-flag"]] {
        let out = bin(dir.path()).args(&argv).output().expect("run");
        assert_eq!(out.status.code(), Some(2), "`{}` should be a usage error", argv.join(" "));
        assert!(
            out.stdout.is_empty(),
            "`{}` wrote to stdout: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(!out.stderr.is_empty(), "`{}` explained nothing", argv.join(" "));
    }
}

/// The verbs a caller may have written a script against. Asserted as a subset:
/// adding a verb is additive and must stay free, while removing or renaming one
/// breaks callers and must be a deliberate, visible act.
#[test]
fn the_documented_verbs_still_exist() {
    let dump = dump();
    let present: Vec<&str> = dump["command"]["subcommands"]
        .as_array()
        .expect("subcommands")
        .iter()
        .map(|c| c["name"].as_str().expect("a name"))
        .collect();

    // Every verb this CLI has shipped. New entries belong here when they ship;
    // an entry leaving is the breaking change this test exists to surface.
    let promised = [
        "agent-context",
        "attach",
        "git",
        "heartbeat",
        "hosts",
        "ls",
        "model",
        "pair",
        "permit",
        "run",
        "schedule",
        "send",
        "serve",
        "state",
        "status",
        "steer",
        "stop",
        "team",
        "term",
        "transcript",
        "update",
        "version",
        "wait",
        "worktree",
    ];
    let missing: Vec<&str> =
        promised.iter().copied().filter(|v| !present.contains(v)).collect();
    assert!(
        missing.is_empty(),
        "verbs disappeared from the CLI: {missing:?}\nstill present: {present:?}\n\
         Removing or renaming a verb breaks every script that calls it — if the \
         removal is intended, drop it from `promised` in the same commit.",
    );
}

/// The dump is the machine-readable contract, so its own shape is part of the
/// contract. An agent generates calls from these fields; a missing `takes_value`
/// makes it omit an argument's value, which is the specific bug the field's
/// comment in `agent_context.rs` records having already happened once.
#[test]
fn the_agent_context_dump_describes_arguments_completely() {
    let dump = dump();

    assert_eq!(dump["command"]["name"], "oximux", "the dump names the command users type");
    for key in ["json_flag", "async_contract", "session_scope", "turn_bound"] {
        assert!(
            dump["conventions"][key].as_str().is_some_and(|s| !s.is_empty()),
            "the `{key}` convention is missing from the dump",
        );
    }

    // Walk the whole tree, counting rather than type-checking.
    //
    // The first version of this asserted `takes_value.is_boolean()`, which is
    // tautological: the field is built from `a.get_action().takes_values()`, so
    // serde always renders *a* boolean. It cannot fail, and in particular it
    // cannot catch the one regression `agent_context.rs` records having already
    // happened — every `takes_value` reading `false` because the value range was
    // read off an unbuilt clap tree. Counting the trues is what catches that.
    fn walk(cmd: &Value, path: &str, args: &mut usize, valued: &mut usize) {
        for arg in cmd["args"].as_array().expect("args") {
            let name = arg["name"].as_str().expect("every arg is named");
            let takes = arg["takes_value"]
                .as_bool()
                .unwrap_or_else(|| panic!("{path} {name}: takes_value is not a boolean"));
            assert!(arg["global"].is_boolean(), "{path} {name}: global must be a boolean");
            *args += 1;
            if takes {
                *valued += 1;
            }
        }
        for sub in cmd["subcommands"].as_array().expect("subcommands") {
            let name = sub["name"].as_str().expect("every subcommand is named");
            assert!(
                sub["about"].as_str().is_some_and(|a| !a.is_empty()),
                "{path} {name} has no description — the dump is an agent's only \
                 documentation",
            );
            walk(sub, &format!("{path} {name}"), args, valued);
        }
    }

    let (mut args, mut valued) = (0, 0);
    walk(&dump["command"], "oximux", &mut args, &mut valued);

    // The tree carries ~105 arguments today. A floor near that catches a walk
    // that silently stops descending; a floor of "more than a handful" would
    // not have.
    assert!(args >= 90, "only {args} arguments walked — the tree looks truncated");
    // ~91 of those take a value. If the value range were being read off an
    // unbuilt tree again this would collapse toward zero, which is the failure
    // this assertion exists for.
    assert!(
        valued >= 75,
        "only {valued} of {args} arguments report taking a value — that is the \
         shape of `takes_value` being read off an unbuilt clap tree, which makes \
         an agent omit every argument's value",
    );
}

/// `--json` is global, and the value-taking globals must say so. An agent that
/// reads `takes_value: false` for `--dir` emits `--dir` with no path and gets a
/// usage error it cannot diagnose.
#[test]
fn the_global_flags_report_whether_they_take_a_value() {
    let dump = dump();
    let args = dump["command"]["args"].as_array().expect("args");
    let find = |name: &str| {
        args.iter()
            .find(|a| a["name"] == name)
            .unwrap_or_else(|| panic!("global `{name}` missing from the dump"))
    };

    assert_eq!(find("json")["takes_value"], false, "--json is a switch");
    assert_eq!(find("json")["global"], true);
    assert_eq!(find("dir")["takes_value"], true, "--dir takes a path");
    assert_eq!(find("timeout")["takes_value"], true, "--timeout takes seconds");
    assert_eq!(find("host")["takes_value"], true, "--host takes a name");
}
