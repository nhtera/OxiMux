//! `oximux` — the scriptable client of a running OxiMux host.
//!
//! Offline verbs (`version`, `agent-context`, every `--help`/typo path) never
//! touch the socket: the client is constructed lazily, only when a verb needs
//! the host. The tokio runtime is likewise built only for host verbs.

mod cli;
mod client;
mod commands;
mod output;

use clap::Parser as _;
use serde_json::json;

use cli::{Cli, Command, ProjectsCommand};
use client::Client;
use output::render;

fn main() -> std::process::ExitCode {
    // Usage errors exit 2 here, printing clap's own message — before any I/O.
    let args = Cli::parse();

    let code = match &args.command {
        // Offline: no runtime, no socket, no database.
        Command::Version => {
            let data = json!({
                "version": env!("CARGO_PKG_VERSION"),
                "protocol_version": oximux_remote_proto::proto::PROTOCOL_VERSION,
            });
            let human = format!(
                "oximux {} (protocol v{})",
                env!("CARGO_PKG_VERSION"),
                oximux_remote_proto::proto::PROTOCOL_VERSION
            );
            render(args.json, Ok((data, human)))
        }
        Command::AgentContext => {
            // Schema output is FOR machines; both modes print JSON, `--json`
            // merely wraps it in the standard envelope.
            let data = commands::agent_context::dump();
            let human = serde_json::to_string_pretty(&data).expect("static json");
            render(args.json, Ok((data, human)))
        }
        // Host verbs: build the runtime, connect lazily, run the verb.
        Command::Status | Command::Ls | Command::Projects { .. } => host_verb(&args),
    };
    std::process::ExitCode::from(code)
}

fn host_verb(args: &Cli) -> u8 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            return render(
                args.json,
                Err(output::Failure::new(
                    "runtime",
                    cli::exit::ERROR,
                    format!("could not start the async runtime: {e}"),
                )),
            );
        }
    };
    rt.block_on(async {
        let outcome = async {
            let client = Client::connect(args.dir.clone(), args.timeout).await?;
            match &args.command {
                Command::Status => commands::status::run(&client).await,
                Command::Ls => commands::sessions::ls(&client).await,
                Command::Projects { command: ProjectsCommand::Ls } => {
                    commands::sessions::projects_ls(&client).await
                }
                // Offline verbs are dispatched in `main`; unreachable here.
                Command::Version | Command::AgentContext => unreachable!("offline verb"),
            }
        }
        .await;
        render(args.json, outcome)
    })
}
