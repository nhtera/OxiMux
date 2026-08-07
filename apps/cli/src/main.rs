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

/// Leaf verbs that erase something. A typo is never nudged toward one of
/// these unless it is a single edit away: "delet" plainly meant `delete`,
/// but "dele" reading as a tip to run `state delete` is a nudge no
/// destructive verb should get. Non-destructive suggestions keep clap's own
/// looser similarity rule.
const DESTRUCTIVE_VERBS: &[&str] = &["rm", "delete", "pair-rm"];

/// Levenshtein distance, the plain O(n·m) row-rolling form. Inputs are
/// subcommand-sized (a dozen bytes), so clarity beats cleverness here.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let subst = prev[j] + usize::from(ca != cb);
            row.push(subst.min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

/// Drop a did-you-mean that points at a destructive verb the user was not
/// one edit away from typing. The rest of the error — including looser
/// suggestions toward non-destructive verbs — is clap's own and unchanged.
fn tighten_destructive_suggestions(mut err: clap::Error) -> clap::Error {
    use clap::error::{ContextKind, ContextValue};
    if err.kind() != clap::error::ErrorKind::InvalidSubcommand {
        return err;
    }
    let Some(ContextValue::String(typed)) = err.get(ContextKind::InvalidSubcommand) else {
        return err;
    };
    let typed = typed.clone();
    let Some(ContextValue::Strings(suggested)) = err.get(ContextKind::SuggestedSubcommand) else {
        return err;
    };
    let kept: Vec<String> = suggested
        .iter()
        .filter(|s| !DESTRUCTIVE_VERBS.contains(&s.as_str()) || edit_distance(&typed, s) <= 1)
        .cloned()
        .collect();
    if kept.len() != suggested.len() {
        // An emptied list must clear the context, not shrink it: clap renders
        // both `Strings(vec![])` and `None` as a dangling "tip:" line.
        if kept.is_empty() {
            err.remove(ContextKind::SuggestedSubcommand);
        } else {
            err.insert(ContextKind::SuggestedSubcommand, ContextValue::Strings(kept));
        }
    }
    err
}

fn main() -> std::process::ExitCode {
    // Usage errors exit 2 here, printing clap's own message — before any I/O.
    let args = match Cli::try_parse() {
        Ok(args) => args,
        // `exit()` keeps clap's own behavior: usage errors print to stderr and
        // exit 2, while `--help`/`--version` print to stdout and exit 0.
        Err(err) => tighten_destructive_suggestions(err).exit(),
    };

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
        // Offline, and deliberately NOT wrapped in the `--json` envelope: the
        // output is a shell script destined for a file, and a JSON wrapper
        // would make `oximux completions zsh > _oximux` write something no
        // shell can source.
        Command::Completions { shell } => {
            use clap::CommandFactory as _;
            use std::io::Write as _;
            let mut cmd = Cli::command();
            // Into a buffer, not straight to stdout: `clap_complete` PANICS if
            // its writer errors, and the writer errors the moment a reader goes
            // away — so `oximux completions zsh | head` printed a Rust panic and
            // a backtrace note. Piping a 150 KB script into a pager or `head` is
            // an ordinary thing to do while checking it.
            let mut script = Vec::new();
            // `oximux`, not the cargo target name `oximux-cli` — the completion
            // has to match what users actually type.
            clap_complete::generate(*shell, &mut cmd, "oximux", &mut script);
            match std::io::stdout().write_all(&script) {
                Ok(()) => cli::exit::OK,
                // A closed pipe is the reader's choice, not this command's
                // failure: exit 0, silently, as `cat` and `yes` do.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => cli::exit::OK,
                Err(e) => render(
                    args.json,
                    Err(output::Failure::new(
                        "io",
                        cli::exit::ERROR,
                        format!("could not write the completion script: {e}"),
                    )),
                ),
            }
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
        // v19: the watch cursor. `state watch` sends `StateWatchFrom`, an
        // ordinal a v18 host answers by dropping the connection — so it needs
        // its own floor above the rest of its family, exactly as
        // `schedule run-once` does. Must stay ABOVE the catch-all `State` arm
        // below, which would otherwise match it first and claim v18.
        Command::State { command: StateCommand::Watch { .. } } => Some((19, "state watch")),
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

/// Usage checks and stdin resolution, done BEFORE a socket is touched.
///
/// Both of these are the caller's mistake, and both used to be discovered too
/// late to say so: they sat past `resolve_and_connect`, so on a machine with no
/// host running the answer was "the host is not reachable" (exit 3) — a
/// diagnosis about the environment for a problem in the argv. A usage error
/// must not depend on a host being up.
///
/// Resolving `-` here also gives it one home. The verbs receive a prompt that
/// is already text, so neither has to know stdin exists.
fn precheck(command: &mut Command, json_mode: bool) -> Result<(), output::Failure> {
    // `term attach` streams the remote screen's raw bytes to stdout — that IS
    // its output — so a JSON envelope appended afterwards leaves a stream no
    // parser can read. Refuse rather than emit the corrupt mixture.
    if json_mode
        && let Command::Term { command: TermCommand::Attach { .. } } = command
    {
        return Err(output::Failure::new(
            "unsupported-in-json",
            cli::exit::USAGE,
            "`term attach` cannot be used with --json: it streams raw terminal bytes to stdout",
        )
        .with_steps([
            "drop --json to attach interactively".into(),
            "for machine-readable terminal state, use `oximux --json term ls`".into(),
        ]));
    }
    // A malformed `--input` is a mistake in the argv, so it must be caught
    // here rather than inside the verb: by the time `permit allow` runs, a
    // client has already been built, and on a machine with no host up the
    // caller was told "the host is not reachable" (exit 3) for bad JSON they
    // could have been shown without any host at all. Parsed twice — once to
    // validate, once for real — because the rule lives in one function and a
    // few bytes of JSON is not worth threading state through the dispatcher to
    // avoid.
    if let Command::Permit { command: PermitCommand::Allow { input: Some(raw), .. } } = command {
        commands::permit::parse_input_override(raw)?;
    }
    match command {
        Command::Run { prompt, .. } | Command::Send { prompt, .. } => {
            *prompt = commands::resolve_prompt(std::mem::take(prompt))?;
        }
        _ => {}
    }
    Ok(())
}

fn host_verb(mut args: Cli) -> u8 {
    // Before the runtime, before the socket: a usage error costs nothing and
    // must not be reported as an unreachable host.
    if let Err(failure) = precheck(&mut args.command, args.json) {
        return render(args.json, Err(failure));
    }
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
                Command::Run {
                    prompt,
                    agent,
                    model,
                    mode,
                    cwd,
                    worktree,
                    output_schema,
                    turn_timeout,
                    stalled_after,
                    bg,
                } => {
                    let run_args = commands::run::RunArgs {
                        prompt,
                        agent,
                        model,
                        mode,
                        cwd,
                        worktree,
                        output_schema,
                        turn_timeout,
                        stalled_after,
                        bg,
                    };
                    commands::run::run(&client, run_args, json_mode).await
                }
                Command::Attach { session, from } => {
                    commands::attach::run(&client, &session, from, json_mode).await
                }
                Command::Send {
                    session,
                    prompt,
                    output_schema,
                    turn_timeout,
                    stalled_after,
                    no_wait,
                } => {
                    let send_args = commands::send::SendArgs {
                        output_schema: output_schema.as_deref(),
                        no_wait,
                        turn_timeout,
                        stalled_after,
                        json_mode,
                    };
                    commands::send::run_checked(&client, &session, prompt, send_args).await
                }
                Command::Wait { session, until, stalled_after } => {
                    commands::wait::run(
                        &client,
                        &session,
                        until,
                        timeout,
                        stalled_after,
                        json_mode,
                    )
                    .await
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
                    PermitCommand::Allow { session, request, input } => {
                        commands::permit::allow(
                            &client,
                            &session,
                            request.as_deref(),
                            input.as_deref(),
                        )
                        .await
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
                    // `--json` is refused in `precheck`, before any connection.
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
                    StateCommand::Watch { prefix, since } => {
                        commands::state::watch(&client, prefix, since, json_mode).await
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
                | Command::Completions { .. }
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
            // v19 sits above its own family: only `state watch` sends the
            // cursor request, and gating the whole family would strand a v18
            // host's perfectly serviceable get/set/delete.
            (vec!["oximux", "state", "watch"], Some(19)),
            (vec!["oximux", "state", "set", "k", "1"], Some(18)),
            (vec!["oximux", "state", "delete", "k"], Some(18)),
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

    /// `run --mode` reaches the parser, and is absent when not given.
    ///
    /// A wiring guard rather than a parser test: the flag exists so a scripted
    /// `run` can start a session in a mode that does not stop on every tool.
    /// Dropped anywhere between clap and `RunArgs` it would fail silently — the
    /// session would simply take the backend default and the run would park on
    /// the first permission request, which is the failure this flag exists to
    /// prevent and is indistinguishable from a slow agent.
    #[test]
    fn run_carries_the_permission_mode_through_to_its_args() {
        let Command::Run { mode, model, .. } =
            command_of(&["oximux", "run", "hi", "--mode", "acceptEdits"])
        else {
            panic!("`run` parses");
        };
        assert_eq!(mode.as_deref(), Some("acceptEdits"));
        assert_eq!(model, None, "--mode must not be confused with --model");

        let Command::Run { mode, .. } = command_of(&["oximux", "run", "hi"]) else {
            panic!("`run` parses");
        };
        assert_eq!(mode, None, "no --mode means the backend default, not a guess");
    }

    /// `--turn-timeout` reaches both streaming verbs, and is refused on the two
    /// flags that never wait for a turn.
    ///
    /// The refusal matters more than the plumbing. `--bg` and `--no-wait` return
    /// before a turn exists, so accepting a turn budget alongside them would
    /// take an argument and silently do nothing — the precise failure this flag
    /// was added to end. Clap's `conflicts_with` makes it exit 2 instead.
    #[test]
    fn turn_timeout_reaches_both_verbs_and_is_refused_where_no_turn_is_awaited() {
        let Command::Run { turn_timeout, .. } =
            command_of(&["oximux", "run", "hi", "--turn-timeout", "30"])
        else {
            panic!("`run` parses");
        };
        assert_eq!(turn_timeout, Some(30));

        let Command::Send { turn_timeout, .. } =
            command_of(&["oximux", "send", "s1", "hi", "--turn-timeout", "30"])
        else {
            panic!("`send` parses");
        };
        assert_eq!(turn_timeout, Some(30));

        // Absent by default: the stream stays unbounded unless asked, so an
        // interactive `run` behind a thinking agent is not cut off.
        let Command::Run { turn_timeout, .. } = command_of(&["oximux", "run", "hi"]) else {
            panic!("`run` parses");
        };
        assert_eq!(turn_timeout, None);

        for argv in [
            ["oximux", "run", "hi", "--bg", "--turn-timeout", "30"].as_slice(),
            ["oximux", "send", "s1", "hi", "--no-wait", "--turn-timeout", "30"].as_slice(),
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "a turn budget alongside a verb that awaits no turn must be refused: {argv:?}",
            );
        }
    }

    /// `--json` with `term attach` is refused as a usage error, with no host.
    ///
    /// The placement is the point. This check used to sit past
    /// `resolve_and_connect`, so on a machine with no host running it never
    /// ran — the caller got "the host is not reachable" (exit 3), a diagnosis
    /// about the environment for a mistake in the argv. `precheck` takes no
    /// client for exactly that reason, so this test cannot pass by accident on
    /// a machine that happens to have a host up.
    #[test]
    fn json_with_term_attach_is_a_usage_error_before_any_connection() {
        let mut command = command_of(&["oximux", "term", "attach", "pty-1"]);
        let failure = precheck(&mut command, true).expect_err("refused under --json");
        assert_eq!(failure.exit, cli::exit::USAGE);
        assert_eq!(failure.code, "unsupported-in-json");
        assert!(!failure.next_steps.is_empty(), "and says what to do instead");

        // Without --json it is an ordinary interactive attach.
        let mut command = command_of(&["oximux", "term", "attach", "pty-1"]);
        assert!(precheck(&mut command, false).is_ok());
        // And the guard is scoped to attach — `term ls` is machine-readable.
        let mut command = command_of(&["oximux", "term", "ls"]);
        assert!(precheck(&mut command, true).is_ok(), "`term ls` must still serve --json");
    }

    /// A malformed `--input` is refused before any connection.
    ///
    /// Same placement rule as the `--json term attach` guard above, and the
    /// same reason: `permit allow` runs only after a client is built, so on a
    /// machine with no host up the caller was told "the host is not reachable"
    /// (exit 3) for JSON that could have been rejected without any host at all.
    #[test]
    fn a_malformed_permit_input_is_a_usage_error_before_any_connection() {
        for bad in ["not json", "[\"a\"]", "42"] {
            let mut command =
                command_of(&["oximux", "permit", "allow", "s1", "--input", bad]);
            let failure = precheck(&mut command, false).expect_err(bad);
            assert_eq!(failure.exit, cli::exit::USAGE, "{bad}");
            assert_eq!(failure.code, "bad-input", "{bad}");
        }

        // A well-formed object passes through to the host, as does no flag.
        let mut command = command_of(&[
            "oximux", "permit", "allow", "s1", "--input", "{\"command\":\"ls\"}",
        ]);
        assert!(precheck(&mut command, false).is_ok());
        let mut command = command_of(&["oximux", "permit", "allow", "s1"]);
        assert!(precheck(&mut command, false).is_ok());
    }

    /// A literal prompt survives `precheck` unchanged, on both verbs.
    ///
    /// The stdin path needs a pipe and so is covered end-to-end rather than
    /// here; what this pins is that the common case is not disturbed by the
    /// resolution step now sitting in front of it.
    #[test]
    fn precheck_leaves_an_ordinary_prompt_alone() {
        let mut command = command_of(&["oximux", "run", "do the thing"]);
        precheck(&mut command, false).expect("ok");
        let Command::Run { prompt, .. } = &command else { panic!("run") };
        assert_eq!(prompt, "do the thing");

        let mut command = command_of(&["oximux", "send", "s1", "do the thing"]);
        precheck(&mut command, false).expect("ok");
        let Command::Send { prompt, .. } = &command else { panic!("send") };
        assert_eq!(prompt, "do the thing");
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

    /// A typo two edits from a destructive verb is not nudged toward it —
    /// "dele" must not read as an invitation to `state delete` — while one
    /// edit away ("delet") plainly meant it and keeps the tip.
    #[test]
    fn a_loose_typo_is_not_nudged_toward_a_destructive_verb() {
        let err = Cli::try_parse_from(["oximux", "state", "dele"]).unwrap_err();
        let msg = tighten_destructive_suggestions(err).to_string();
        assert!(
            !msg.contains("delete"),
            "two edits from `delete` must not suggest it, got:\n{msg}"
        );
        assert!(!msg.contains("tip:"), "an emptied tip renders as no tip at all, got:\n{msg}");

        let err = Cli::try_parse_from(["oximux", "state", "delet"]).unwrap_err();
        let msg = tighten_destructive_suggestions(err).to_string();
        assert!(msg.contains("delete"), "one edit away keeps the tip, got:\n{msg}");
    }

    /// The filter only tightens destructive tips: everything else — other
    /// error kinds, suggestions toward harmless verbs — is clap's own,
    /// untouched.
    #[test]
    fn non_destructive_suggestions_keep_claps_own_rule() {
        let err = Cli::try_parse_from(["oximux", "state", "watc"]).unwrap_err();
        let msg = tighten_destructive_suggestions(err).to_string();
        assert!(msg.contains("watch"), "harmless tips stay loose, got:\n{msg}");

        let err = Cli::try_parse_from(["oximux", "run"]).unwrap_err();
        let before = err.to_string();
        assert_eq!(tighten_destructive_suggestions(err).to_string(), before);
    }

    #[test]
    fn edit_distance_is_levenshtein() {
        assert_eq!(edit_distance("dele", "delete"), 2);
        assert_eq!(edit_distance("delet", "delete"), 1);
        assert_eq!(edit_distance("rm", "rm"), 0);
        assert_eq!(edit_distance("", "rm"), 2);
        assert_eq!(edit_distance("rn", "rm"), 1);
    }
}
