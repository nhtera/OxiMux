//! `oximux agent-context` — the command surface as data, for agents driving
//! this CLI. Derived by walking the live clap tree, so it CANNOT drift from
//! what the parser actually accepts; there is no second schema to maintain.

use clap::CommandFactory as _;
use serde_json::{Value, json};

use crate::cli::{Cli, exit};

pub fn dump() -> Value {
    let cmd = Cli::command();
    json!({
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
            "json_flag": "--json prints {\"ok\":true,\"data\":…} or {\"ok\":false,\"error\":{code,message,next_steps}} on stdout",
            "async_contract": "send-style verbs return when the host ACCEPTS the work, not when the agent finishes",
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
                "takes_value": a.get_num_args().is_some_and(|n| n.takes_values()),
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
        assert_eq!(dump["exit_codes"]["access_denied"], 5);
        assert_eq!(dump["exit_codes"]["host_unreachable"], 3);
        assert_eq!(dump["command"]["name"], "oximux", "users type `oximux`");
    }
}
