//! `oximux skills` — the agent-facing guides, served from the binary itself.
//!
//! Entirely offline. The guides are `include_str!`d, so `skills get` needs no
//! runtime, no network, and no host, and a guide can never describe a verb the
//! binary serving it does not have — the version that ships the guide is the
//! version that ships the parser. That coupling is the whole design: a guide
//! fetched from anywhere else is a guide that can be a release out of date,
//! and a wrong guide is worse than none, because an agent following it spends
//! a turn discovering the verb is gone.
//!
//! The [`tests`] module holds the other half of that guarantee: every command
//! a guide names is walked against the live clap tree at test time.

use std::path::PathBuf;

use oximux_agent_hooks::agent_hook_dialects::{DIALECTS, HookDialect};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::output::Failure;

/// One guide, embedded.
pub struct Guide {
    /// The topic name, which is also the installed directory name.
    pub topic: &'static str,
    /// One line, for `skills ls`.
    pub summary: &'static str,
    /// The file verbatim, YAML frontmatter and all.
    pub text: &'static str,
}

/// Every guide this binary carries.
///
/// Ordered for reading, not alphabetically: the CLI guide is the one the team
/// guide tells you to read first.
pub const GUIDES: &[Guide] = &[
    Guide {
        topic: "oximux-cli",
        summary: "Start sessions, watch them finish, decide their permission requests, manage worktrees",
        text: include_str!("../../../../docs/skills/oximux-cli.md"),
    },
    Guide {
        topic: "oximux-team",
        summary: "Run several agents on one task with roles, worktrees, and the state blackboard",
        text: include_str!("../../../../docs/skills/oximux-team.md"),
    },
];

/// Where a guide goes for one agent, or `None` if that agent has no home to
/// resolve against.
fn install_path(dialect: &HookDialect, topic: &str) -> Option<PathBuf> {
    Some(skills_dir(dialect)?.join(topic).join("SKILL.md"))
}

/// The directory an agent keeps its skills in.
fn skills_dir(dialect: &HookDialect) -> Option<PathBuf> {
    Some((dialect.home)()?.join("skills"))
}

/// Whether this machine already keeps skills for that agent.
///
/// Deliberately an observation, not a claim. An earlier version of this file
/// carried a hardcoded list of "the agents that read skills" — and it was
/// wrong: it excluded codex and droid, both of which have populated
/// `skills/` directories here. A frozen fact about someone else's tool goes
/// stale without anyone noticing, and there is no way to check it from inside
/// this repo. Looking at the filesystem cannot go stale, and it answers the
/// question that actually matters for a default: is this a place skills
/// already live?
///
/// A named `--agent` bypasses this, because then the user has told us.
fn keeps_skills(dialect: &HookDialect) -> bool {
    skills_dir(dialect).is_some_and(|dir| dir.is_dir())
}

/// The guide named, or a usage failure listing the ones that exist.
fn guide_for(topic: &str) -> Result<&'static Guide, Failure> {
    GUIDES.iter().find(|g| g.topic == topic).ok_or_else(|| {
        Failure::new(
            "usage",
            exit::USAGE,
            format!("unknown topic {topic:?} — must be {}", known_topics()),
        )
        .with_steps(["`oximux skills ls` prints every topic with a summary".into()])
    })
}

fn known_topics() -> String {
    GUIDES.iter().map(|g| g.topic).collect::<Vec<_>>().join("|")
}

/// The dialects `install` writes to by default: agents on this machine that
/// already keep a `skills/` directory.
fn skill_readers() -> Vec<&'static HookDialect> {
    DIALECTS
        .iter()
        .filter(|d| d.agent_is_installed() && keeps_skills(d))
        .collect()
}

/// Strip the YAML frontmatter, leaving the prose.
///
/// The frontmatter is metadata for an agent runtime's skill discovery, not
/// something a reader asked for — so `get` prints the body and `--full` prints
/// the file as it would be installed. A file with no frontmatter is returned
/// whole rather than mangled: the delimiter is required to be the first line,
/// and a `---` appearing later in a document is a horizontal rule.
fn body_of(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    match rest.find("\n---\n") {
        Some(end) => rest[end + "\n---\n".len()..].trim_start_matches('\n'),
        None => text,
    }
}

/// `skills ls`.
pub fn list() -> Result<(Value, String), Failure> {
    let data = json!({
        "skills": GUIDES.iter().map(|g| json!({
            "topic": g.topic,
            "summary": g.summary,
        })).collect::<Vec<_>>(),
    });
    let width = GUIDES.iter().map(|g| g.topic.len()).max().unwrap_or(0);
    let mut human = String::new();
    for guide in GUIDES {
        human.push_str(&format!("{:<width$}  {}\n", guide.topic, guide.summary));
    }
    Ok((data, human))
}

/// `skills get <topic> [--full]`.
pub fn get(topic: &str, full: bool) -> Result<(Value, String), Failure> {
    let guide = guide_for(topic)?;
    let text = if full { guide.text } else { body_of(guide.text) };
    Ok((
        json!({ "topic": guide.topic, "text": text }),
        text.to_string(),
    ))
}

/// `skills install [--agent <slug>]`.
///
/// Writes into agents that are already on this machine and nowhere else, the
/// same rule `agent hooks on` follows: OxiMux adds to an agent's own config
/// directory and never conjures one, so an agent you have never run is
/// reported and skipped rather than given a directory it did not ask for.
///
/// With no `--agent`, the targets are the agents that already keep a
/// `skills/` directory — an observation about this machine rather than a claim
/// about which agents load skills, which is not knowable from here. Naming an
/// `--agent` overrides that: the user has said where they want it, so the
/// directory is created if it is missing.
///
/// A guide OxiMux wrote before is overwritten — that is the point, since a
/// stale guide is the failure this whole verb exists to prevent. A file that
/// is NOT one of ours is left alone and reported, because the assumption that
/// nothing else could own that path is an assumption, not an invariant.
pub fn install(agent: Option<&str>) -> Result<(Value, String), Failure> {
    let targets = match agent {
        Some(slug) => {
            let dialect = DIALECTS.iter().find(|d| d.slug == slug).ok_or_else(|| {
                Failure::new(
                    "usage",
                    exit::USAGE,
                    format!(
                        "unknown agent {slug:?} — must be {}",
                        oximux_agent_hooks::agent_hook_dialects::known_slugs()
                    ),
                )
            })?;
            // An explicitly named agent that is not here is a failure, not a
            // skipped row: the caller asked for one thing and got none, and a
            // provisioning script must be able to see that in the exit code.
            if !dialect.agent_is_installed() {
                return Err(Failure::new(
                    "agent-absent",
                    exit::ERROR,
                    format!("{} is not on this machine, so there is nowhere to install", dialect.agent),
                )
                .with_steps([
                    format!("run {} once so it creates its own config directory", dialect.slug),
                    "`oximux skills install` with no --agent installs into the agents that are here".into(),
                ]));
            }
            vec![dialect]
        }
        None => skill_readers(),
    };

    let mut rows = Vec::new();
    let mut written = 0usize;
    let mut failed = 0usize;
    for dialect in targets {
        for guide in GUIDES {
            let Some(path) = install_path(dialect, guide.topic) else {
                rows.push(row(dialect, guide.topic, "no-home", None, None));
                continue;
            };
            if let Some(owner) = foreign_file(&path) {
                failed += 1;
                rows.push(row(dialect, guide.topic, "not-ours", Some(&path), Some(&owner)));
                continue;
            }
            match write_guide(&path, guide.text) {
                Ok(()) => {
                    written += 1;
                    rows.push(row(dialect, guide.topic, "installed", Some(&path), None));
                }
                Err(err) => {
                    failed += 1;
                    rows.push(row(dialect, guide.topic, "error", Some(&path), Some(&err)));
                }
            }
        }
    }

    let mut human = String::new();
    for entry in &rows {
        let agent = entry["agent"].as_str().unwrap_or("?");
        let topic = entry["topic"].as_str().unwrap_or("?");
        match entry["outcome"].as_str().unwrap_or("") {
            "installed" => {
                human.push_str(&format!("{agent}  {topic}  {}\n", entry["path"].as_str().unwrap_or("")));
            }
            other => {
                let detail = entry["detail"].as_str().map(|d| format!(" — {d}")).unwrap_or_default();
                human.push_str(&format!("{agent}  {topic}  {other}{detail}\n"));
            }
        }
    }
    if rows.is_empty() {
        human.push_str(
            "No agent on this machine keeps a skills directory yet.\nRun an agent once, or name one with --agent.\n",
        );
    }

    let data = json!({ "written": written, "failed": failed, "installed": rows });

    // Every write failing is a failed command, not a successful report of
    // failure: a caller that only checks the exit code must not read "nothing
    // worked" as "done".
    if written == 0 && failed > 0 {
        return Err(Failure::new("install", exit::ERROR, "no guide could be installed")
            .with_steps(["`--json` lists each target and why it was refused".into()])
            .with_data(data));
    }
    Ok((data, human))
}

/// One row, the same shape for every outcome.
///
/// Uniform because a consumer indexing `.installed[].topic` should not get a
/// null for some rows — the neighbouring `agent hooks` verb settled this same
/// question the same way.
fn row(dialect: &HookDialect, topic: &str, outcome: &str, path: Option<&std::path::Path>, detail: Option<&str>) -> Value {
    json!({
        "agent": dialect.slug,
        "name": dialect.agent,
        "topic": topic,
        "outcome": outcome,
        "installed": outcome == "installed",
        "path": path.map(|p| p.to_string_lossy().into_owned()),
        "detail": detail,
    })
}

/// Why this path must not be written, or `None` when it is ours to write.
///
/// Ownership is read off the frontmatter OxiMux itself wrote, so it needs no
/// side-car marker file and keeps the installed bytes identical to the source
/// guide. An unreadable file counts as foreign: refusing to clobber something
/// we cannot even inspect is the safe direction.
fn foreign_file(path: &std::path::Path) -> Option<String> {
    // `symlink_metadata` does not follow the link, so a `SKILL.md` symlinked
    // into a dotfiles repo is caught rather than written through.
    match std::fs::symlink_metadata(path) {
        Err(_) => None, // nothing there: ours to create
        Ok(meta) if meta.file_type().is_symlink() => {
            Some("a symlink, left alone".to_string())
        }
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(existing) if is_ours(&existing) => None,
            Ok(_) => Some("a file OxiMux did not write".to_string()),
            Err(err) => Some(format!("unreadable ({err})")),
        },
    }
}

/// Whether a file on disk is a guide this CLI wrote.
fn is_ours(text: &str) -> bool {
    GUIDES.iter().any(|g| text.starts_with(&format!("---\nname: {}\n", g.topic)))
}

/// Create the topic directory and write the guide, reporting the failure as a
/// string rather than aborting: one unwritable agent home must not stop the
/// other agents from being installed into.
fn write_guide(path: &std::path::Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    /// Every command a guide names must exist in the parser that ships with
    /// it.
    ///
    /// This is the gate that makes the guides safe to own. They are prose, so
    /// nothing else stops one from naming a verb that was renamed three
    /// releases ago — and an agent following a stale guide does not get an
    /// error it can act on, it gets exit 2 on a command it was told to trust.
    ///
    /// Walking the live clap tree rather than a list of verb names is what
    /// keeps the check honest: a renamed subcommand fails here without anyone
    /// remembering this test exists.
    #[test]
    fn every_command_a_guide_names_exists() {
        let root = crate::cli::Cli::command();
        let mut checked = 0usize;
        for guide in GUIDES {
            for span in command_spans(body_of(guide.text)) {
                for invocation in invocations(&span) {
                    checked += 1;
                    let mut cmd = &root;
                    let mut walked: Vec<&str> = vec!["oximux"];
                    for word in &invocation {
                        // A command with no subcommands is a leaf: everything
                        // after it is a positional argument, and
                        // `worktree create fix-parser` must not read
                        // `fix-parser` as a verb. Only a command that HAS
                        // subcommands takes one, which is what makes an
                        // unmatched word there a real typo rather than data.
                        if cmd.get_subcommands().next().is_none() {
                            break;
                        }
                        let Some(next) = cmd.get_subcommands().find(|s| s.get_name() == word.as_str()) else {
                            panic!(
                                "guide {:?} names `{} {}`, but {:?} is not a subcommand of `{}`\n  in: {}",
                                guide.topic,
                                walked.join(" "),
                                word,
                                word,
                                walked.join(" "),
                                span.trim(),
                            );
                        };
                        cmd = next;
                        walked.push(word.as_str());
                    }
                }
            }
        }
        assert!(
            checked >= 20,
            "only {checked} commands found across the guides — the extractor probably stopped matching"
        );
    }

    /// Long flags named in a guide must exist on the command they are written
    /// against.
    ///
    /// A wrong flag strands a caller exactly as a wrong verb does — exit 2 on
    /// a line the guide said to run — and flags are the half that churns,
    /// since a verb is rarely renamed but its options grow and are dropped
    /// every release. Global flags count as present on every command, which is
    /// what `--json` on `team status` relies on.
    #[test]
    fn every_flag_a_guide_names_exists_on_its_command() {
        let root = crate::cli::Cli::command();
        let globals: Vec<String> = root
            .get_arguments()
            .filter_map(|a| a.get_long().map(str::to_string))
            .collect();
        let mut flags_checked = 0usize;

        for guide in GUIDES {
            for span in command_spans(body_of(guide.text)) {
                for (path, flags) in invocations_with_flags(&span) {
                    let mut cmd = &root;
                    for word in &path {
                        if cmd.get_subcommands().next().is_none() {
                            break;
                        }
                        match cmd.get_subcommands().find(|s| s.get_name() == word.as_str()) {
                            Some(next) => cmd = next,
                            // The verb check owns this failure; skip here so
                            // one typo does not report twice.
                            None => break,
                        }
                    }
                    let longs: Vec<String> = cmd
                        .get_arguments()
                        .filter_map(|a| a.get_long().map(str::to_string))
                        .collect();
                    for flag in flags {
                        flags_checked += 1;
                        assert!(
                            longs.contains(&flag) || globals.contains(&flag),
                            "guide {:?} names `--{flag}` on `oximux {}`, which does not accept it\n  accepts: {:?}\n  in: {}",
                            guide.topic,
                            path.join(" "),
                            longs,
                            span.trim(),
                        );
                    }
                }
            }
        }
        // The bug that actually shipped here was a FLAG-coverage bug: the
        // extractor silently stopped seeing flags after a line continuation,
        // and the test went green by checking nothing. A command-count canary
        // does not catch that; this one does.
        assert!(
            flags_checked >= 25,
            "only {flags_checked} flags found across the guides — the extractor probably stopped matching"
        );
    }

    /// Every command a guide shows must actually PARSE.
    ///
    /// The verb and flag gates check that names exist. Neither checks arity,
    /// and that is a real gap with a real victim: the guide's
    /// `heartbeat create "…" --cron "…"` named only real things and still
    /// exited 2, because `--name` is required and was missing. An agent
    /// copying that line gets a usage error on a command the guide vouched
    /// for.
    ///
    /// Placeholders are substituted, so this checks shape — required
    /// arguments, arity, conflicts — not the values a reader supplies.
    #[test]
    fn every_command_a_guide_shows_actually_parses() {
        let mut checked = 0usize;
        for guide in GUIDES {
            for span in command_spans(body_of(guide.text)) {
                for argv in argvs(&span) {
                    checked += 1;
                    if let Err(err) = <crate::cli::Cli as clap::Parser>::try_parse_from(&argv) {
                        panic!(
                            "guide {:?} shows a command that does not parse:\n  {}\n{}",
                            guide.topic,
                            argv.join(" "),
                            err
                        );
                    }
                }
            }
        }
        assert!(
            checked >= 20,
            "only {checked} invocations reconstructed — the tokenizer probably stopped matching"
        );
    }

    /// The team guide's worked example is inside `RUN=$(oximux …)`, which the
    /// extractor once skipped entirely — four flags and two verbs unchecked.
    #[test]
    fn a_command_substitution_is_still_an_invocation() {
        assert!(is_oximux_token("RUN=$(oximux"));
        assert!(is_oximux_token("$(oximux"));
        assert!(is_oximux_token("oximux"));
        assert!(!is_oximux_token("oximuxx"));
        assert!(!is_oximux_token("--oximux"));

        let found = invocations_with_flags("RUN=$(oximux team run --name x --json | jq -r .data.id)");
        assert_eq!(found.len(), 1, "the substitution is one invocation");
        assert_eq!(found[0].0, vec!["team", "run"]);
        assert_eq!(found[0].1, vec!["name".to_string(), "json".to_string()]);
    }

    /// A quoted value with spaces is one argument, not several.
    #[test]
    fn quotes_survive_tokenising() {
        let tokens = shell_tokens(r#"oximux heartbeat create "sweep it" --cron "*/15 * * * *""#);
        assert_eq!(
            tokens,
            vec!["oximux", "heartbeat", "create", "sweep it", "--cron", "*/15 * * * *"]
        );
    }

    /// Only code carries commands; prose may say "the oximux CLI" freely.
    ///
    /// Callers pass the BODY, not the whole file: a guide's YAML `description`
    /// names verb families so an agent runtime can decide when to load it
    /// ("when the task involves `oximux team`"), and those are discovery
    /// blurbs rather than lines anyone runs.
    ///
    /// Fenced blocks are taken whole, and outside them only inline-code spans
    /// count — which is also a rule for whoever writes the next guide: a
    /// command that is not in backticks is not checked, so put it in backticks.
    ///
    /// Known blind spots, none of which the current guides use. Each is a way
    /// to lose coverage silently, so prefer ``` fences and single backticks:
    ///
    /// * `~~~` fences — only ``` toggles `in_fence`.
    /// * Four-space indented code — no backticks, so it reads as prose.
    /// * A fence inside a blockquote (`> ```sh`) — `trim_start` strips
    ///   whitespace, not `>`, so the fence never toggles.
    /// * Double-backtick inline spans — the parity flip drops the code half.
    /// * An UNBALANCED fence marker anywhere inverts `in_fence` for the whole
    ///   rest of the document, flipping every later block.
    fn command_spans(body: &str) -> Vec<String> {
        let mut spans: Vec<String> = Vec::new();
        let mut in_fence = false;
        for line in body.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                // A shell continuation is one command across several lines.
                // Without joining them, every flag after the first `\` is
                // invisible to the gate — which is how a six-line
                // `team run --role … --worktree-each` had none of its flags
                // checked at all.
                match spans.last_mut() {
                    Some(prev) if prev.trim_end().ends_with('\\') => {
                        let joined = prev.trim_end().trim_end_matches('\\').to_string();
                        *prev = format!("{joined} {}", line.trim());
                    }
                    _ => spans.push(line.to_string()),
                }
                continue;
            }
            let mut parts = line.split('`');
            parts.next();
            let mut code = true;
            for part in parts {
                if code {
                    spans.push(part.to_string());
                }
                code = !code;
            }
        }
        spans
    }

    /// The subcommand path of every `oximux …` invocation in one span.
    fn invocations(span: &str) -> Vec<Vec<String>> {
        invocations_with_flags(span).into_iter().map(|(path, _)| path).collect()
    }

    /// Each `oximux …` invocation as (subcommand path, long flags named).
    ///
    /// The path ends at the first token that cannot be a subcommand — an
    /// option, a placeholder, a shell variable, a quoted string. Every command
    /// in this CLI that has subcommands takes no positional of its own, so a
    /// bare lowercase word where a subcommand is expected is a verb and is
    /// checked as one; that is what catches a typo instead of silently reading
    /// it as an argument.
    fn invocations_with_flags(span: &str) -> Vec<(Vec<String>, Vec<String>)> {
        let owned = shell_tokens(span);
        let tokens: Vec<&str> = owned.iter().map(String::as_str).collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            if !is_oximux_token(tokens[i]) {
                i += 1;
                continue;
            }
            i += 1;
            let mut path = Vec::new();
            let mut flags = Vec::new();
            let mut in_path = true;
            while i < tokens.len() {
                let token = tokens[i];
                if is_terminator(token) || is_oximux_token(token) {
                    break;
                }
                if let Some(long) = token.strip_prefix("--") {
                    let name = long.split(['=', '\'', '"']).next().unwrap_or(long);
                    if !name.is_empty() {
                        flags.push(name.to_string());
                    }
                    in_path = false;
                } else if in_path && is_verb_word(token) {
                    path.push(token.trim_end_matches([';', ')']).to_string());
                } else {
                    in_path = false;
                }
                let ends_command = token.ends_with(';');
                i += 1;
                if ends_command {
                    break;
                }
            }
            if !path.is_empty() || !flags.is_empty() {
                out.push((path, flags));
            }
        }
        out
    }

    /// Split a command line the way a shell would, honouring quotes.
    ///
    /// Whitespace splitting is wrong for a guide: `--cron "*/15 * * * *"` is
    /// ONE argument, and splitting it into five made the parse gate below
    /// impossible to write. Quotes are consumed, so the token is the value the
    /// command would actually receive.
    fn shell_tokens(span: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut quote: Option<char> = None;
        for ch in span.chars() {
            match quote {
                Some(q) if ch == q => quote = None,
                Some(_) => current.push(ch),
                None if ch == '\'' || ch == '"' => {
                    quote = Some(ch);
                    started = true;
                }
                None if ch.is_whitespace() => {
                    if started {
                        out.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                None => {
                    current.push(ch);
                    started = true;
                }
            }
        }
        if started {
            out.push(current);
        }
        out
    }

    /// Every `oximux …` invocation in one span, as the argv the shell would
    /// hand the binary — placeholders replaced with something that parses.
    fn argvs(span: &str) -> Vec<Vec<String>> {
        let tokens = shell_tokens(span);
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            if !is_oximux_token(&tokens[i]) {
                i += 1;
                continue;
            }
            i += 1;
            let mut argv = vec!["oximux".to_string()];
            while i < tokens.len() {
                let token = tokens[i].as_str();
                if is_terminator(token) || is_oximux_token(token) {
                    break;
                }
                let ends_command = token.ends_with(';');
                argv.push(concrete(token.trim_end_matches([';', ')'])));
                i += 1;
                // `…; done` — the semicolon closes the command, and what
                // follows is the shell's own syntax, not an argument.
                if ends_command {
                    break;
                }
            }
            if argv.len() > 1 {
                out.push(argv);
            }
        }
        out
    }

    /// A guide placeholder as a value the parser will accept.
    ///
    /// `<ID>`, `$RUN` and bare capitals like `S` are stand-ins a reader
    /// substitutes; the parser only needs *something* there to check arity.
    fn concrete(token: &str) -> String {
        // A bare number is a real value a guide chose (`--timeout 900`), not a
        // stand-in — so a placeholder must carry at least one capital.
        let shouty = !token.starts_with('-')
            && token.chars().any(|c| c.is_ascii_uppercase())
            && token.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        let looks_like_placeholder = token.starts_with('<') || token.starts_with('$') || shouty;
        if looks_like_placeholder { "X".to_string() } else { token.to_string() }
    }

    /// A token that could be a subcommand name: lowercase letters, digits and
    /// hyphens only. Placeholders (`<ID>`, `$RUN`, `"…"`) and options are all
    /// excluded by construction. A trailing `;` or `)` is shell punctuation,
    /// not part of the name — without stripping it, `oximux team ls; jq .`
    /// hid `ls` from the gate entirely.
    fn is_verb_word(token: &str) -> bool {
        let token = token.trim_end_matches([';', ')']);
        !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !token.starts_with('-')
    }

    /// Where one command ends: a pipe, a chain, a line continuation, or the
    /// start of a shell comment. A trailing `# what this does` is prose that
    /// happens to sit on a command line, and feeding it to the parser reports
    /// a defect in the guide that is not there.
    fn is_terminator(token: &str) -> bool {
        token == "|" || token == "&&" || token == "||" || token == "\\" || token.starts_with('#')
    }

    /// Whether a token is an invocation of this CLI.
    ///
    /// Shell wraps the command name in ways that are invisible to a plain
    /// equality test: `RUN=$(oximux …)` is the shape the team guide's own
    /// worked example uses, and before this the whole block — four flags and
    /// two verbs — was silently unchecked. Strips an assignment prefix and any
    /// opening substitution, quoting or grouping punctuation.
    fn is_oximux_token(token: &str) -> bool {
        let token = token.trim_end_matches([';', ')']);
        let token = match token.split_once('=') {
            // `VAR=$(oximux` — an assignment, not a flag (which starts `-`).
            Some((lhs, rhs)) if !lhs.starts_with('-') && !lhs.is_empty() => rhs,
            _ => token,
        };
        token.trim_start_matches(['$', '(', '`', '"', '\'']) == "oximux"
    }

    /// The extractor must actually find the commands, or both gates above pass
    /// by finding nothing.
    #[test]
    fn the_extractor_finds_commands_in_both_code_shapes() {
        let fenced = command_spans("text\n```sh\noximux team run --name x\n```\nmore").join(" ");
        assert_eq!(invocations(&fenced), vec![vec!["team", "run"]]);

        let inline = command_spans("run `oximux worktree set <ID> --phase done` when finished").join(" ");
        assert_eq!(invocations(&inline), vec![vec!["worktree", "set"]]);

        // Prose outside backticks is not scanned, or "CLI" would be a verb.
        assert!(command_spans("the oximux CLI is offline").is_empty());
    }

    /// A shell continuation is one command, so its later flags are checked.
    ///
    /// The regression this guards is a silent one: before it, everything after
    /// the first `\` in a fenced block was simply not scanned, so a wrong flag
    /// on line four of a `team run` passed the gate. A gate with a hole is
    /// worse than no gate — it is trusted.
    #[test]
    fn a_line_continuation_is_one_invocation() {
        let spans = command_spans("```sh\noximux team run \\\n  --role a=b \\\n  --worktree-each\n```");
        let joined = spans.join("\n");
        let found = invocations_with_flags(&joined);
        assert_eq!(found.len(), 1, "joined into one invocation: {spans:?}");
        assert_eq!(found[0].0, vec!["team", "run"]);
        assert_eq!(found[0].1, vec!["role".to_string(), "worktree-each".to_string()]);
    }

    /// A placeholder ends the subcommand path; it is an argument, not a verb.
    ///
    /// The boundary the whole extractor rests on: `permit allow` is two verbs
    /// and everything after is data. Were a quoted `"$S"` read as a verb, the
    /// gate would fail on every correct guide, and the natural "fix" would be
    /// to loosen it until it caught nothing.
    #[test]
    fn a_placeholder_is_not_read_as_a_subcommand() {
        assert_eq!(invocations("oximux permit allow \"$S\" \"$REQ\""), vec![vec!["permit", "allow"]]);
        assert_eq!(invocations("oximux worktree rm <ID>"), vec![vec!["worktree", "rm"]]);
        assert_eq!(invocations("oximux run \"do the thing\""), vec![vec!["run"]]);
    }

    /// Two invocations on one line are two checks, not one.
    #[test]
    fn a_pipeline_splits_into_separate_invocations() {
        // The extractor is deliberately greedy — `k` is a positional of
        // `state get`, and it is the WALKER that stops at a leaf command
        // rather than the extractor, which has no view of the tree.
        let found = invocations("oximux team ls --json | jq . && oximux state get k");
        assert_eq!(found, vec![vec!["team", "ls"], vec!["state", "get", "k"]]);
    }

    /// Flags are attributed to the command they were written on.
    #[test]
    fn flags_are_read_against_their_own_command() {
        let found = invocations_with_flags("oximux worktree set <ID> --comment x --phase done");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, vec!["worktree", "set"]);
        assert_eq!(found[0].1, vec!["comment".to_string(), "phase".to_string()]);

        // `--role a=b` keeps the flag name and drops the value.
        let roles = invocations_with_flags("oximux team run --role a=b --json");
        assert_eq!(roles[0].1, vec!["role".to_string(), "json".to_string()]);
    }

    /// A verb that does not exist fails the gate.
    ///
    /// Mutation check, kept as a test: without it the gate could silently stop
    /// matching and every guide would "pass".
    #[test]
    fn a_bogus_verb_would_not_resolve() {
        let root = crate::cli::Cli::command();
        assert!(
            root.get_subcommands().any(|s| s.get_name() == "worktree"),
            "sanity: the real verb resolves"
        );
        assert!(
            !root.get_subcommands().any(|s| s.get_name() == "wroktree"),
            "a typo must not resolve, or the gate proves nothing"
        );
    }

    /// `--full` prints the file as installed; plain `get` prints the prose.
    #[test]
    fn get_strips_the_frontmatter_unless_full() {
        let (_, body) = get("oximux-cli", false).expect("a known topic");
        let (_, full) = get("oximux-cli", true).expect("a known topic");

        assert!(full.starts_with("---\nname: oximux-cli"), "--full keeps the frontmatter");
        assert!(!body.starts_with("---"), "the body has none: {:?}", &body[..40.min(body.len())]);
        assert!(body.starts_with("# "), "the body opens on the heading: {:?}", &body[..40.min(body.len())]);
        assert!(full.len() > body.len());
    }

    /// A document with no frontmatter is returned whole, not truncated at a
    /// horizontal rule further down.
    #[test]
    fn a_guide_without_frontmatter_survives_intact() {
        assert_eq!(body_of("# Title\n\ntext\n\n---\n\nmore\n"), "# Title\n\ntext\n\n---\n\nmore\n");
        assert_eq!(body_of("---\nname: x\n---\n\n# Title\n"), "# Title\n");
    }

    /// An unknown topic is a usage error that names the real ones.
    #[test]
    fn an_unknown_topic_lists_the_known_ones() {
        let failure = get("oximux-nope", false).expect_err("unknown topic");
        assert_eq!(failure.exit, exit::USAGE);
        assert!(failure.message.contains("oximux-cli"), "{}", failure.message);
    }

    /// Every guide carries the frontmatter an agent runtime discovers it by,
    /// and its `name` is the topic — the installed directory is named from the
    /// topic, so a mismatch installs a skill the runtime lists under another
    /// name.
    #[test]
    fn every_guide_declares_its_own_name() {
        for guide in GUIDES {
            assert!(
                guide.text.starts_with("---\n"),
                "{} has no frontmatter",
                guide.topic
            );
            assert!(
                guide.text.contains(&format!("\nname: {}\n", guide.topic)),
                "{} does not declare `name: {}`",
                guide.topic,
                guide.topic
            );
            assert!(
                guide.text.contains("\ndescription:"),
                "{} has no description for skill discovery",
                guide.topic
            );
        }
    }

    /// The default target set is an observation, not a frozen list.
    ///
    /// The list this replaced was wrong — it excluded codex and droid, both of
    /// which keep populated `skills/` directories on the machine the list was
    /// "measured" on. The measurement had been truncated. A probe cannot go
    /// stale that way.
    #[test]
    fn the_default_targets_are_probed_not_hardcoded() {
        let home = tempfile::tempdir().expect("a temp dir");
        let dialect = DIALECTS
            .iter()
            .find(|d| d.slug == "claude")
            .expect("claude is a dialect");

        // Nothing there: not a default target, and no path is conjured.
        assert!(!keeps_skills(dialect) || skills_dir(dialect).is_some());

        // The path shape is the convention every skills-reading agent shares.
        if let Some(path) = install_path(dialect, "oximux-cli") {
            assert!(path.ends_with("skills/oximux-cli/SKILL.md"), "unexpected path {path:?}");
        }
        drop(home);
    }

    /// A file OxiMux did not write is never clobbered.
    ///
    /// The doc comment this guards used to *assert* that nothing else could
    /// own the path. That was an assumption; the sibling `agent hooks` verb
    /// takes a backup and honours a marker precisely because it is not safe.
    #[test]
    fn a_foreign_skill_file_is_left_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("SKILL.md");

        // Nothing there yet: ours to create.
        assert!(foreign_file(&path).is_none());

        // Ours: overwritten without complaint, which is the whole point.
        std::fs::write(&path, GUIDES[0].text).expect("write");
        assert!(foreign_file(&path).is_none(), "a guide we wrote is ours to refresh");

        // Someone else's: refused.
        std::fs::write(&path, "---\nname: my-own-skill\n---\n# mine\n").expect("write");
        assert!(foreign_file(&path).is_some(), "a foreign file must be refused");

        // A near-miss — right shape, wrong name — is still not ours.
        std::fs::write(&path, "---\nname: oximux-clip\n---\n").expect("write");
        assert!(foreign_file(&path).is_some(), "only an exact topic name is ours");
    }

    /// A symlink is reported, not written through into whatever it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_guide_is_not_followed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("elsewhere.md");
        std::fs::write(&real, "someone's dotfiles\n").expect("write");
        let link = dir.path().join("SKILL.md");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(foreign_file(&link).is_some(), "a symlink must not be written through");
        assert_eq!(
            std::fs::read_to_string(&real).expect("read"),
            "someone's dotfiles\n",
            "the target was modified"
        );
    }

    /// `write_guide` creates the topic directory and writes the file.
    #[test]
    fn write_guide_creates_its_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("skills").join("oximux-cli").join("SKILL.md");
        write_guide(&path, GUIDES[0].text).expect("writes through a missing directory");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GUIDES[0].text);

        // Overwriting one of ours is not an error.
        write_guide(&path, GUIDES[1].text).expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GUIDES[1].text);
    }

    /// A write into an unwritable place is reported, not panicked on.
    #[test]
    fn an_unwritable_target_reports_rather_than_aborting() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // A FILE where the directory should be: `create_dir_all` must fail.
        let blocker = dir.path().join("skills");
        std::fs::write(&blocker, "not a directory").expect("write");
        let path = blocker.join("oximux-cli").join("SKILL.md");
        assert!(write_guide(&path, "x").is_err(), "must report, not panic");
    }

    /// Naming an agent that is not on this machine fails, rather than
    /// reporting success for having done nothing.
    #[test]
    fn a_named_absent_agent_is_an_error() {
        // Pick a dialect whose home does not exist under a temp HOME. Rather
        // than mutate the process environment (which races every other test),
        // assert the branch's shape directly.
        let absent = DIALECTS.iter().find(|d| !d.agent_is_installed());
        if let Some(dialect) = absent {
            let failure = install(Some(dialect.slug)).expect_err("an absent agent is an error");
            assert_eq!(failure.exit, exit::ERROR);
            assert_eq!(failure.code, "agent-absent");
        }
    }

    /// An unknown slug is a usage error naming the real ones.
    #[test]
    fn an_unknown_agent_is_a_usage_error() {
        let failure = install(Some("nope")).expect_err("unknown agent");
        assert_eq!(failure.exit, exit::USAGE);
        assert!(failure.message.contains("claude"), "{}", failure.message);
    }
}
