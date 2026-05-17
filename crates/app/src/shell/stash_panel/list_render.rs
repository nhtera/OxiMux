//! Pure render-data helpers for the stash list. Tests live alongside in
//! `tests/stash_panel_list.rs`.

use oximux_core::StashEntry;

/// Single-line label for a stash row: `stash@{N}  [branch]  message`.
/// Branch and message both trim; an empty message becomes `(no message)`.
pub fn row_label(entry: &StashEntry) -> String {
    let msg = if entry.message.trim().is_empty() {
        "(no message)".to_string()
    } else {
        entry.message.trim().to_string()
    };
    format!(
        "stash@{{{idx}}}  [{branch}]  {msg}",
        idx = entry.stash_ref.index,
        branch = entry.branch
    )
}
