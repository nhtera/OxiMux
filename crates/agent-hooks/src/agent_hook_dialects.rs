//! Every agent CLI that reports status through a hooks file, in one table.
//!
//! Detecting an agent from the process tree tells the rail *that* it is there,
//! and its window title says roughly what it is doing. Neither can say what the
//! agent SAID — so every row but Claude's showed a bare status verb where a
//! Claude row showed the actual reply. The difference was never the rail; it
//! was that only Claude had hooks installed.
//!
//! The agents differ far less than they look. Every one of them runs a command
//! at fixed lifecycle points and hands it the event as JSON **on stdin**, so
//! the existing `oximux agent-status` CLI is the hook for all of them
//! unchanged. What differs is only: which file the hooks live in, how one entry
//! is spelled in it, what the events are called, and which key carries the
//! reply. All four are data, which is what [`DIALECTS`] is — the installer in
//! [`crate::agent_hooks_global`] and the payload readers below both read from
//! it, so adding an agent is a row rather than a module.
//!
//! Two deliberate non-goals, both learned from Codex:
//!
//! * **A per-turn `notify` program is not used** where an agent offers one.
//!   Those settings hold a single program, not a list, so writing one would
//!   silently replace whatever the user already has there. A hooks file has a
//!   per-event array, so ours can be merged alongside anything else.
//! * **Hook trust is left to the agent.** An agent that holds a trusted-hash
//!   ledger and asks the user to approve a new hook in its own UI gets to keep
//!   doing that. Forging an entry there would be a consent bypass. Until the
//!   user says yes, these hooks simply never fire and the rail falls back to
//!   process + title exactly as before.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// One lifecycle event wired to one reported state.
pub struct EventSpec {
    /// Event name as the agent's hooks file spells it.
    pub event: &'static str,
    /// Tool-name filter, where the agent's file takes one. `None` for events
    /// that are not about a tool.
    pub matcher: Option<&'static str>,
    /// Reported state: `working`, `needs_approval` or `idle`.
    pub state: &'static str,
    /// Extra CLI flags appended to this event's command. Empty for all but the
    /// events that need the payload filtered before they report anything.
    pub flags: &'static str,
}

/// A bound on how long the agent waits for our hook, in whatever unit and
/// under whatever key that agent's file spells it.
///
/// Load-bearing wherever the agent runs its hooks synchronously, which most
/// do: ours then sits in front of the user's turn, and this bounds the worst
/// case — an unreachable relay, a socket that never answers — to a pause
/// rather than a stall. Generous relative to the real cost (a local socket
/// round-trip) so a loaded machine does not lose a status update to a missed
/// deadline.
pub struct Timeout {
    pub key: &'static str,
    pub value: u64,
}

/// How one hook entry is spelled in a given agent's file.
pub enum EntryShape {
    /// `{matcher?, hooks:[{type:"command", command, …}]}` — an entry is a
    /// *group* of commands. The shape Claude established and most agents copied.
    Nested {
        /// Whether the command entry may carry `"async": true`. Only meaningful
        /// where the agent supports it; an async hook cannot hold anything up,
        /// which is why the agent that offers it needs no timeout.
        async_command: bool,
        timeout: Option<Timeout>,
    },
    /// The entry *is* the command: `{command, timeout?}`. No group, no array.
    Flat {
        /// Key holding the shell command — not always `command`.
        command_key: &'static str,
        /// Whether the entry also carries `"type": "command"`.
        typed: bool,
        timeout: Option<Timeout>,
    },
}

/// How an agent is taught to report, and what OxiMux writes to teach it.
///
/// Both variants are one file in the agent's own config directory. They differ
/// in what is IN it: a list of commands the agent shells out to, or a program
/// the agent loads and runs itself. An agent with no hooks file is not a weaker
/// case of one that has them — there is nothing to merge into, no entries to
/// mark, and nothing of the user's to preserve.
pub enum Install {
    /// A JSON hooks file — `{hooks:{Event:[entry, …]}}` — that the user may
    /// also write, so ours are merged in and marked rather than replacing it.
    HooksFile {
        /// Bookkeeping key stamped on our entries, or `None` where the file
        /// rejects unknown fields (or where we own the whole file and need no
        /// marker to find our own entries again).
        marker: Option<&'static str>,
        /// True when nothing but OxiMux ever writes this file, so removing our
        /// hooks means deleting it rather than pruning entries out of it.
        owns_file: bool,
        /// Root-level schema version the file must declare, where it declares
        /// one.
        root_version: Option<u64>,
        entry: EntryShape,
        events: &'static [EventSpec],
    },
    /// A source file the agent discovers and runs in-process. The agent's own
    /// event API is the hook, so there is nothing to merge: the file is ours
    /// alone, written whole and deleted whole.
    Extension {
        /// Renders the source, given the absolute path of the `oximux` binary
        /// the extension calls back into.
        source: fn(&Path) -> String,
    },
}

/// One agent's status reporting, end to end.
pub struct HookDialect {
    /// `--format` value, and the log/agent identifier. Also the marker-free
    /// dialects' identity in a command string.
    pub slug: &'static str,
    /// Display name, for logs.
    pub agent: &'static str,
    /// The agent's own configuration directory, resolved at call time so an
    /// agent that relocates it via env is honoured.
    ///
    /// Separate from [`Self::file`] because its EXISTENCE is the test for
    /// whether the agent is installed at all. OxiMux adds to an agent's home;
    /// it never conjures one, so a user who has never run a given agent never
    /// finds its dotfile appear.
    pub home: fn() -> Option<PathBuf>,
    /// Path of the hooks or extension file, relative to [`Self::home`].
    pub file: &'static str,
    pub install: Install,
    /// Keys carrying the agent's reply on its turn-end payload, most specific
    /// first. Empty where the agent hands over only a transcript path.
    pub message_keys: &'static [&'static str],
    /// Whether a turn-end payload with no reply in it should be chased into the
    /// transcript the payload points at.
    pub reads_transcript: bool,
}

impl HookDialect {
    /// Absolute path of the file OxiMux writes, or `None` with no home
    /// directory to resolve it against.
    pub fn path(&self) -> Option<PathBuf> {
        Some((self.home)()?.join(self.file))
    }

    /// True when this agent is installed — that is, when it has already made
    /// its own configuration directory.
    ///
    /// A hooks file written into a home that does not exist is not merely
    /// litter: the agent is not there to read it, so the write buys nothing
    /// and leaves a dotfile the user never asked for.
    pub fn agent_is_installed(&self) -> bool {
        (self.home)().is_some_and(|home| home.is_dir())
    }

    /// The events this dialect wires, or empty for one whose agent dispatches
    /// its own.
    pub fn events(&self) -> &'static [EventSpec] {
        match &self.install {
            Install::HooksFile { events, .. } => events,
            Install::Extension { .. } => &[],
        }
    }
}

/// Marks an entry as OxiMux-owned so re-install/remove touches only our hooks.
pub(crate) const MANAGED_MARKER: &str = "_oximux_managed";

/// Every agent whose status hooks OxiMux installs.
///
/// Ordered as they were measured. Claude is first because its row is the one
/// every other row is trying to look like.
pub const DIALECTS: &[HookDialect] =
    &[CLAUDE, CODEX, DROID, COPILOT, GEMINI, CURSOR, GROK, PI, OMP];

/// Claude Code — `~/.claude/settings.json`.
///
/// The only dialect whose commands carry no `--format`: they are byte-shared
/// with the per-spawn `--settings` injection, and Claude's command-string hook
/// dedup only collapses the two if they match exactly.
///
/// `Notification` (not `PermissionRequest`, a dead name in current Claude) is
/// what fires for a tool-permission prompt, but it also fires for a benign
/// "waiting for your input" nudge — hence the filter flag, without which the
/// dot would go amber for both.
const CLAUDE: HookDialect = HookDialect {
    slug: "claude",
    agent: "Claude Code",
    home: claude_home,
    file: "settings.json",
    install: Install::HooksFile {
        marker: Some(MANAGED_MARKER),
        owns_file: false,
        root_version: None,
        entry: EntryShape::Nested {
            async_command: true,
            timeout: None,
        },
        events: &[
            EventSpec { event: "PreToolUse", matcher: Some("*"), state: "working", flags: "" },
            // Fires the instant the user submits — whether typed into the agent's
            // own TUI or sent from OxiMux — carrying the prompt that becomes the
            // row's title. Without it a text-only reply that calls no tool would
            // look idle for its whole turn.
            EventSpec { event: "UserPromptSubmit", matcher: None, state: "working", flags: "" },
            EventSpec {
                event: "Notification",
                matcher: None,
                state: "needs_approval",
                flags: "--filter-notification",
            },
            EventSpec { event: "Stop", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &["last_assistant_message"],
    reads_transcript: true,
};

/// Codex — `$CODEX_HOME/hooks.json`.
///
/// Rejects a hooks file carrying fields it does not define, so nothing may be
/// stamped on its entries at all; ours are recognised by their command
/// instead. `PermissionRequest` is its real approval event and needs no filter:
/// unlike Claude's, it fires only for an actual approval ask.
const CODEX: HookDialect = HookDialect {
    slug: "codex",
    agent: "Codex",
    home: codex_home,
    file: "hooks.json",
    install: Install::HooksFile {
        marker: None,
        owns_file: false,
        root_version: None,
        entry: EntryShape::Nested {
            async_command: false,
            timeout: Some(Timeout { key: "timeout", value: 5 }),
        },
        events: &[
            EventSpec { event: "UserPromptSubmit", matcher: None, state: "working", flags: "" },
            EventSpec { event: "PreToolUse", matcher: Some("*"), state: "working", flags: "" },
            EventSpec { event: "PermissionRequest", matcher: None, state: "needs_approval", flags: "" },
            // The one that closes the gap: Codex puts the turn's reply here.
            EventSpec { event: "Stop", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &["last_assistant_message"],
    reads_transcript: false,
};

/// Droid — `~/.factory/settings.json`.
///
/// Claude's file shape and Claude's event names, measured: a `SessionStart`
/// payload carries `hook_event_name`, `session_id`, `cwd` and `transcript_path`
/// under exactly those spellings, and Droid's own hook log reports our entries
/// matched, executed and exited 0 with empty output accepted.
///
/// Its turn-end payload carries no reply — only a transcript path — which is
/// why [`HookDialect::reads_transcript`] is load-bearing here and not merely a
/// fallback.
///
/// No approval event is wired. Droid reports one through `Notification`, which
/// — like Claude's — also carries benign nudges, and the text that separates
/// them has not been measured here. A missing amber dot costs a row one state;
/// an amber dot that lights for every notification is a bug in every row.
const DROID: HookDialect = HookDialect {
    slug: "droid",
    agent: "Droid",
    home: droid_home,
    file: "settings.json",
    install: Install::HooksFile {
        marker: Some(MANAGED_MARKER),
        owns_file: false,
        root_version: None,
        entry: EntryShape::Nested {
            async_command: false,
            timeout: Some(Timeout { key: "timeout", value: 10 }),
        },
        events: &[
            EventSpec { event: "UserPromptSubmit", matcher: None, state: "working", flags: "" },
            EventSpec { event: "PreToolUse", matcher: Some("*"), state: "working", flags: "" },
            EventSpec { event: "Stop", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &["last_assistant_message"],
    reads_transcript: true,
};

/// GitHub Copilot — `~/.copilot/hooks/oximux.json`.
///
/// Reads a *directory* of hook files, so ours is a file of our own and nothing
/// of the user's is ever merged, marked or pruned. Its entries are flat — the
/// entry is the command — and the command lives under `bash`, not `command`.
///
/// Measured end to end: all six events fire, the payloads use Claude's key
/// names (`prompt`, `tool_name`, `tool_input`), and `Stop` carries a
/// `transcript_path` but no reply, so the reply comes from the transcript.
const COPILOT: HookDialect = HookDialect {
    slug: "copilot",
    agent: "GitHub Copilot",
    home: copilot_home,
    file: "hooks/oximux.json",
    install: Install::HooksFile {
        marker: None,
        owns_file: true,
        root_version: Some(1),
        entry: EntryShape::Flat {
            command_key: "bash",
            typed: true,
            timeout: Some(Timeout { key: "timeoutSec", value: 5 }),
        },
        events: &[
            EventSpec { event: "UserPromptSubmit", matcher: None, state: "working", flags: "" },
            EventSpec { event: "PreToolUse", matcher: None, state: "working", flags: "" },
            EventSpec { event: "Stop", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &[],
    reads_transcript: true,
};

/// Gemini CLI — `~/.gemini/settings.json`.
///
/// Claude's file shape, its own event names, and a timeout in **milliseconds**
/// where every other dialect counts seconds — the one unit difference in the
/// table, and the reason [`Timeout`] carries its value rather than deriving it.
///
/// No approval event exists: Gemini's approvals are inline UI, so a Gemini row
/// never reports needs_approval. That is upstream, not a gap here.
const GEMINI: HookDialect = HookDialect {
    slug: "gemini",
    agent: "Gemini CLI",
    home: gemini_home,
    file: "settings.json",
    install: Install::HooksFile {
        marker: Some(MANAGED_MARKER),
        owns_file: false,
        root_version: None,
        entry: EntryShape::Nested {
            async_command: false,
            timeout: Some(Timeout { key: "timeout", value: 10_000 }),
        },
        events: &[
            EventSpec { event: "BeforeAgent", matcher: None, state: "working", flags: "" },
            EventSpec { event: "BeforeTool", matcher: None, state: "working", flags: "" },
            EventSpec { event: "AfterAgent", matcher: None, state: "idle", flags: "" },
        ],
    },
    // Gemini spells the turn's reply for itself; nothing else in its payload
    // carries assistant text.
    message_keys: &["prompt_response"],
    reads_transcript: false,
};

/// Cursor — `~/.cursor/hooks.json`.
///
/// Flat entries and camelCase events — the only dialect that is neither
/// Claude's shape nor Claude's vocabulary.
///
/// `afterAgentResponse` is wired as well as `stop` because it is the one that
/// carries the reply; both report idle, so whichever lands last leaves the row
/// idle either way.
const CURSOR: HookDialect = HookDialect {
    slug: "cursor",
    agent: "Cursor",
    home: cursor_home,
    file: "hooks.json",
    install: Install::HooksFile {
        marker: None,
        owns_file: false,
        root_version: None,
        entry: EntryShape::Flat {
            command_key: "command",
            typed: false,
            timeout: Some(Timeout { key: "timeout", value: 10 }),
        },
        events: &[
            EventSpec { event: "beforeSubmitPrompt", matcher: None, state: "working", flags: "" },
            EventSpec { event: "preToolUse", matcher: None, state: "working", flags: "" },
            EventSpec { event: "afterAgentResponse", matcher: None, state: "idle", flags: "" },
            EventSpec { event: "stop", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &["text"],
    reads_transcript: false,
};

/// Grok — `~/.grok/hooks/oximux.json`.
///
/// Another directory of hook files, so again ours is our own.
///
/// Its tool matcher is a real regular expression, so the bare `*` every other
/// dialect uses is not a match-all here and would silently match nothing.
/// `StopFailure` is wired alongside `Stop` because a turn that ends on an API
/// error fires only the former, and without it a row sticks on working.
const GROK: HookDialect = HookDialect {
    slug: "grok",
    agent: "Grok",
    home: grok_home,
    file: "hooks/oximux.json",
    install: Install::HooksFile {
        marker: None,
        owns_file: true,
        root_version: None,
        entry: EntryShape::Nested {
            async_command: false,
            timeout: Some(Timeout { key: "timeout", value: 10 }),
        },
        events: &[
            EventSpec { event: "UserPromptSubmit", matcher: None, state: "working", flags: "" },
            EventSpec { event: "PreToolUse", matcher: Some(".*"), state: "working", flags: "" },
            EventSpec { event: "Stop", matcher: None, state: "idle", flags: "" },
            EventSpec { event: "StopFailure", matcher: None, state: "idle", flags: "" },
        ],
    },
    message_keys: &["lastAssistantMessage", "last_assistant_message"],
    reads_transcript: true,
};

/// Pi — `~/.pi/agent/extensions/oximux-agent-status.ts`.
///
/// The one agent here with no hooks file. Its extension point is an in-process
/// TypeScript API, so instead of a list of commands OxiMux writes a small
/// program that subscribes to Pi's own events and shells out to the same CLI
/// every other dialect does. See [`crate::pi_status_extension`] for the source
/// and for the measured event sequence it depends on.
///
/// The extension composes its payload in Claude's key names, so nothing here
/// is Pi-shaped: the reply arrives under `last_assistant_message` like Codex's,
/// already flattened out of Pi's array of message parts.
const PI: HookDialect = HookDialect {
    slug: "pi",
    agent: "Pi",
    home: pi_home,
    file: crate::pi_status_extension::EXTENSION_FILE,
    install: Install::Extension {
        source: crate::pi_status_extension::source,
    },
    message_keys: &["last_assistant_message"],
    reads_transcript: false,
};

/// omp — `~/.omp/agent/extensions/oximux-agent-status-omp.ts`.
///
/// A Pi fork that kept Pi's extension API and event dialect — re-measured
/// live against omp 18.0.4: same event sequence, same double-fire pairs, the
/// reply under `message.content[].text` in the raw events. Like Pi's, the
/// extension composes its payload in Claude's key names, the reply already
/// flattened out of that array of parts. The extension is the shared Pi
/// template rendered with omp's identity, into a file whose NAME differs from
/// Pi's because omp also kept Pi's `PI_CODING_AGENT_DIR` override: with that
/// set, both agents read the same directory, and two dialects writing one
/// path would fight over it (see [`crate::pi_status_extension`]).
const OMP: HookDialect = HookDialect {
    slug: "omp",
    agent: "omp",
    home: omp_home,
    file: crate::pi_status_extension::OMP_EXTENSION_FILE,
    install: Install::Extension {
        source: crate::pi_status_extension::omp_source,
    },
    message_keys: &["last_assistant_message"],
    reads_transcript: false,
};

/// The dialect named by a `--format` slug, or `None` for an unknown one.
pub fn dialect_for_slug(slug: &str) -> Option<&'static HookDialect> {
    DIALECTS.iter().find(|d| d.slug == slug)
}

/// Every `--format` value the CLI accepts, for the usage message.
pub fn known_slugs() -> String {
    DIALECTS
        .iter()
        .map(|d| d.slug)
        .collect::<Vec<_>>()
        .join("|")
}

/// The hook commands for one dialect, one per wired event.
///
/// The single source of truth for both the global install and — for Claude —
/// the per-spawn `--settings` JSON, so the command strings are byte-identical
/// and Claude's dedup makes an agent that sees both fire each hook once.
pub(crate) fn hook_specs(dialect: &HookDialect, binary_path: &Path) -> Vec<crate::agent_status_hooks::HookSpec> {
    // Single-quote the binary path (an installed bundle path can contain
    // spaces) and escape any embedded quote (`'` → `'\''`) so a home dir like
    // `/Users/O'X` cannot break out of the quoting into shell injection.
    let quoted = binary_path.display().to_string().replace('\'', "'\\''");
    dialect
        .events()
        .iter()
        .map(|spec| {
            let mut command = format!("'{quoted}' agent-status --state {}", spec.state);
            if !spec.flags.is_empty() {
                command.push(' ');
                command.push_str(spec.flags);
            }
            // Claude's commands carry no `--format`: it is the default, and
            // spelling it would break the byte-equality the `--settings` path
            // depends on.
            if dialect.slug != CLAUDE.slug {
                command.push_str(" --format ");
                command.push_str(dialect.slug);
            }
            crate::agent_status_hooks::HookSpec {
                event: spec.event,
                matcher: spec.matcher,
                command,
            }
        })
        .collect()
}

/// The agent's last reply from a turn-end payload, by whichever route this
/// dialect has one.
///
/// Two routes, tried in order, because the agents split evenly between them:
/// the reply handed over directly (preferred — race-free, and the payload is
/// the agent's own account of what it said), or the transcript the payload
/// points at. A dialect with neither returns `None` and the row keeps its
/// previous reading rather than being blanked.
pub fn last_message(dialect: &HookDialect, stdin_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(stdin_json).ok()?;
    if let Some(msg) = read_str(&value, dialect.message_keys) {
        return crate::agent_status_hooks::normalize_message(&msg);
    }
    if !dialect.reads_transcript {
        return None;
    }
    let path = read_str(&value, &["transcript_path", "transcriptPath"])?;
    crate::agent_status_hooks::last_assistant_from_transcript(Path::new(&path))
}

/// The tool a `PreToolUse`-shaped payload is about.
pub fn tool_name(stdin_json: &str) -> Option<String> {
    read_str(&parse(stdin_json)?, &["tool_name", "toolName", "name"])
}

/// The user's prompt from a turn-start payload.
///
/// Several spellings are accepted because this key is the least stable part of
/// the payload across agent CLIs, and a missed prompt costs the row its title
/// while a wrong one is impossible — none of these names carries anything else.
pub fn prompt(stdin_json: &str) -> Option<String> {
    read_str(
        &parse(stdin_json)?,
        &[
            "prompt",
            "user_prompt",
            "userPrompt",
            "initial_prompt",
            "initialPrompt",
            "user_message",
            "message",
        ],
    )
}

fn parse(stdin_json: &str) -> Option<Value> {
    serde_json::from_str(stdin_json).ok()
}

/// First key present as a non-empty string, trimmed.
fn read_str(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| value.get(*k)?.as_str())
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}

fn claude_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude"))
}

/// Honours `CODEX_HOME` so a user who relocated it still gets hooks installed
/// in the home Codex actually reads.
fn codex_home() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => Some(dirs::home_dir()?.join(".codex")),
    }
}

fn droid_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".factory"))
}

fn gemini_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".gemini"))
}

fn cursor_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".cursor"))
}

fn copilot_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".copilot"))
}

fn grok_home() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".grok"))
}

/// Honours `PI_CODING_AGENT_DIR`, which relocates Pi's whole agent directory:
/// installing into the default while Pi reads another is a file written to a
/// directory nothing ever loads.
fn pi_home() -> Option<PathBuf> {
    pi_family_home(".pi")
}

/// omp inherited Pi's env override VERBATIM (`PI_CODING_AGENT_DIR`, verified
/// in omp 18.0.4), so with it set omp and Pi genuinely read the same agent
/// directory — the homes are allowed to collide because the two dialects
/// write differently-NAMED files (see
/// [`crate::pi_status_extension::OMP_EXTENSION_FILE`]).
fn omp_home() -> Option<PathBuf> {
    pi_family_home(".omp")
}

/// Resolve a Pi-family agent home, believing the SHARED env override only on
/// the agent's own evidence.
///
/// The override cannot say WHICH of the two agents is installed — both read
/// the same variable, so on a machine with only one of them, trusting it
/// alone would have the other dialect report itself installed and write its
/// extension into a directory its agent never created (and whose resident
/// agent loads everything under `extensions/`, spawning a second reporter per
/// event). Each agent still writes logs and runtime records under its own
/// config root (`~/.pi` / `~/.omp`) even when the agent dir is relocated —
/// measured on omp 18.0.4 — so the root's existence is the footprint that
/// tells the two apart.
fn pi_family_home(root_name: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    match std::env::var_os("PI_CODING_AGENT_DIR") {
        Some(dir) if !dir.is_empty() => {
            home.join(root_name).is_dir().then(|| PathBuf::from(dir))
        }
        _ => Some(home.join(root_name).join("agent")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> &'static Path {
        Path::new("/Applications/OxiMux.app/Contents/MacOS/oximux")
    }

    fn dialect(slug: &str) -> &'static HookDialect {
        dialect_for_slug(slug).expect("a known slug")
    }

    #[test]
    fn every_dialect_reports_both_ends_of_a_turn() {
        // A dialect that never reports idle can never take a row OUT of
        // working — the exact defect that made every non-Claude row read
        // wrong — and one that never reports working never puts it in.
        for d in DIALECTS {
            for state in ["working", "idle"] {
                let reports = match &d.install {
                    Install::HooksFile { events, .. } => events.iter().any(|e| e.state == state),
                    // An extension dispatches its own events, so the claim has
                    // to be checked against the source we are about to write.
                    Install::Extension { source } => {
                        source(Path::new("/x/oximux")).contains(&format!(r#"report("{state}""#))
                    }
                };
                assert!(reports, "{} never reports {state}", d.slug);
            }
        }
    }

    #[test]
    fn every_dialect_can_produce_a_reply() {
        // A dialect that reads neither a direct key nor a transcript would
        // install hooks that report a state and never a message — working
        // hooks, and the reported bug still open.
        for d in DIALECTS {
            assert!(
                !d.message_keys.is_empty() || d.reads_transcript,
                "{} has no route to the agent's reply",
                d.slug
            );
        }
    }

    #[test]
    fn no_two_dialects_write_the_same_file() {
        // Two rows resolving to one path would have each install stomp the
        // other's on every sync, and the row for whichever lost would go
        // quiet — with the file on disk looking perfectly correct.
        let mut paths: Vec<_> = DIALECTS.iter().filter_map(|d| d.path()).collect();
        let before = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(before, paths.len(), "two dialects claim one file");
    }

    #[test]
    fn omp_and_pi_never_write_the_same_file_even_when_their_homes_collide() {
        // omp kept Pi's `PI_CODING_AGENT_DIR` override verbatim, so with that
        // variable set both homes resolve to ONE directory —
        // `no_two_dialects_write_the_same_file` runs with the variable unset
        // and can never catch that collision. The invariant that protects it
        // is the RELATIVE file name: distinct names keep each dialect's
        // install and prune its own whichever directory they land in.
        assert_ne!(dialect("pi").file, dialect("omp").file);
        // Both live under extensions/, where the runtime discovers every file,
        // so distinct names still mean both get loaded — that is the deal.
        assert!(dialect("omp").file.starts_with("extensions/"));
    }

    #[test]
    fn the_omp_reply_arrives_under_the_shared_key() {
        // The omp extension composes the same Claude-shaped payload the Pi one
        // does; a drifted key spelling here would install working hooks that
        // never surface a reply (the bug the ~788 Pi assert exists to stop).
        assert_eq!(
            last_message(dialect("omp"), r#"{"last_assistant_message":"DONE"}"#).as_deref(),
            Some("DONE")
        );
        assert!(matches!(dialect("omp").install, Install::Extension { .. }));
    }

    #[test]
    fn an_absent_agent_is_never_given_a_home() {
        // The gate that keeps OxiMux from creating ~/.cursor, ~/.grok and the
        // rest for a user who has installed none of them.
        for d in DIALECTS {
            let home = (d.home)().expect("a home dir");
            assert_eq!(
                d.agent_is_installed(),
                home.is_dir(),
                "{} disagrees with its own home {home:?}",
                d.slug
            );
        }
    }

    #[test]
    fn slugs_are_unique_and_resolvable() {
        for d in DIALECTS {
            assert_eq!(dialect(d.slug).slug, d.slug);
        }
        let mut slugs: Vec<_> = DIALECTS.iter().map(|d| d.slug).collect();
        slugs.sort_unstable();
        let before = slugs.len();
        slugs.dedup();
        assert_eq!(before, slugs.len(), "two dialects share a slug");
        assert!(dialect_for_slug("nope").is_none());
    }

    #[test]
    fn a_file_we_own_carries_no_marker() {
        // A marker exists to find our entries among the user's. Where the file
        // is ours alone there are no others, and stamping one would only risk
        // a schema the agent rejects.
        for d in DIALECTS {
            let Install::HooksFile { owns_file: true, marker, .. } = &d.install else {
                continue;
            };
            assert!(marker.is_none(), "{} owns its file yet stamps a marker", d.slug);
        }
    }

    #[test]
    fn claude_commands_carry_no_format_flag() {
        // Byte-equality with the per-spawn `--settings` injection is what makes
        // Claude's hook dedup collapse the pair. A `--format claude` here would
        // fire every Claude hook twice.
        for spec in hook_specs(dialect("claude"), binary()) {
            assert!(
                !spec.command.contains("--format"),
                "Claude command must stay format-free, got {:?}",
                spec.command
            );
        }
    }

    #[test]
    fn every_other_dialect_selects_its_own_reader() {
        for d in DIALECTS.iter().filter(|d| d.slug != "claude") {
            for spec in hook_specs(d, binary()) {
                assert!(
                    spec.command.contains(&format!("--format {}", d.slug)),
                    "{} command must select its reader, got {:?}",
                    d.slug,
                    spec.command
                );
                assert!(spec.command.contains("agent-status"));
            }
        }
    }

    #[test]
    fn a_path_with_a_quote_cannot_break_out_of_the_command() {
        for d in DIALECTS {
            for spec in hook_specs(d, Path::new("/Users/O'X/oximux")) {
                assert!(
                    spec.command.contains(r"/Users/O'\''X/oximux"),
                    "{} left an embedded quote unescaped: {:?}",
                    d.slug,
                    spec.command
                );
            }
        }
    }

    #[test]
    fn the_grok_tool_matcher_is_a_regex_not_a_glob() {
        // Grok matches tool names with a real regular expression, where a bare
        // `*` is not "match all" — it fails to match anything, so the tool
        // hooks would install cleanly and never fire.
        let pre = dialect("grok")
            .events()
            .iter()
            .find(|e| e.event == "PreToolUse")
            .expect("a tool event");
        assert_eq!(pre.matcher, Some(".*"));
    }

    #[test]
    fn the_gemini_timeout_is_spelled_in_milliseconds() {
        // The one unit difference in the table. Five seconds' worth of
        // milliseconds would be 5, which Gemini reads as five MILLISECONDS —
        // short enough that the hook is abandoned before the socket answers.
        let Install::HooksFile {
            entry: EntryShape::Nested { timeout: Some(t), .. },
            ..
        } = &dialect("gemini").install
        else {
            panic!("gemini is a nested dialect with a timeout");
        };
        assert!(t.value >= 1_000, "a millisecond timeout of {} is a sub-second deadline", t.value);
    }

    #[test]
    fn a_synchronous_dialect_always_bounds_its_hook() {
        // An unbounded synchronous hook can stall the user's turn on a relay
        // that never answers. Only an async hook may go without.
        for d in DIALECTS {
            let bounded = match &d.install {
                Install::HooksFile { entry: EntryShape::Nested { async_command, timeout }, .. } => {
                    *async_command || timeout.is_some()
                }
                Install::HooksFile { entry: EntryShape::Flat { timeout, .. }, .. } => {
                    timeout.is_some()
                }
                // An extension runs in the agent's own process and spawns its
                // reporter detached; there is nothing for the agent to wait on.
                Install::Extension { .. } => true,
            };
            assert!(bounded, "{} runs unbounded and synchronously", d.slug);
        }
    }

    #[test]
    fn the_turn_end_payload_yields_the_agents_reply() {
        let json = r#"{"model":"gpt-5.6-sol","last_assistant_message":"Hi! What would you like to work on?"}"#;
        assert_eq!(
            last_message(dialect("codex"), json).as_deref(),
            Some("Hi! What would you like to work on?")
        );
        // Gemini spells the same thing differently, and must not read Codex's key.
        assert_eq!(last_message(dialect("gemini"), json), None);
        assert_eq!(
            last_message(dialect("gemini"), r#"{"prompt_response":"done"}"#).as_deref(),
            Some("done")
        );
        assert_eq!(
            last_message(dialect("cursor"), r#"{"text":"all tests pass"}"#).as_deref(),
            Some("all tests pass")
        );
        // Grok is measured under both spellings; the camelCase one wins.
        assert_eq!(
            last_message(
                dialect("grok"),
                r#"{"lastAssistantMessage":"first","last_assistant_message":"second"}"#
            )
            .as_deref(),
            Some("first")
        );
    }

    #[test]
    fn a_payload_without_a_reply_yields_nothing() {
        // A turn that produced no assistant text must leave the row's previous
        // reading alone rather than blanking it with an empty string.
        for d in DIALECTS {
            assert_eq!(last_message(d, r#"{"model":"x"}"#), None, "{}", d.slug);
            assert_eq!(last_message(d, r#"{"last_assistant_message":"   "}"#), None, "{}", d.slug);
        }
    }

    #[test]
    fn a_dialect_that_reads_no_transcript_ignores_a_transcript_path() {
        // Codex hands its reply over directly; chasing a path it did not mean
        // as a transcript would read some other agent's file.
        let json = r#"{"transcript_path":"/definitely/not/here.jsonl"}"#;
        assert_eq!(last_message(dialect("codex"), json), None);
        assert_eq!(last_message(dialect("gemini"), json), None);
    }

    #[test]
    fn malformed_json_is_not_an_error() {
        // A hook must never fail the agent it is attached to.
        for d in DIALECTS {
            assert_eq!(last_message(d, "not json"), None);
            assert_eq!(last_message(d, ""), None);
        }
        assert_eq!(tool_name(""), None);
        assert_eq!(prompt("[]"), None);
    }

    #[test]
    fn the_tool_name_is_read_from_any_measured_spelling() {
        assert_eq!(tool_name(r#"{"tool_name":"shell"}"#).as_deref(), Some("shell"));
        assert_eq!(tool_name(r#"{"name":"apply_patch"}"#).as_deref(), Some("apply_patch"));
        // The specific spelling wins over the generic one.
        assert_eq!(
            tool_name(r#"{"name":"generic","tool_name":"shell"}"#).as_deref(),
            Some("shell")
        );
    }

    #[test]
    fn the_prompt_is_read_from_any_accepted_spelling() {
        assert_eq!(prompt(r#"{"prompt":"fix the parser"}"#).as_deref(), Some("fix the parser"));
        assert_eq!(prompt(r#"{"user_message":"hi"}"#).as_deref(), Some("hi"));
        assert_eq!(prompt(r#"{"message":"hi"}"#).as_deref(), Some("hi"));
        // Whitespace-only is absent, not a title.
        assert_eq!(prompt(r#"{"prompt":"  "}"#), None);
    }

    #[test]
    fn every_hooks_file_sits_under_its_agents_own_home() {
        // Guards the defaults; the env-overridable homes are not asserted here
        // because tests share a process environment.
        for d in DIALECTS {
            let home = (d.home)().expect("a home dir");
            let path = d.path().expect("a resolved path");
            assert!(path.starts_with(&home), "{} writes outside its home: {path:?}", d.slug);
            let want = match d.install {
                Install::HooksFile { .. } => "json",
                Install::Extension { .. } => "ts",
            };
            assert!(
                path.extension().is_some_and(|e| e == want),
                "{} points at {path:?}, which is not a .{want} file",
                d.slug
            );
        }
    }
}
