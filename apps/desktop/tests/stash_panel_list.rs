//! Pure-unit tests for the stash row-label formatter.

use oximux_app::shell::stash_panel::list_render::row_label;
use oximux_core::{StashEntry, StashRef};

fn entry(index: usize, branch: &str, message: &str) -> StashEntry {
    StashEntry {
        stash_ref: StashRef { index },
        branch: branch.to_string(),
        message: message.to_string(),
    }
}

#[test]
fn label_combines_index_branch_and_message() {
    let e = entry(0, "main", "feat(api): hello");
    assert_eq!(
        row_label(&e),
        "stash@{0}  [main]  feat(api): hello".to_string()
    );
}

#[test]
fn higher_index_renders_correctly() {
    let e = entry(12, "feature", "fix typo");
    assert_eq!(row_label(&e), "stash@{12}  [feature]  fix typo");
}

#[test]
fn empty_message_becomes_no_message_placeholder() {
    let e = entry(0, "main", "");
    assert_eq!(row_label(&e), "stash@{0}  [main]  (no message)");
}

#[test]
fn whitespace_only_message_becomes_no_message_placeholder() {
    let e = entry(0, "main", "   \n\t  ");
    assert_eq!(row_label(&e), "stash@{0}  [main]  (no message)");
}

#[test]
fn message_with_surrounding_whitespace_is_trimmed() {
    let e = entry(0, "main", "  WIP  ");
    assert_eq!(row_label(&e), "stash@{0}  [main]  WIP");
}
