//! Pure unit tests for `RightTab` and `visible_tabs`. No GPUI runtime needed.

use oximux_app::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};

#[test]
fn visible_tabs_with_repo_returns_explorer_search_source_control() {
    // `Files` is intentionally hidden from `visible_tabs` (see
    // tab.rs::visible_tabs doc). The variant + view code remain so the
    // editor-crate FileTree model + watcher stay reachable; the surface
    // re-appears here once LSP-aware affordances justify a second file
    // tab under a non-"Files" label.
    let tabs = visible_tabs(TabVisibility { has_repo: true });
    assert_eq!(
        tabs,
        vec![
            RightTab::Explorer,
            RightTab::Search,
            RightTab::SourceControl,
            // History is repo-independent and always trails the tab row.
            RightTab::History,
        ]
    );
}

#[test]
fn visible_tabs_without_repo_omits_source_control_and_files() {
    let tabs = visible_tabs(TabVisibility { has_repo: false });
    // Source Control drops without a repo; History stays (repo-independent).
    assert_eq!(tabs, vec![RightTab::Explorer, RightTab::Search, RightTab::History]);
}

#[test]
fn files_tab_hidden_from_visible_in_both_repo_states() {
    // Guard against accidentally re-exposing Files before the LSP-aware
    // re-launch lands. Flip the assertion when intentionally reintroducing.
    let with_repo = visible_tabs(TabVisibility { has_repo: true });
    let without_repo = visible_tabs(TabVisibility { has_repo: false });
    assert!(!with_repo.contains(&RightTab::Files));
    assert!(!without_repo.contains(&RightTab::Files));
}

#[test]
fn right_tab_roundtrip_derives() {
    // Verify Copy + PartialEq are usable as the spec requires.
    let tab = RightTab::SourceControl;
    let copied = tab; // Copy
    assert_eq!(tab, copied); // PartialEq
    assert_ne!(tab, RightTab::Explorer);
    assert_ne!(tab, RightTab::Files);
}

#[test]
fn files_tab_metadata_still_present_for_future_reintroduction() {
    // Even though Files is not in `visible_tabs`, the variant must keep
    // its metadata (icon / label / title) so re-exposing later is a
    // one-line change in `visible_tabs` rather than a metadata rebuild.
    assert_eq!(RightTab::Files.label(), "F");
    assert_eq!(RightTab::Files.title(), "Files");
    assert_eq!(RightTab::Files.icon_path(), "icons/folder.svg");
    assert_ne!(RightTab::Files.label(), RightTab::Explorer.label());
    assert_ne!(RightTab::Files.icon_path(), RightTab::Explorer.icon_path());
}
