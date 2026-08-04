//! The clap derive tree — the single source of truth for parsing, `--help`,
//! and the `agent-context` schema dump. Nothing here touches a socket or a
//! database: construction must stay free of side effects so `--help` and typo
//! paths cost nothing.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Exit codes, stated once. `2` is what clap itself exits with on a usage
/// error, so the contract holds without wrapping the parser.
pub mod exit {
    pub const OK: u8 = 0;
    pub const ERROR: u8 = 1;
    pub const USAGE: u8 = 2;
    pub const UNREACHABLE: u8 = 3;
    pub const TIMEOUT: u8 = 4;
    pub const DENIED: u8 = 5;
}

/// Drive a running OxiMux host from the command line.
///
/// Commands that talk to a host need local CLI access enabled in the desktop
/// app (Settings → Remote). Async contract: sending a prompt or command is
/// acknowledged when the host ACCEPTS it, not when the agent finishes — watch
/// the session for completion.
///
/// Exit codes: 0 ok · 1 error · 2 usage · 3 host unreachable · 4 timed out ·
/// 5 access denied.
#[derive(Parser, Debug)]
#[command(name = "oximux", version, about, verbatim_doc_comment)]
pub struct Cli {
    /// Emit machine-readable JSON on stdout (one convention, every verb).
    /// Streaming verbs (run/send/attach/wait) emit NDJSON event lines, then a
    /// final result object.
    #[arg(long, global = true)]
    pub json: bool,

    /// The host's runtime directory (where its control socket lives).
    /// Defaults to this machine's OxiMux data directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Seconds to wait for a host reply before giving up (exit 4). For `wait`,
    /// this is the overall bound on the wait itself.
    #[arg(long, global = true, default_value_t = 10, value_name = "SECS")]
    pub timeout: u64,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Is a host reachable, and what is it running? (versions, session counts)
    Status,
    /// List the host's agent sessions.
    Ls,
    /// Start an agent session and send it a prompt.
    ///
    /// Async contract: the host acknowledges when it ACCEPTS the prompt, not
    /// when the agent finishes. By default this stays attached and streams the
    /// turn to completion; `--bg` prints the new session id and exits
    /// immediately — pair it with `wait` or `attach`.
    #[command(verbatim_doc_comment)]
    Run {
        /// The prompt to send.
        prompt: String,
        /// Which configured agent to start (default: the host's default agent).
        #[arg(long, value_name = "AGENT_ID")]
        agent: Option<String>,
        /// Switch the session to this model before sending the prompt.
        #[arg(long, value_name = "MODEL_ID")]
        model: Option<String>,
        /// Working directory for the session (default: the current directory).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Create a worktree with this slug under the project first, and start
        /// the session inside it. The project is the `--cwd` (or current)
        /// directory, which must be a project the host knows.
        #[arg(long, value_name = "SLUG")]
        worktree: Option<String>,
        /// Print the session id and exit instead of staying attached.
        #[arg(long)]
        bg: bool,
    },
    /// Stream a session's live events to the terminal.
    ///
    /// Ctrl+C detaches; the agent keeps running. If the stream gaps past the
    /// host's replay backlog, the fold is resynced from the transcript and a
    /// `— resynced —` marker is printed rather than losing events silently.
    #[command(verbatim_doc_comment)]
    Attach {
        /// The session id (see `oximux ls`).
        session: String,
        /// Replay retained events after this sequence number first
        /// (default: attach at the live edge).
        #[arg(long, value_name = "SEQ")]
        from: Option<u64>,
    },
    /// Send a prompt into an existing session.
    ///
    /// Async contract: the host acknowledges when it ACCEPTS the prompt, not
    /// when the agent finishes. By default this stays attached and streams the
    /// turn to completion; `--no-wait` returns right after the acknowledgment —
    /// pair it with `wait` or `attach`.
    #[command(verbatim_doc_comment)]
    Send {
        /// The session id (see `oximux ls`).
        session: String,
        /// The prompt to send.
        prompt: String,
        /// Return as soon as the host accepts the prompt.
        #[arg(long)]
        no_wait: bool,
    },
    /// Block until a session reaches a state (or --timeout expires, exit 4).
    Wait {
        /// The session id (see `oximux ls`).
        session: String,
        /// The state to wait for.
        #[arg(long, value_enum)]
        until: WaitUntil,
    },
    /// Fetch a session's full transcript (paged under the hood).
    Transcript {
        /// The session id (see `oximux ls`).
        session: String,
    },
    /// Interrupt a session's in-flight turn. The session stays open.
    Stop {
        /// The session id (see `oximux ls`).
        session: String,
    },
    /// Redirect a mid-turn agent with additional guidance.
    Steer {
        /// The session id (see `oximux ls`).
        session: String,
        /// The guidance to inject.
        text: String,
    },
    /// See and decide a session's pending permission requests and questions.
    Permit {
        #[command(subcommand)]
        command: PermitCommand,
    },
    /// See and switch a session's model.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Switch a session's permission mode.
    Mode {
        #[command(subcommand)]
        command: ModeCommand,
    },
    /// Git status, diffs, staging, and commits for a session's repository.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// See and attach to the host's terminals.
    Term {
        #[command(subcommand)]
        command: TermCommand,
    },
    /// Create, list, and remove project worktrees on the host.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Project verbs.
    Projects {
        #[command(subcommand)]
        command: ProjectsCommand,
    },
    /// Scheduled agent runs: create, list, pause, and fire them on the host.
    ///
    /// A schedule sends its prompt into a fresh session on a cadence. It fires
    /// only while a host is running (the desktop app or `oximux serve`);
    /// missed occurrences are skipped forward, never replayed in a burst.
    #[command(verbatim_doc_comment)]
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },
    /// Run this machine as a headless OxiMux host.
    ///
    /// Boots the same session/terminal/storage stack the desktop app hosts —
    /// minus every window — then serves the local CLI socket and the paired-
    /// device endpoint until SIGTERM/Ctrl+C, draining in-flight agent turns
    /// before exiting. Stdout carries exactly one readiness JSON line; logs go
    /// to stderr. Pair devices at runtime with `pair-new` (never a boot flag,
    /// so no ticket ever lands in a journal).
    #[command(verbatim_doc_comment)]
    Serve {
        /// The data directory (default: this machine's OxiMux data dir, shared
        /// with the desktop app so sessions and pairings are one set).
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// A project root to offer as a new-session target (repeatable; also
        /// read from <data-dir>/projects.toml).
        #[arg(long = "project", value_name = "DIR")]
        projects: Vec<PathBuf>,
        /// Run under the Service Control Manager. Set by the installed
        /// service's own command line; not for interactive use.
        #[cfg(windows)]
        #[arg(long, hide = true)]
        service: bool,
        /// Register `oximux serve` as a Windows service (requires an elevated
        /// prompt and an explicit --data-dir; start it with `sc start`).
        #[cfg(windows)]
        #[arg(long, conflicts_with_all = ["service", "uninstall_service"])]
        install_service: bool,
        /// Stop (best-effort) and remove the Windows service.
        #[cfg(windows)]
        #[arg(long, conflicts_with = "service")]
        uninstall_service: bool,
    },
    /// Mint a one-time, short-lived pairing ticket on the running host.
    ///
    /// Prints the ticket (and its QR) to an interactive terminal ONLY — it is
    /// a bearer credential, and a journal that captured it would stay
    /// redeemable for its window. The enrollment it mints has full write
    /// access unless --read-only opts it down.
    #[command(verbatim_doc_comment)]
    PairNew {
        /// Mint a read-only enrollment (it can watch, never act).
        #[arg(long)]
        read_only: bool,
        /// Print the ticket even though stdout is not a terminal. You are
        /// taking responsibility for where it lands.
        #[arg(long)]
        force_non_tty: bool,
    },
    /// List the host's paired devices, tier and revocation included.
    PairLs,
    /// Erase one device's enrollment (it may pair again with a fresh ticket).
    PairRm {
        /// The device's public key, as `pair-ls` printed it (64 hex chars).
        pubkey: String,
    },
    /// Print this CLI's build and protocol versions (offline).
    Version,
    /// Print the full command schema as JSON, for agents driving this CLI
    /// (offline — never touches the host).
    AgentContext,
}

/// The states `wait --until` accepts.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitUntil {
    /// The current turn has completed (a session with no turn in flight
    /// already counts as done).
    Done,
    /// A permission request or question is awaiting a decision.
    NeedsApproval,
    /// Done, and nothing is awaiting a decision.
    Idle,
}

#[derive(Subcommand, Debug)]
pub enum PermitCommand {
    /// List a session's pending permission requests and questions.
    Ls {
        /// The session id (see `oximux ls`).
        session: String,
    },
    /// Approve a pending permission request.
    Allow {
        /// The session id (see `oximux ls`).
        session: String,
        /// The request id (from `permit ls`; default: the latest pending).
        request: Option<String>,
    },
    /// Deny a pending permission request.
    Deny {
        /// The session id (see `oximux ls`).
        session: String,
        /// The request id (from `permit ls`; default: the latest pending).
        request: Option<String>,
        /// The reason shown to the agent.
        #[arg(long, default_value = "Denied from the CLI")]
        message: String,
    },
    /// Answer a pending multiple-choice question.
    ///
    /// Interactive on a terminal (a numbered picker per question); headless
    /// with `--answer`, one per question in order — an option label, a 1-based
    /// option number, or free text.
    #[command(verbatim_doc_comment)]
    Answer {
        /// The session id (see `oximux ls`).
        session: String,
        /// The request id (from `permit ls`; default: the latest pending).
        request: Option<String>,
        /// One answer per question, in order (label, 1-based number, or text).
        #[arg(long, value_name = "ANSWER")]
        answer: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModelCommand {
    /// List the models (and modes) the session's backend offers.
    Ls {
        /// The session id (see `oximux ls`).
        session: String,
    },
    /// Switch the session's model. May be refused by backends that fix the
    /// model at spawn when no desktop view can respawn the child.
    Set {
        /// The session id (see `oximux ls`).
        session: String,
        /// The model id (from `model ls`).
        model: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModeCommand {
    /// Switch the session's permission mode (ids from `model ls`).
    Set {
        /// The session id (see `oximux ls`).
        session: String,
        /// The mode id (from `model ls`).
        mode: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum GitCommand {
    /// Working-tree status of the session's repository.
    Status {
        /// The session id (see `oximux ls`).
        session: String,
    },
    /// Diff one path (as listed by `git status`).
    Diff {
        /// The session id (see `oximux ls`).
        session: String,
        /// Repository-relative path, as `git status` listed it.
        path: String,
        /// Diff the index against HEAD instead of the worktree.
        #[arg(long)]
        staged: bool,
        /// The path is untracked (read from disk, not from git).
        #[arg(long)]
        untracked: bool,
    },
    /// Stage paths into the index.
    Stage {
        /// The session id (see `oximux ls`).
        session: String,
        /// Repository-relative paths, as `git status` listed them.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Remove paths from the index, leaving the worktree untouched.
    Unstage {
        /// The session id (see `oximux ls`).
        session: String,
        /// Repository-relative paths, as `git status` listed them.
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// Commit what is already staged.
    Commit {
        /// The session id (see `oximux ls`).
        session: String,
        /// The commit message.
        #[arg(short, long)]
        message: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum TermCommand {
    /// List the host's terminals.
    Ls,
    /// Attach to a terminal: keystrokes go to the host, output comes back.
    /// Ctrl+] detaches; the terminal keeps running.
    #[command(verbatim_doc_comment)]
    Attach {
        /// The terminal id (see `term ls`).
        pty: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreeCommand {
    /// Create a worktree (and its branch) under a project. The host derives
    /// the on-disk location; the reply carries it.
    Create {
        /// The worktree slug (becomes branch `oximux/<slug>`).
        slug: String,
        /// The project's root path (default: the current directory). Must be a
        /// project the host knows (see `projects ls`).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
    /// List worktrees (all projects unless --project narrows it).
    Ls {
        /// A project root path to narrow to (see `projects ls`).
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
    },
    /// Remove a worktree by id (see `worktree ls`). Refused (not forced) when
    /// the worktree has uncommitted changes.
    Rm {
        /// The worktree id (from `worktree ls`).
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    /// List the projects the host offers as new-session targets.
    Ls,
}

#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    /// Create a schedule. Exactly one cadence flag is required.
    Create {
        /// The prompt each fire sends into its fresh session.
        prompt: String,
        /// A name for the schedule (shown in lists).
        #[arg(long)]
        name: String,
        /// Working directory for each run's session (default: the current
        /// directory).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// Which configured agent runs it (default: the host's default agent).
        #[arg(long, value_name = "AGENT_ID")]
        agent: Option<String>,
        /// Fire every N minutes (minimum 5).
        #[arg(long, value_name = "MINUTES", conflicts_with_all = ["daily", "weekly"])]
        every: Option<u32>,
        /// Fire daily at this local time, e.g. 09:00.
        #[arg(long, value_name = "HH:MM", conflicts_with = "weekly")]
        daily: Option<String>,
        /// Fire weekly at this day and local time, e.g. "mon 09:00".
        #[arg(long, value_name = "DAY HH:MM")]
        weekly: Option<String>,
    },
    /// List schedules with cadence, next fire, and state.
    Ls,
    /// A schedule's recent run history, newest first.
    Logs {
        /// The schedule id (from `schedule ls`).
        id: String,
        /// How many runs to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Pause a schedule (it keeps its history; resume re-arms from now).
    Pause {
        /// The schedule id (from `schedule ls`).
        id: String,
    },
    /// Resume a paused schedule — armed forward from now, no catch-up fire.
    Resume {
        /// The schedule id (from `schedule ls`).
        id: String,
    },
    /// Fire a schedule immediately. Its cadence is untouched: the next
    /// scheduled occurrence still fires on time. Waits for the run to start
    /// (or fail to) and reports the recorded outcome.
    RunOnce {
        /// The schedule id (from `schedule ls`).
        id: String,
    },
    /// Delete a schedule and its run history.
    Rm {
        /// The schedule id (from `schedule ls`).
        id: String,
    },
}
