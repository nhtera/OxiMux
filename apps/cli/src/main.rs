//! `oximux` — the scriptable client of a running OxiMux host.
//!
//! Offline verbs (`version`, `agent-context`, every `--help`/typo path) never
//! touch the socket: the client is constructed lazily, only when a verb needs
//! the host. The tokio runtime is likewise built only for host verbs.

mod cli;
mod client;
mod commands;
mod output;
mod output_schema;
mod render;
mod serve;

use clap::Parser as _;
use serde_json::json;

use cli::{
    Cli, Command, GitCommand, HeartbeatCommand, ModeCommand, ModelCommand, PermitCommand,
    ProjectsCommand, ScheduleCommand, StateCommand, TeamCommand, TeamReportStatus, TermCommand,
    WorktreeCommand,
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
        #[cfg(not(windows))]
        Command::Serve { data_dir, projects } => serve::run(serve::ServeArgs {
            data_dir: data_dir.clone(),
            projects: projects.clone(),
        }),
        // Windows adds the SCM modes: install/uninstall are one-shot admin
        // helpers, --service hands the process to the service dispatcher, and
        // the plain invocation serves on the console exactly as unix does.
        #[cfg(windows)]
        Command::Serve { data_dir, projects, service, install_service, uninstall_service } => {
            if *install_service {
                serve::service_windows::install(data_dir.clone(), projects)
            } else if *uninstall_service {
                serve::service_windows::uninstall()
            } else {
                let serve_args = serve::ServeArgs {
                    data_dir: data_dir.clone(),
                    projects: projects.clone(),
                };
                if *service {
                    serve::service_windows::run_service(serve_args)
                } else {
                    serve::run(serve_args)
                }
            }
        }
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
                Command::Schedule { command } => match command {
                    ScheduleCommand::Create { prompt, name, cwd, agent, every, daily, weekly } => {
                        let create_args = commands::schedule::CreateArgs {
                            prompt,
                            name,
                            cwd,
                            agent,
                            every,
                            daily,
                            weekly,
                        };
                        commands::schedule::create(&client, create_args).await
                    }
                    ScheduleCommand::Ls => commands::schedule::ls(&client).await,
                    ScheduleCommand::Logs { id, limit } => {
                        commands::schedule::logs(&client, &id, limit).await
                    }
                    ScheduleCommand::Pause { id } => {
                        commands::schedule::set_enabled(&client, &id, false).await
                    }
                    ScheduleCommand::Resume { id } => {
                        commands::schedule::set_enabled(&client, &id, true).await
                    }
                    ScheduleCommand::RunOnce { id } => {
                        commands::schedule::run_once(&client, &id).await
                    }
                    ScheduleCommand::Rm { id } => commands::schedule::rm(&client, &id).await,
                },
                Command::Run { prompt, agent, model, cwd, worktree, output_schema, bg } => {
                    let run_args = commands::run::RunArgs {
                        prompt,
                        agent,
                        model,
                        cwd,
                        worktree,
                        output_schema,
                        bg,
                    };
                    commands::run::run(&client, run_args, json_mode).await
                }
                Command::Attach { session, from } => {
                    commands::attach::run(&client, &session, from, json_mode).await
                }
                Command::Send { session, prompt, output_schema, no_wait } => {
                    commands::send::run_checked(
                        &client,
                        &session,
                        &prompt,
                        output_schema.as_deref(),
                        no_wait,
                        json_mode,
                    )
                    .await
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
                Command::Heartbeat { command } => match command {
                    HeartbeatCommand::Create { prompt, name, cron, session } => {
                        commands::heartbeat::create(&client, session, name, &cron, prompt).await
                    }
                    HeartbeatCommand::Ls { session } => {
                        commands::heartbeat::ls(&client, session).await
                    }
                    HeartbeatCommand::Rm { id } => commands::heartbeat::rm(&client, &id).await,
                },
                Command::Team { command } => match command {
                    TeamCommand::Run { name, roles, cwd, agent, worktree_each } => {
                        let args = commands::team::RunArgs {
                            name,
                            roles,
                            cwd,
                            agent,
                            worktree_each,
                        };
                        commands::team::run(&client, args).await
                    }
                    TeamCommand::Report { run, role, status, summary } => {
                        let ok = status == TeamReportStatus::Done;
                        commands::team::report(&client, &run, &role, ok, summary).await
                    }
                    TeamCommand::Status { run } => commands::team::status(&client, &run).await,
                    TeamCommand::Ls => commands::team::ls(&client).await,
                },
                Command::State { command } => match command {
                    StateCommand::Get { key } => commands::state::get(&client, &key).await,
                    StateCommand::Set { key, value, if_version } => {
                        commands::state::set(&client, &key, &value, if_version).await
                    }
                    StateCommand::Delete { key } => commands::state::delete(&client, &key).await,
                    StateCommand::Watch { prefix } => {
                        commands::state::watch(&client, prefix, json_mode).await
                    }
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
