//! The clap derive tree — the single source of truth for parsing, `--help`,
//! and the `agent-context` schema dump. Nothing here touches a socket or a
//! database: construction must stay free of side effects so `--help` and typo
//! paths cost nothing.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    #[arg(long, global = true)]
    pub json: bool,

    /// The host's runtime directory (where its control socket lives).
    /// Defaults to this machine's OxiMux data directory.
    #[arg(long, global = true, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Seconds to wait for a host reply before giving up (exit 4).
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
    /// Project verbs.
    Projects {
        #[command(subcommand)]
        command: ProjectsCommand,
    },
    /// Print this CLI's build and protocol versions (offline).
    Version,
    /// Print the full command schema as JSON, for agents driving this CLI
    /// (offline — never touches the host).
    AgentContext,
}

#[derive(Subcommand, Debug)]
pub enum ProjectsCommand {
    /// List the projects the host offers as new-session targets.
    Ls,
}
