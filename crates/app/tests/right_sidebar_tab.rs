//! Pure unit tests for `RightTab` and `visible_tabs`. No GPUI runtime needed.

use oximux_app::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};

#[test]
fn visible_tabs_with_repo_returns_all_four() {
    let tabs = visible_tabs(TabVisibility { has_repo: true });
    assert_eq!(
        tabs,
        vec![
            RightTab::Files,
            RightTab::Explorer,
            RightTab::Search,
            RightTab::SourceControl,
        ]
    );
}

#[test]
fn visible_tabs_without_repo_omits_source_control() {
    let tabs = visible_tabs(TabVisibility { has_repo: false });
    assert_eq!(
        tabs,
        vec![RightTab::Files, RightTab::Explorer, RightTab::Search]
    );
}

#[test]
fn files_tab_always_visible_regardless_of_repo() {
    // Files tab is workspace-wide — not gated on git presence.
    let with_repo = visible_tabs(TabVisibility { has_repo: true });
    let without_repo = visible_tabs(TabVisibility { has_repo: false });
    assert!(with_repo.contains(&RightTab::Files));
    assert!(without_repo.contains(&RightTab::Files));
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
fn files_tab_has_distinct_icon_label_title() {
    // Guard against accidental aliasing with another tab's chrome.
    assert_eq!(RightTab::Files.label(), "F");
    assert_eq!(RightTab::Files.title(), "Files");
    assert_eq!(RightTab::Files.icon_path(), "icons/folder.svg");
    assert_ne!(RightTab::Files.label(), RightTab::Explorer.label());
    assert_ne!(RightTab::Files.icon_path(), RightTab::Explorer.icon_path());
}
