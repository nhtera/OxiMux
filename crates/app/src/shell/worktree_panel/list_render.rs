//! Pure helpers for worktree row rendering. Decouples label/badge logic
//! from GPUI so tests run as plain Rust.

use oximux_core::WorktreeInfo;

/// Suggest a worktree directory next to the main workdir using the slug
/// as the leaf name. The host can override; this just gives a sensible
/// default in the "Path" input.
///
/// Format: `<parent>/oximux-wt-<slug>` where parent is the directory
/// containing `main_workdir`. If `main_workdir` has no parent (e.g. `/`)
/// the suggestion falls back to `./oximux-wt-<slug>`.
pub fn suggest_worktree_path(main_workdir: &std::path::Path, slug: &str) -> String {
    let leaf = format!("oximux-wt-{slug}");
    match main_workdir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(&leaf).display().to_string(),
        _ => format!("./{leaf}"),
    }
}

/// Single-line row label: `[branch]  <path>` for linked worktrees, or
/// `[branch]  <path>  (main)` for the main worktree. Detached HEAD shows
/// `[detached HEAD]`. Locked appends `🔒` (single glyph, no theme dep).
pub fn row_label(w: &WorktreeInfo) -> String {
    let branch = match &w.branch {
        Some(b) => format!("[{b}]"),
        None => "[detached HEAD]".to_string(),
    };
    let path = w.path.display();
    let main = if w.is_main { "  (main)" } else { "" };
    let locked = if w.is_locked { "  🔒" } else { "" };
    format!("{branch}  {path}{main}{locked}")
}
