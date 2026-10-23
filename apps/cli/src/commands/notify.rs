//! `oximux notify [--title T] [--body B]` — raise an attention banner for the
//! pane this runs in.
//!
//! The same verb the desktop binary answers, so a script or an agent hook
//! reaches it through whichever `oximux` is first on PATH — inside an OxiMux
//! terminal, the CLI the app bundles. Dispatched before clap, like
//! [`agent_status`](super::agent_status), and for the same reasons: hooks run
//! it (Codex's `notify` appends its own JSON payload as an argument, which clap
//! would answer with usage and exit 2), and **it must never fail an agent's
//! turn** — every outcome is exit 0, with anything worth saying on stderr.
//! Unlike `agent-status` it does say why nothing happened, because a person
//! typing it at a prompt wants to know.

use oximux_agent_hooks::report::NotifyArgs;
use oximux_relay_proto::{Request, Response};

use super::hook_relay;
use crate::cli::exit;

/// True when this invocation is the notify verb.
pub fn is_invocation() -> bool {
    std::env::args().nth(1).as_deref() == Some("notify")
}

/// Run the verb. Returns the process exit code.
pub fn run() -> u8 {
    let NotifyArgs { title, body } = NotifyArgs::parse(std::env::args().skip(2));
    let Some(pty_id) = std::env::var("OXIMUX_PTY_ID").ok().filter(|id| !id.is_empty()) else {
        eprintln!("oximux notify: OXIMUX_PTY_ID not set (run inside an OxiMux terminal)");
        return exit::OK;
    };
    let Some((socket, token)) = oximux_remote_local::default_runtime_dir().and_then(|dir| hook_relay::endpoint(&dir))
    else {
        eprintln!("oximux notify: no OxiMux terminal daemon is running");
        return exit::OK;
    };
    let Some(rt) = hook_relay::runtime() else {
        return exit::OK;
    };
    match rt.block_on(hook_relay::deliver(&socket, &token, Request::Notify { pty_id, title, body })) {
        Ok(Response::Ok) => {}
        Ok(Response::Err { message, .. }) => eprintln!("oximux notify: {message}"),
        Ok(other) => eprintln!("oximux notify: unexpected response: {other:?}"),
        Err(why) => eprintln!("oximux notify: {why}"),
    }
    exit::OK
}
