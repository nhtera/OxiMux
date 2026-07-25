//! Substring filter for the changed-files list. Pure helper so the entity
//! module stays focused on render orchestration.

use oximux_core::FileStatus;

/// Filter `files` to entries whose relative path contains `query` (case
/// insensitive). Empty / whitespace-only query is a pass-through.
pub fn filter_files<'a>(files: &'a [FileStatus], query: &str) -> Vec<&'a FileStatus> {
    let needle = query.trim();
    if needle.is_empty() {
        return files.iter().collect();
    }
    let lower = needle.to_lowercase();
    files
        .iter()
        .filter(|f| f.path.to_string_lossy().to_lowercase().contains(&lower))
        .collect()
}
