//! `oximux` — the scriptable client of a running OxiMux host.
//!
//! Offline verbs (`version`, `agent-context`, every `--help`/typo path) never
//! touch the socket: the client is constructed lazily, only when a verb needs
//! the host. The tokio runtime is likewise built only for host verbs.

mod build_info;
mod cli;
mod client;
mod client_identity;
mod commands;
mod hosts_store;
mod output;
mod output_schema;
mod remote_client;
mod render;
mod serve;
mod update;

use clap::Parser as _;

use cli::{
    Cli, Command, GitCommand, HeartbeatCommand, HostsCommand, ModeCommand, ModelCommand,
    PermitCommand, ProjectsCommand, ScheduleCommand, StateCommand, TeamCommand, TeamReportStatus,
    TermCommand, WorktreeCommand,
};
use client::Client;
use output::render;

fn main() -> std::process::ExitCode {
    // Usage errors exit 2 here, printing clap's own message — before any I/O.
    let args = Cli::parse();

    // Completes an update that could not delete the binaries it replaced
    // because they were still running. That is the norm on Windows, where an
    // image mapped into a live process cannot be unlinked, and the exception on
    // unix — where the swap deletes its own backups and this finds nothing
    // unless one of those deletions failed, the one case nothing else cleans
    // up. Cheap (one readdir), silent, and unconditional: the very next
    // invocation after an update is the first chance to finish it.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        update::swap::sweep_backups(dir);
    }

    let code = match &args.command {
        // Offline: no runtime, no socket, no database.
        Command::Version => {
            let (data, human) = commands::update::version();
            render(args.json, Ok((data, human)))
        }
        // Talks to the release server and nothing else — no host, no runtime.
        // That is what lets it repair a machine whose own host is wedged.
        Command::Update { check } => render(args.json, commands::update::run(*check)),
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

/// The protocol version a verb needs, and the name to say when a host is too
/// old for it.
///
/// Only verbs whose RPCs were *appended* appear here. That is the point of the
/// gate: an old host serves everything below its own version perfectly well, so
/// refusing wholesale would break the compatibility this protocol is designed
/// for. What it must not do is send an ordinal the host cannot decode — postcard
/// answers that by dropping the connection, which surfaces to the user as "the
/// host closed the connection" with nothing to act on.
fn required_version(command: &Command) -> Option<(u32, &'static str)> {
    match command {
        // v18: the automation surface.
        Command::Heartbeat { .. } => Some((18, "heartbeat")),
        Command::Team { .. } => Some((18, "team")),
        Command::State { .. } => Some((18, "state")),
        // v17: the manual fire. The rest of `schedule` is v10.
        Command::Schedule { command: ScheduleCommand::RunOnce { .. } } => {
            Some((17, "schedule run-once"))
        }
        // v16: the worktree surface, paginated transcripts, pairing admin.
        Command::Worktree { .. } => Some((16, "worktree")),
        Command::Transcript { .. } => Some((16, "transcript")),
        Command::PairNew { .. } | Command::PairLs | Command::PairRm { .. } => {
            Some((16, "pairing administration"))
        }
        _ => None,
    }
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
    let dir = args.dir.clone();
    let host = args.host.clone();
    rt.block_on(async {
        let outcome = async {
            // Verbs that talk to no host, or to several, are dispatched before
            // a single client is built: `pair` has nothing to connect to yet,
            // `hosts` reads local files, and `ls --all-hosts` fans out.
            match args.command {
                Command::Pair { ticket, name, default } => {
                    return commands::hosts::pair(&ticket, name, default, timeout).await;
                }
                Command::Hosts { command } => {
                    return match command {
                        HostsCommand::Add { name, ticket, default } => {
                            commands::hosts::pair(&ticket, Some(name), default, timeout).await
                        }
                        HostsCommand::Ls { probe } => commands::hosts::ls(probe, timeout).await,
                        HostsCommand::Rm { name } => commands::hosts::rm(&name, timeout).await,
                        HostsCommand::Default { name } => commands::hosts::set_default(&name),
                    };
                }
                Command::Ls { all_hosts: true, strict } => {
                    return commands::hosts::fleet_ls(strict, timeout, dir).await;
                }
                _ => {}
            }
            let client = Client::resolve_and_connect(
                client::HostSelection { flag: host, dir },
                timeout,
            )
            .await?;
            // The compat gate. `Hello` already refused a host outside the
            // mutually-compatible range; this catches the narrower case of a
            // host inside it that predates a specific verb — where postcard
            // would answer an appended ordinal by dropping the connection
            // rather than erroring. Only the verbs that need a floor are
            // gated, so reads a v15 host can still serve keep working.
            if let Some((needed, verb)) = required_version(&args.command) {
                client.require_version(needed, verb)?;
            }
            match args.command {
                Command::Status => commands::status::run(&client).await,
                Command::Ls { .. } => commands::sessions::ls(&client).await,
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
                // Offline verbs and `serve` are dispatched in `main`; the
                // host-book and fleet verbs a few lines above. Unreachable.
                Command::Version
                | Command::Update { .. }
                | Command::AgentContext
                | Command::Serve { .. }
                | Command::Pair { .. }
                | Command::Hosts { .. } => unreachable!("dispatched earlier"),
            }
        }
        .await;
        render(json_mode, outcome)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_of(argv: &[&str]) -> Command {
        Cli::try_parse_from(argv).expect("parses").command
    }

    /// The gate covers exactly the appended surfaces, and nothing else. A verb
    /// wrongly listed here would refuse a host that can serve it perfectly
    /// well; one wrongly omitted would send an ordinal an old host answers by
    /// dropping the connection.
    #[test]
    fn only_appended_verbs_declare_a_version_floor() {
        for (argv, expected) in [
            (vec!["oximux", "heartbeat", "ls"], Some(18)),
            (vec!["oximux", "team", "ls"], Some(18)),
            (vec!["oximux", "state", "get", "k"], Some(18)),
            (vec!["oximux", "schedule", "run-once", "sch-1"], Some(17)),
            (vec!["oximux", "worktree", "ls"], Some(16)),
            (vec!["oximux", "transcript", "s1"], Some(16)),
            (vec!["oximux", "pair-ls"], Some(16)),
            // The long-standing surface every host has spoken since v1–v12.
            (vec!["oximux", "ls"], None),
            (vec!["oximux", "status"], None),
            (vec!["oximux", "send", "s1", "hi"], None),
            (vec!["oximux", "schedule", "ls"], None),
        ] {
            let needed = required_version(&command_of(&argv)).map(|(v, _)| v);
            assert_eq!(needed, expected, "{argv:?}");
        }
    }

    /// `schedule` is the one family split across versions: only the manual fire
    /// is v17, and gating the whole family would strand a v10 host's list.
    #[test]
    fn only_run_once_gates_the_schedule_family() {
        assert!(required_version(&command_of(&["oximux", "schedule", "ls"])).is_none());
        assert!(required_version(&command_of(&["oximux", "schedule", "rm", "s"])).is_none());
        assert_eq!(
            required_version(&command_of(&["oximux", "schedule", "run-once", "s"])).map(|(v, _)| v),
            Some(17)
        );
    }
}
