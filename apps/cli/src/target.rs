//! Id-prefix targets: every verb that names a session or a terminal accepts the
//! full id or a unique prefix of at least [`MIN_PREFIX`] characters, the way
//! `git` takes a short commit hash.
//!
//! Resolved here, client-side, against the existing `ls` / `term ls` replies —
//! no new host verb, so it works against every host this CLI can talk to. One
//! pass over the parsed command rewrites the id in place before dispatch, so
//! the verbs themselves only ever see full ids.
//!
//! Ids are resolved on every call and never cached: a prefix that is unique
//! today may not be tomorrow. A full UUID skips the lookup and goes to the host
//! as typed, so an unknown full id is still the host's answer (exit 1), and
//! only a prefix that matches nothing or several exits [`exit::TARGET`].

use oximux_remote_proto::proto::{Request, Response};
use serde_json::json;

use crate::cli::{
    Command, GitCommand, ModeCommand, ModelCommand, PermitCommand, TermCommand, exit,
};
use crate::client::Client;
use crate::output::Failure;

/// The shortest prefix accepted. Shorter strings must match an id exactly.
pub const MIN_PREFIX: usize = 4;

/// What an id argument names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Session,
    Terminal,
}

/// One id the host knows, with what a person needs to tell it apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub label: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    Found(String),
    NotFound,
    Ambiguous(Vec<Candidate>),
}

/// Match `arg` against the known ids: an exact id wins, then a unique prefix
/// of at least [`MIN_PREFIX`] characters — of a UUID only. Other ids are
/// complete names (`agent-1`), and a stale one must not quietly become the
/// `agent-15` that happens to start with it: they match exactly or not at all.
pub fn pick(arg: &str, candidates: &[Candidate]) -> Pick {
    if candidates.iter().any(|c| c.id == arg) {
        return Pick::Found(arg.to_owned());
    }
    let hexish = arg.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    if arg.len() < MIN_PREFIX || !hexish {
        return Pick::NotFound;
    }
    let mut hits: Vec<Candidate> = candidates
        .iter()
        .filter(|c| is_full_id(&c.id) && c.id.starts_with(arg))
        .cloned()
        .collect();
    match hits.len() {
        0 => Pick::NotFound,
        1 => Pick::Found(hits.remove(0).id),
        _ => Pick::Ambiguous(hits),
    }
}

/// A full hyphenated UUID — what both session and terminal ids are. Sent as
/// typed, without a lookup.
fn is_full_id(arg: &str) -> bool {
    let b = arg.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The id argument of a parsed command, if it takes one. `state` is not here:
/// its argument is a blackboard key, not an id.
pub fn slot(command: &mut Command) -> Option<(Kind, &mut String)> {
    fn session(s: &mut String) -> Option<(Kind, &mut String)> {
        Some((Kind::Session, s))
    }
    match command {
        Command::Attach { session: s, .. }
        | Command::Send { session: s, .. }
        | Command::Wait { session: s, .. }
        | Command::Transcript { session: s }
        | Command::Stop { session: s }
        | Command::Steer { session: s, .. } => session(s),
        Command::Permit { command } => match command {
            PermitCommand::Ls { session: s }
            | PermitCommand::Allow { session: s, .. }
            | PermitCommand::Deny { session: s, .. }
            | PermitCommand::Answer { session: s, .. } => session(s),
        },
        Command::Model { command } => match command {
            ModelCommand::Ls { session: s } | ModelCommand::Set { session: s, .. } => session(s),
        },
        Command::Mode { command: ModeCommand::Set { session: s, .. } } => session(s),
        Command::Git { command } => match command {
            GitCommand::Status { session: s }
            | GitCommand::Diff { session: s, .. }
            | GitCommand::Stage { session: s, .. }
            | GitCommand::Unstage { session: s, .. }
            | GitCommand::Commit { session: s, .. } => session(s),
        },
        Command::Term { command: TermCommand::Attach { pty } | TermCommand::Send { pty, .. } } => {
            Some((Kind::Terminal, pty))
        }
        Command::Events { session: Some(s), .. } => session(s),
        _ => None,
    }
}

/// Rewrite the command's id argument to the full id it names.
pub async fn resolve_in_place(client: &Client, command: &mut Command) -> Result<(), Failure> {
    let Some((kind, arg)) = slot(command) else { return Ok(()) };
    if is_full_id(arg) {
        return Ok(());
    }
    // A host that will not list (a scope that forbids it, an old host) still
    // gets the id as typed: resolution is a convenience, and the host remains
    // the authority on whether the id exists.
    let Some(candidates) = candidates(client, kind).await? else { return Ok(()) };
    match pick(arg, &candidates) {
        Pick::Found(id) => {
            *arg = id;
            Ok(())
        }
        // An agent-scoped caller lists only its own session, so "not found"
        // may really be "not yours": the host says which, with its own hint.
        Pick::NotFound
            if kind == Kind::Session && crate::commands::events::agent_scoped() =>
        {
            Ok(())
        }
        Pick::NotFound => Err(not_found(kind, arg)),
        Pick::Ambiguous(hits) => Err(ambiguous(kind, arg, &hits)),
    }
}

async fn candidates(client: &Client, kind: Kind) -> Result<Option<Vec<Candidate>>, Failure> {
    Ok(match kind {
        Kind::Session => match client.call(Request::ListSessions).await? {
            Response::Sessions(rows) => Some(
                rows.into_iter()
                    .map(|s| Candidate { id: s.session_id, label: s.title })
                    .collect(),
            ),
            _ => None,
        },
        Kind::Terminal => match client.call(Request::ListTerminals).await? {
            Response::Terminals(rows) => Some(
                rows.into_iter().map(|t| Candidate { id: t.pty_id, label: t.cwd }).collect(),
            ),
            _ => None,
        },
    })
}

fn noun(kind: Kind) -> (&'static str, &'static str) {
    match kind {
        Kind::Session => ("session", "oximux ls"),
        Kind::Terminal => ("terminal", "oximux term ls"),
    }
}

fn not_found(kind: Kind, arg: &str) -> Failure {
    let (what, list) = noun(kind);
    let mut steps = vec![format!("run `{list}` to list them")];
    if arg.chars().count() < MIN_PREFIX {
        steps.push(format!("a prefix needs at least {MIN_PREFIX} characters"));
    }
    Failure::new("target-not-found", exit::TARGET, format!("no {what} matches `{arg}`"))
        .with_steps(steps)
}

fn ambiguous(kind: Kind, arg: &str, hits: &[Candidate]) -> Failure {
    let (what, _) = noun(kind);
    // The candidates go to stderr in both modes, so a script that captures
    // stdout still leaves a person something to read.
    for hit in hits {
        eprintln!("  {}  {}", crate::output::printable(&hit.id), crate::output::printable(&hit.label));
    }
    Failure::new(
        "target-ambiguous",
        exit::TARGET,
        format!("`{arg}` matches {} {what}s", hits.len()),
    )
    .with_steps(["type more of the id".into()])
    .with_data(json!({
        "candidates": hits
            .iter()
            .map(|h| json!({ "id": h.id, "label": h.label }))
            .collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: &[&str]) -> Vec<Candidate> {
        ids.iter().map(|id| Candidate { id: (*id).into(), label: String::new() }).collect()
    }

    #[test]
    fn an_exact_id_wins_even_when_it_prefixes_another() {
        let known = rows(&["abc", "abcdef"]);
        assert_eq!(pick("abc", &known), Pick::Found("abc".into()));
    }

    const A: &str = "1f3a9c00-1234-4abc-9def-0123456789ab";
    const B: &str = "1f3b0000-1234-4abc-9def-0123456789ab";
    const C: &str = "77aa0000-1234-4abc-9def-0123456789ab";
    const A2: &str = "1f3a0000-1234-4abc-9def-0123456789ab";

    #[test]
    fn a_unique_prefix_resolves_and_a_shared_one_is_ambiguous() {
        let known = rows(&[A, B, C]);
        assert_eq!(pick("77aa", &known), Pick::Found(C.into()));
        assert_eq!(pick("1f3a", &known), Pick::Found(A.into()));
        let shared = rows(&[A, A2, C]);
        let Pick::Ambiguous(hits) = pick("1f3a", &shared) else {
            panic!("two ids share the prefix");
        };
        assert_eq!(hits.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), [A, A2]);
        // Under the minimum, a prefix is not tried at all.
        assert_eq!(pick("1f3", &known), Pick::NotFound);
    }

    #[test]
    fn short_or_unknown_args_match_nothing() {
        let known = rows(&[A]);
        assert_eq!(pick("1f3", &known), Pick::NotFound);
        assert_eq!(pick("ffff", &known), Pick::NotFound);
        assert_eq!(pick("1f3a9c00-1234-4abc-9def-0123456789abc", &known), Pick::NotFound);
        assert_eq!(pick("1f3a", &[]), Pick::NotFound);
    }

    #[test]
    fn a_non_uuid_id_matches_exactly_or_not_at_all() {
        // A stale `agent-1` must never become the live `agent-15`.
        let known = rows(&["agent-15", "agent-2"]);
        assert_eq!(pick("agent-1", &known), Pick::NotFound);
        assert_eq!(pick("agent-2", &known), Pick::Found("agent-2".into()));
        // Nor does a hex-looking prefix reach a non-UUID id.
        assert_eq!(pick("abcd", &rows(&["abcdef"])), Pick::NotFound);
    }

    #[test]
    fn only_a_full_uuid_skips_the_lookup() {
        assert!(is_full_id("1f3a9c00-1234-4abc-9def-0123456789ab"));
        assert!(!is_full_id("1f3a9c00"));
        assert!(!is_full_id("1f3a9c00-1234-4abc-9def-0123456789ag"));
        assert!(!is_full_id("1f3a9c00_1234-4abc-9def-0123456789ab"));
    }

    /// One argv per id site, the id always `a1b2`. The source of truth for
    /// both tests below.
    const SITES: &[&[&str]] = &[
        &["attach", "a1b2"],
        &["send", "a1b2", "hi"],
        &["wait", "a1b2", "--until", "idle"],
        &["transcript", "a1b2"],
        &["stop", "a1b2"],
        &["steer", "a1b2", "hi"],
        &["permit", "ls", "a1b2"],
        &["permit", "allow", "a1b2", "r1"],
        &["permit", "deny", "a1b2", "r1"],
        &["permit", "answer", "a1b2", "r1"],
        &["model", "ls", "a1b2"],
        &["model", "set", "a1b2", "m"],
        &["mode", "set", "a1b2", "m"],
        &["git", "status", "a1b2"],
        &["git", "diff", "a1b2", "f"],
        &["git", "stage", "a1b2", "f"],
        &["git", "unstage", "a1b2", "f"],
        &["git", "commit", "a1b2", "--message", "msg"],
        &["term", "attach", "a1b2"],
        &["term", "send", "a1b2", "ls"],
        &["events", "--session", "a1b2"],
    ];

    fn parse(args: &[&str]) -> crate::cli::Cli {
        use clap::Parser as _;
        let argv = std::iter::once("oximux").chain(args.iter().copied());
        crate::cli::Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }

    #[test]
    fn every_id_site_resolves_its_id_argument() {
        for args in SITES {
            let mut cli = parse(args);
            let (kind, arg) = slot(&mut cli.command).unwrap_or_else(|| panic!("{args:?} has no slot"));
            assert_eq!(arg, "a1b2", "{args:?}");
            let want = if args[0] == "term" { Kind::Terminal } else { Kind::Session };
            assert_eq!(kind, want, "{args:?}");
        }
        // Not ids: a blackboard key, and verbs that take none.
        for args in [&["state", "get", "a1b2"][..], &["ls"], &["events"]] {
            assert!(slot(&mut parse(args).command).is_none(), "{args:?}");
        }
    }

    /// Every argument whose help promises prefix matching is one of the sites
    /// above — so a new id verb cannot ship with the promise and no resolver.
    #[test]
    fn every_argument_that_promises_a_prefix_is_a_resolved_site() {
        use clap::CommandFactory as _;
        fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<String>) {
            for arg in cmd.get_arguments() {
                let help = arg.get_help().map(|h| h.to_string()).unwrap_or_default();
                if help.contains("unique prefix") {
                    out.push(path.trim().to_owned());
                }
            }
            for sub in cmd.get_subcommands() {
                walk(sub, &format!("{path} {}", sub.get_name()), out);
            }
        }
        let mut promised = Vec::new();
        walk(&crate::cli::Cli::command(), "", &mut promised);
        promised.sort();
        let mut sites: Vec<String> = SITES
            .iter()
            .map(|args| {
                let words = args.iter().take_while(|w| **w != "a1b2" && !w.starts_with("--"));
                words.copied().collect::<Vec<_>>().join(" ")
            })
            .collect();
        sites.sort();
        assert_eq!(promised, sites);
    }
}
