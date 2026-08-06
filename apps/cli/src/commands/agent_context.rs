//! `oximux agent-context` — the command surface as data, for agents driving
//! this CLI. Derived by walking the live clap tree, so it CANNOT drift from
//! what the parser actually accepts; there is no second schema to maintain.

use clap::CommandFactory as _;
use serde_json::{Value, json};

use crate::cli::{Cli, exit};

/// The shape of this dump, so a consumer that caches or codegens against it can
/// tell when the shape moved under it — the same contract `serve`'s readiness
/// line carries.
///
/// Bump ONLY for a change that breaks a reader of the previous version:
/// renaming or removing a top-level key, or changing the meaning or type of an
/// existing field. Adding a command, a flag, or a new top-level key does not
/// bump it — the tree is *expected* to grow, and a reader that broke on growth
/// would break on every release.
///
/// This cannot be usefully retrofitted: a consumer already parsing an
/// unversioned dump has no field to check, so the field only helps if it was
/// there first.
const SCHEMA_VERSION: u32 = 1;

pub fn dump() -> Value {
    let cmd = Cli::command();
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "command": describe(&cmd),
        "exit_codes": {
            "ok": exit::OK,
            "error": exit::ERROR,
            "usage": exit::USAGE,
            "host_unreachable": exit::UNREACHABLE,
            "timeout": exit::TIMEOUT,
            "access_denied": exit::DENIED,
        },
        "conventions": {
            "json_flag": "--json prints {\"ok\":true,\"data\":…} or {\"ok\":false,\"error\":{code,message,next_steps}} on stdout; `error.data` is present only when a failure leaves something addressable behind (e.g. session_id on a turn timeout)",
            "async_contract": "send-style verbs return when the host ACCEPTS the work, not when the agent finishes",
            // Spelled out because an agent reading this dump is exactly the
            // caller that gets stranded by the distinction: --timeout is global,
            // so it appears on run/send and reads like a bound on the turn.
            "turn_bound": "--timeout bounds one host reply, never an agent's turn; run/send stream unbounded unless given --turn-timeout <SECS> (exit 4), which stops the wait and leaves the agent running",
            "session_scope": format!(
                "when {} is set, this CLI reaches only that session",
                oximux_remote_local::SESSION_ENV_VAR
            ),
        },
    })
}

fn describe(cmd: &clap::Command) -> Value {
    let args: Vec<Value> = cmd
        .get_arguments()
        .filter(|a| a.get_id() != "help" && a.get_id() != "version")
        .map(|a| {
            json!({
                "name": a.get_id().as_str(),
                "long": a.get_long(),
                "help": a.get_help().map(|h| h.to_string()),
                // From the action, not `get_num_args()`. `Cli::command()` hands
                // back an UNBUILT tree, and clap fills an argument's value range
                // during the build it never runs — so `get_num_args()` is `None`
                // for every argument here and this field read `false` even for
                // `--dir` and `--timeout`. An agent driving this CLI from the
                // dump would then omit their values.
                "takes_value": a.get_action().takes_values(),
                "global": a.is_global_set(),
            })
        })
        .collect();
    let subcommands: Vec<Value> = cmd.get_subcommands().map(describe).collect();
    json!({
        "name": cmd.get_name(),
        "about": cmd.get_about().map(|a| a.to_string()),
        "args": args,
        "subcommands": subcommands,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap's own consistency check — catches conflicting/misconfigured args
    /// at test time instead of first parse.
    #[test]
    fn clap_tree_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// The golden shape: every verb and global flag the phase promises is in
    /// the dump, under the names scripts will key on.
    #[test]
    fn dump_matches_the_clap_tree() {
        let dump = dump();
        let names: Vec<&str> = dump["command"]["subcommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        for verb in ["status", "ls", "projects", "version", "agent-context"] {
            assert!(names.contains(&verb), "verb {verb} missing from {names:?}");
        }
        let globals: Vec<&str> = dump["command"]["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["global"].as_bool() == Some(true))
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        for flag in ["json", "dir", "timeout"] {
            assert!(globals.contains(&flag), "global --{flag} missing from {globals:?}");
        }
        // Whether a flag takes a value is the one field a caller cannot guess,
        // and the whole audience for this dump is a machine composing argv from
        // it. `--dir X` and `--timeout N` take values; `--json` is a bare
        // switch. Asserted because reading this off clap's unbuilt tree used to
        // report `false` for all three.
        let takes_value = |name: &str| -> bool {
            dump["command"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"] == name)
                .unwrap_or_else(|| panic!("--{name} missing from the dump"))["takes_value"]
                .as_bool()
                .expect("takes_value is a bool")
        };
        assert!(takes_value("dir"), "--dir takes a directory");
        assert!(takes_value("timeout"), "--timeout takes a number of seconds");
        assert!(!takes_value("json"), "--json is a bare switch");
        assert_eq!(dump["exit_codes"]["access_denied"], 5);
        assert_eq!(dump["exit_codes"]["host_unreachable"], 3);
        assert_eq!(dump["command"]["name"], "oximux", "users type `oximux`");
    }

    /// The dump declares its own shape. Without this a consumer that cached or
    /// codegened from it has no way to notice the shape changed — and this is
    /// the one command whose entire audience is such a consumer.
    #[test]
    fn the_dump_declares_its_schema_version() {
        let dump = dump();
        assert_eq!(
            dump["schemaVersion"], SCHEMA_VERSION,
            "agent-context must carry the version of its own shape"
        );
        assert!(
            dump["schemaVersion"].is_u64(),
            "a number, so a consumer can compare it: {:?}",
            dump["schemaVersion"]
        );
    }
}
