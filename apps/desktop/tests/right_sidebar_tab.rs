//! Pure unit tests for `RightTab` and `visible_tabs`. No GPUI runtime needed.

use oximux_app::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};

#[test]
fn a_repo_project_shows_the_full_tab_row_in_order() {
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
            // History and Ports are repo-independent and trail the row.
            RightTab::History,
            RightTab::Ports,
        ]
    );
}

#[test]
fn only_source_control_drops_when_there_is_no_repo() {
    let tabs = visible_tabs(TabVisibility { has_repo: false });
    assert_eq!(
        tabs,
        vec![
            RightTab::Explorer,
            RightTab::Search,
            RightTab::History,
            RightTab::Ports
        ]
    );
}

#[test]
fn ports_is_visible_whether_or_not_the_project_is_a_repo() {
    // A dev server listens the same either way. Regression guard: Ports was
    // added beside History precisely because both are repo-independent, and
    // grouping it with SourceControl would silently hide it for the projects
    // most likely to be a scratch directory with a server in it.
    for has_repo in [true, false] {
        assert!(
            visible_tabs(TabVisibility { has_repo }).contains(&RightTab::Ports),
            "Ports must survive has_repo = {has_repo}"
        );
    }
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
