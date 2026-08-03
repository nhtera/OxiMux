//! Async project file index for Quick Open.
//!
//! Lists the project's tracked + untracked-but-not-ignored files via
//! `rg --files` (same runtime dependency the search panel already relies
//! on; respects `.gitignore`). Capped so a giant monorepo can't blow up
//! the O(n) fuzzy matcher or the (non-virtualized) result column.

use std::path::PathBuf;
use std::process::Stdio;

use oximux_no_window::NoWindow as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::shell::tool_paths::rg_program;

/// Hard cap on indexed paths. Bounds memory + keeps `filter_and_rank`
/// snappy on huge repos. A streaming matcher (`nucleo`) is the follow-up
/// if this proves limiting (see `match_engine.rs` note).
const MAX_ENTRIES: usize = 20_000;

/// Why a scan produced no usable index.
#[derive(Debug)]
pub enum ScanError {
    /// `rg` is not installed / not on PATH.
    RgMissing,
    /// `rg` spawned but failed (io error, etc).
    Failed(String),
}

impl ScanError {
    /// A user-facing, copyable hint rendered in the palette when the index
    /// can't be built.
    pub fn hint(&self) -> String {
        match self {
            // The packaged app bundles rg, so this state is reachable only in
            // dev builds (cargo run) on a machine without ripgrep.
            ScanError::RgMissing => {
                "ripgrep missing (dev build?) — `brew install ripgrep`, or rebuild the app bundle"
                    .to_string()
            }
            ScanError::Failed(e) => format!("file scan failed: {e}"),
        }
    }
}

/// Scan `root` for files, returning project-relative paths in `rg`'s order.
/// Respects `.gitignore`. Capped at [`MAX_ENTRIES`]; on overflow the index
/// is truncated (Quick Open degrades to the first N files rather than
/// stalling). Spawns off the UI thread — call from a tokio runtime.
pub async fn scan_files(root: PathBuf) -> Result<Vec<String>, ScanError> {
    let mut cmd = Command::new(rg_program());
    cmd.arg("--files")
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .no_window()
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ScanError::RgMissing),
        Err(e) => return Err(ScanError::Failed(e.to_string())),
    };

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ScanError::Failed("missing rg stdout".to_string()))?;
    let mut lines = BufReader::new(stdout).lines();

    let mut out = Vec::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        out.push(line);
        if out.len() >= MAX_ENTRIES {
            break;
        }
    }
    // `child` (kill_on_drop) is dropped at return — if we broke early on the
    // cap, that terminates rg instead of leaking it blocked on a full pipe.
    Ok(out)
}
