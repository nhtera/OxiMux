//! Pure unit tests for `RightTab` and `visible_tabs`. No GPUI runtime needed.

use oximux_app::shell::right_sidebar::tab::{RightTab, TabVisibility, visible_tabs};

#[test]
fn visible_tabs_with_repo_returns_all_three() {
    let tabs = visible_tabs(TabVisibility { has_repo: true });
    assert_eq!(
        tabs,
        vec![
            RightTab::Explorer,
            RightTab::Search,
            RightTab::SourceControl
        ]
    );
}

#[test]
fn visible_tabs_without_repo_omits_source_control() {
    let tabs = visible_tabs(TabVisibility { has_repo: false });
    assert_eq!(tabs, vec![RightTab::Explorer, RightTab::Search]);
}

#[test]
fn right_tab_roundtrip_derives() {
    // Verify Copy + PartialEq are usable as the spec requires.
    let tab = RightTab::SourceControl;
    let copied = tab; // Copy
    assert_eq!(tab, copied); // PartialEq
    assert_ne!(tab, RightTab::Explorer);
    assert_ne!(tab, RightTab::Search);
}
