//! Local full-text index of past agent conversations (Claude + Codex).
//!
//! An opt-in SQLite FTS5 database, separate from the app database so it can be
//! deleted and rebuilt freely: it is derived entirely from the transcripts the
//! agent CLIs already journal on disk. [`SessionIndexer`] keeps it current on
//! its own thread (append-aware, budgeted passes); [`search`] answers queries
//! over short-lived read-only connections.
//!
//! The crate takes the database path and the transcript roots as parameters —
//! where they live is the app's business.

mod cursor;
mod db;
mod indexer;
mod ingest;
mod parse_claude;
mod parse_codex;
mod query;
mod query_plan;
mod schema;
mod snippet;
mod turn;
mod watermark;
mod writer;

#[cfg(test)]
mod indexer_tests;
#[cfg(test)]
mod query_tests;

use std::path::{Path, PathBuf};

pub use db::{delete_index_files, index_size_bytes, open_reader};
pub use indexer::{IndexStatus, IndexerHandle, SessionIndexer, read_status};
pub use query::{Scope, SearchError, SearchHit, SearchPage, SearchRequest, Sort, search};
pub use snippet::Span;
pub use rusqlite::InterruptHandle;
pub use turn::Role;

/// Which agent wrote a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn as_str(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }
}

/// Transcript roots to index. A `None` root is not scanned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexSources {
    /// Claude's state root (`~/.claude`).
    pub claude_dir: Option<PathBuf>,
    /// Codex's state root (`$CODEX_HOME` or `~/.codex`).
    pub codex_dir: Option<PathBuf>,
}

impl IndexSources {
    /// The standard roots under `home`.
    pub fn for_home(home: &Path) -> Self {
        Self {
            claude_dir: Some(home.join(".claude")),
            codex_dir: Some(crate::session_log::usage_codex::state_dir(home)),
        }
    }
}

/// [`cwd_key`] of the path with symlinks resolved (`/tmp` and `/private/tmp`
/// agree), falling back to the path as written when it no longer exists.
pub fn canonical_cwd_key(path: &str) -> String {
    let resolved = std::fs::canonicalize(path).ok().and_then(|p| p.to_str().map(str::to_string));
    cwd_key(resolved.as_deref().unwrap_or(path))
}

/// The form a cwd is stored and compared in: `/` separators, no trailing
/// slash, and (on Windows, where paths are case-insensitive) lowercase. Scope
/// filters match a prefix of this on whole path components.
pub fn cwd_key(path: &str) -> String {
    let mut s = path.replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) { s.to_lowercase() } else { s }
}
