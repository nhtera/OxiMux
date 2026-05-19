//! Async filesystem helpers for the file explorer.
//!
//! Wraps `tokio::fs::read_dir` with the should_include filter and produces
//! sorted `TreeNode` slices ready for the cache. `depth` is always 0 here;
//! `flatten()` in `tree_state` fills it during flat-row construction.
//!
//! Symlinks are skipped entirely (v1 — avoids loop hazard).
//! A 5-second timeout guards slow network mounts.

use crate::shell::file_explorer::tree_state::{DirCache, TreeNode, should_include};
use std::path::PathBuf;
use std::time::Duration;

/// Read one directory, filter excluded names and symlinks, and return sorted
/// children.
///
/// Sort order: directories first, then files; within each group
/// case-insensitive ascending by name. `depth` is set to 0 — the caller's
/// `flatten()` assigns real depth values.
///
/// `repo_root` is used to compute `relative_path` for each entry.
pub async fn read_dir_filtered(
    path: PathBuf,
    repo_root: PathBuf,
) -> Result<Vec<TreeNode>, std::io::Error> {
    let mut rd = tokio::fs::read_dir(&path).await?;
    let mut out: Vec<TreeNode> = Vec::new();

    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !should_include(&name) {
            continue;
        }
        // Use file_type() — does NOT follow symlinks. Skip symlinks entirely
        // for v1 to avoid loop hazard on repos with self-referential links.
        let ft = entry.file_type().await?;
        if ft.is_symlink() {
            continue;
        }
        let abs_path = entry.path();
        let relative_path = abs_path
            .strip_prefix(&repo_root)
            .unwrap_or(&abs_path)
            .to_path_buf();
        out.push(TreeNode {
            name,
            path: abs_path,
            relative_path,
            is_directory: ft.is_dir(),
            depth: 0,
        });
    }

    // dirs first, then files; ties broken case-insensitively by name
    out.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(out)
}

/// Convenience wrapper: load a directory and wrap it in a `DirCache`.
///
/// Applies a 5-second timeout to guard slow network mounts. On timeout, logs
/// a warning and returns an empty-but-loaded cache so the UI doesn't spin
/// forever.
pub async fn load_dir_cache(path: PathBuf, repo_root: PathBuf) -> DirCache {
    match tokio::time::timeout(
        Duration::from_secs(5),
        read_dir_filtered(path.clone(), repo_root),
    )
    .await
    {
        Ok(Ok(children)) => DirCache {
            children,
            loading: false,
            loaded: true,
        },
        Ok(Err(e)) => {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %path.display(),
                error = %e,
                "dir read failed"
            );
            DirCache {
                children: vec![],
                loading: false,
                loaded: true,
            }
        }
        Err(_) => {
            tracing::warn!(
                target: "oximux_app::file_explorer",
                path = %path.display(),
                "dir read timed out after 5s"
            );
            DirCache {
                children: vec![],
                loading: false,
                loaded: true,
            }
        }
    }
}
