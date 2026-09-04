//! `oximux agent-status` — the command an installed hook actually runs.
//!
//! Every agent CLI OxiMux teaches to report runs this with a `--state`, and
//! hands it the lifecycle event as JSON on stdin. It resolves the pane it is
//! running in, asks the relay to frame the state as OSC-9999 on that pane's
//! output stream, and gets out of the way.
//!
//! **It must never fail the agent's turn.** Most agents run their hooks
//! synchronously, so this sits in front of the user's next reply; anything that
//! is not a mis-installed hook exits 0 and says nothing. A hook that reported
//! nothing costs a row its detail. A hook that errors costs the user their
//! turn.
//!
//! Dispatched before clap, like the desktop's copy, for two reasons: the flags
//! come from a file OxiMux itself wrote (a newer OxiMux may have written a flag
//! this binary has never heard of, which must be ignored rather than refused),
//! and clap answers an unknown flag by printing usage and exiting 2 — which is
//! exactly the failure the paragraph above forbids.

use oximux_agent_hooks::report::StatusArgs;

use crate::cli::exit;

/// True when this invocation is the hook verb, which is dispatched before the
/// argument parser ever runs.
pub fn is_hook_invocation() -> bool {
    std::env::args().nth(1).as_deref() == Some("agent-status")
}

/// Run the hook. Returns the process exit code.
pub fn run() -> u8 {
    let args = match StatusArgs::parse(std::env::args().skip(2)) {
        Ok(args) => args,
        // The one failure worth reporting: the hook entry itself is wrong, and
        // nothing will make it work until someone edits it.
        Err(msg) => {
            eprintln!("oximux agent-status: {msg}");
            return exit::USAGE;
        }
    };
    let stdin_json = {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        buf
    };
    // Absent outside an OxiMux pane — a plain shell, or an agent started from
    // somewhere else entirely. There is no row to report to, and that is not an
    // error.
    let pty_id = match std::env::var("OXIMUX_PTY_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => return exit::OK,
    };
    let Some(payload) = args.payload(&stdin_json) else {
        return exit::OK;
    };
    // The host's data root, the same one `oximux serve` defaults to and the
    // desktop computes for itself. A host serving a `--data-dir` elsewhere is
    // not reachable from here: the hook is handed no way to learn about it.
    let Some(runtime_dir) = oximux_remote_local::default_runtime_dir() else {
        eprintln!("oximux agent-status: this platform reports no local data directory");
        return exit::ERROR;
    };
    let token = match std::fs::read_to_string(oximux_remote_local::token_path(&runtime_dir)) {
        Ok(token) => token.trim().to_owned(),
        // No relay running — the common case outside OxiMux, and nothing the
        // agent should hear about.
        Err(_) => return exit::OK,
    };
    let socket = oximux_remote_local::socket_path(&runtime_dir);

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return exit::OK;
    };
    rt.block_on(async move {
        // Bounded, because the whole exchange sits in front of the user's next
        // reply on every agent that runs its hooks synchronously.
        //
        // Not a hypothetical: the relay does not answer `AgentStatus` for a pty
        // it does not know, and a pane closed while its agent was mid-turn is
        // exactly that. Unbounded, the hook then waits forever — and on the
        // dialects that run ours asynchronously (no timeout of their own, since
        // an async hook cannot hold anything up) it would leak one stuck
        // process per event. Generous for a local socket round-trip, so a
        // loaded machine still reports.
        let deadline = std::time::Duration::from_secs(3);
        let sent = tokio::time::timeout(deadline, async {
            let client = oximux_relay_client::RelayClient::connect(&socket, &token).await.ok()?;
            client
                .request(oximux_relay_proto::Request::AgentStatus { pty_id, payload })
                .await
                .ok()
        })
        .await;
        // Every outcome is exit 0. A stale socket outliving its daemon, a pane
        // that has gone away, a relay too busy to answer — none of them are the
        // agent's problem, and none are worth failing its turn over.
        let _ = sent;
        exit::OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mis_installed_hook_is_the_only_thing_worth_an_error() {
        // Everything else — no pane, no relay, a stale socket — exits 0, so the
        // hook never costs the user a turn. Only an entry that can never work
        // says so, and it says so once per event until someone fixes the file.
        assert!(StatusArgs::parse(["--state".into(), "nonsense".into()]).is_err());
        assert!(StatusArgs::parse(["--state".into(), "idle".into()]).is_ok());
    }

    #[test]
    fn the_hook_verb_is_recognised_only_as_the_first_argument() {
        // It is dispatched before clap, so the check has to be exact: a
        // `--message` containing the word must not be mistaken for it.
        assert!(!is_hook_invocation() || std::env::args().nth(1).as_deref() == Some("agent-status"));
    }
}
