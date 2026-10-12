//! `oximux sim run` and `sim flow` arguments (v30), and the exact-selector
//! flags target-taking verbs share. Kept beside `cli.rs` so that file stays
//! small.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use oximux_remote_proto::simulator::{SimSelectorWire, SimTargetWire};

/// `--exact`, `--role`, `--nth`: name an element exactly.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExactArgs {
    /// Match the label or identifier exactly (no substring fallback).
    #[arg(long)]
    pub exact: bool,
    /// Only elements of this role (`Button`, `TextField`, … as `ax` prints
    /// them); exact.
    #[arg(long, value_name = "ROLE")]
    pub role: Option<String>,
    /// The Nth such element on screen, from 0, in `ax` order.
    #[arg(long, value_name = "N")]
    pub nth: Option<u32>,
}

impl ExactArgs {
    /// Whether any of the flags is given (the verb then needs a v30 host).
    pub fn used(&self) -> bool {
        self.exact || self.role.is_some() || self.nth.is_some()
    }

    /// The exact target for `label`/`id` with these flags; `None` when no
    /// flag asks for one.
    pub fn target(&self, label: Option<&String>, id: Option<&String>) -> Option<SimTargetWire> {
        self.used().then(|| {
            SimTargetWire::Exact(SimSelectorWire { label: label.cloned(), id: id.cloned(), role: self.role.clone(), nth: self.nth.unwrap_or(0) })
        })
    }
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// The steps: one `oximux sim` command per line without the prefix
    /// (`tap --label "Sign in" --exact`), `#` comments. `-` reads stdin.
    #[arg(value_name = "FILE")]
    pub file: PathBuf,
    /// Settle after every action step.
    #[arg(long)]
    pub settle: bool,
    /// Start at step N (from 1), e.g. to resume after exit 7.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub from: usize,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum FlowCommand {
    /// Record this worktree's agent verbs on its device, from now.
    Start,
    /// Stop recording: the steps are saved in OxiMux's data (private) and the
    /// path printed, or written to --out.
    Stop {
        /// Write the flow file here instead.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    /// Whether a recording runs, and its steps so far.
    Status,
    /// Turn a flow file into another tool's format.
    Export {
        /// The flow file (`-` reads stdin).
        file: PathBuf,
        /// As Maestro YAML (the only format so far).
        #[arg(long, required = true)]
        maestro: bool,
        /// Write it here instead of stdout.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
        /// The app the flow drives (default: the file's `# app`, else its
        /// first launch).
        #[arg(long, value_name = "ID")]
        app_id: Option<String>,
    },
}
