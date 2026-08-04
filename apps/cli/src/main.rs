//! `oximux` — the scriptable client of a running OxiMux host.
//!
//! Offline verbs (`version`, `agent-context`, every `--help`/typo path) never
//! touch the socket: the client is constructed lazily, only when a verb needs
//! the host. The tokio runtime is likewise built only for host verbs.

mod cli;
mod client;
mod commands;
mod output;
mod render;
mod serve;

use clap::Parser as _;
use serde_json::json;

use cli::{
    Cli, Command, GitCommand, ModeCommand, ModelCommand, PermitCommand, ProjectsCommand,
    TermCommand, WorktreeCommand,
};
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
        // Serve owns its own (multi-thread) runtime and its own output
        // contract — one readiness line on stdout, logs on stderr.
        Command::Serve { data_dir, projects } => serve::run(serve::ServeArgs {
            data_dir: data_dir.clone(),
            projects: projects.clone(),
        }),
        // Host verbs: build the runtime, connect lazily, run the verb.
        _ => host_verb(args),
    };
    std::process::ExitCode::from(code)
}

fn host_verb(args: Cli) -> u8 {
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
    let json_mode = args.json;
    let timeout = args.timeout;
    rt.block_on(async {
        let outcome = async {
            let client = Client::connect(args.dir.clone(), timeout).await?;
            match args.command {
                Command::Status => commands::status::run(&client).await,
                Command::Ls => commands::sessions::ls(&client).await,
                Command::Projects { command: ProjectsCommand::Ls } => {
                    commands::sessions::projects_ls(&client).await
                }
                Command::Run { prompt, agent, model, cwd, worktree, bg } => {
                    let run_args =
                        commands::run::RunArgs { prompt, agent, model, cwd, worktree, bg };
                    commands::run::run(&client, run_args, json_mode).await
                }
                Command::Attach { session, from } => {
                    commands::attach::run(&client, &session, from, json_mode).await
                }
                Command::Send { session, prompt, no_wait } => {
                    commands::send::run(&client, &session, &prompt, no_wait, json_mode).await
                }
                Command::Wait { session, until } => {
                    commands::wait::run(&client, &session, until, timeout, json_mode).await
                }
                Command::Transcript { session } => {
                    commands::transcript::run(&client, &session).await
                }
                Command::Stop { session } => commands::session_ctl::stop(&client, &session).await,
                Command::Steer { session, text } => {
                    commands::session_ctl::steer(&client, &session, &text).await
                }
                Command::Permit { command } => match command {
                    PermitCommand::Ls { session } => commands::permit::ls(&client, &session).await,
                    PermitCommand::Allow { session, request } => {
                        commands::permit::allow(&client, &session, request.as_deref()).await
                    }
                    PermitCommand::Deny { session, request, message } => {
                        commands::permit::deny(&client, &session, request.as_deref(), &message)
                            .await
                    }
                    PermitCommand::Answer { session, request, answer } => {
                        commands::permit::answer(&client, &session, request.as_deref(), &answer)
                            .await
                    }
                },
                Command::Model { command } => match command {
                    ModelCommand::Ls { session } => commands::model::ls(&client, &session).await,
                    ModelCommand::Set { session, model } => {
                        commands::model::set_model(&client, &session, &model).await
                    }
                },
                Command::Mode { command } => match command {
                    ModeCommand::Set { session, mode } => {
                        commands::model::set_mode(&client, &session, &mode).await
                    }
                },
                Command::Git { command } => match command {
                    GitCommand::Status { session } => {
                        commands::git::status(&client, &session).await
                    }
                    GitCommand::Diff { session, path, staged, untracked } => {
                        commands::git::diff(&client, &session, &path, staged, untracked).await
                    }
                    GitCommand::Stage { session, paths } => {
                        commands::git::stage(&client, &session, paths).await
                    }
                    GitCommand::Unstage { session, paths } => {
                        commands::git::unstage(&client, &session, paths).await
                    }
                    GitCommand::Commit { session, message } => {
                        commands::git::commit(&client, &session, &message).await
                    }
                },
                Command::Term { command } => match command {
                    TermCommand::Ls => commands::term::ls(&client).await,
                    TermCommand::Attach { pty } => commands::term::attach(&client, &pty).await,
                },
                Command::Worktree { command } => match command {
                    WorktreeCommand::Create { slug, project } => {
                        commands::worktree::create(&client, &slug, project).await
                    }
                    WorktreeCommand::Ls { project } => {
                        commands::worktree::ls(&client, project).await
                    }
                    WorktreeCommand::Rm { id } => commands::worktree::rm(&client, &id).await,
                },
                Command::PairNew { read_only, force_non_tty } => {
                    commands::pair::pair_new(&client, read_only, force_non_tty, json_mode).await
                }
                Command::PairLs => commands::pair::pair_ls(&client).await,
                Command::PairRm { pubkey } => commands::pair::pair_rm(&client, &pubkey).await,
                // Offline verbs and `serve` are dispatched in `main`;
                // unreachable here.
                Command::Version | Command::AgentContext | Command::Serve { .. } => {
                    unreachable!("dispatched in main")
                }
            }
        }
        .await;
        render(json_mode, outcome)
    })
}
