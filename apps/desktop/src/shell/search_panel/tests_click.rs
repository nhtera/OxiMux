//! Issue #73: clicking a search result must reach the host editor with an
//! absolute path and the match position, and clicking a file header must
//! collapse its group. Drives real mouse events through the rendered panel so
//! hit-testing inside the virtualized list and the scrollbar overlay is
//! covered, not just the handler bodies.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{Modifiers, TestAppContext, VisualTestContext};
use oximux_settings::{Density, Theme, Typography};

use super::SearchPanel;
use super::rg_runner::{SearchFileResult, SearchMatch, SearchResults};

type Opened = Arc<Mutex<Vec<(PathBuf, u32, u32)>>>;

fn results(root: &std::path::Path) -> SearchResults {
    let m = |line: u32, column: u32, text: &str| SearchMatch {
        line,
        column,
        line_content: text.into(),
        match_length: "allowlist".len(),
    };
    SearchResults {
        files: vec![
            SearchFileResult {
                file_path: root.join("services/control/CLAUDE.md"),
                relative_path: PathBuf::from("services/control/CLAUDE.md"),
                matches: vec![m(45, 27, "billing_timezone (a fixed allowlist)")],
            },
            SearchFileResult {
                file_path: root.join("README.md"),
                relative_path: PathBuf::from("README.md"),
                matches: vec![m(50, 4, "é allowlist"), m(60, 1, "allowlist")],
            },
        ],
        total_matches: 3,
        truncated: false,
    }
}

fn mount(cx: &mut TestAppContext) -> (gpui::Entity<SearchPanel>, &mut VisualTestContext, Opened, PathBuf) {
    // The panel's rg probe spawns a tokio process at construction, so a
    // reactor must be entered. Current-thread and never driven: the probe just
    // stays pending (no worker thread to trip the deterministic scheduler),
    // and leaking the runtime keeps it alive past the test's own teardown.
    let rt = Box::leak(Box::new(
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime"),
    ));
    std::mem::forget(rt.enter());
    cx.update(gpui_component::init);
    let root = PathBuf::from("/repo/root");
    let opened: Opened = Arc::default();
    let (panel, vcx) = cx.add_window_view(|window, cx| {
        SearchPanel::new(root.clone(), Theme::default(), Density::default(), Typography::default(), window, cx)
    });
    let sink = opened.clone();
    let root_for_results = root.clone();
    panel.update(vcx, |p, cx| {
        p.set_on_open_hit(Arc::new(move |path, line, col, _, _| sink.lock().unwrap().push((path, line, col))));
        // A non-empty query routes the body to the results list.
        p.options.query = "allowlist".into();
        p.apply_result(Ok(results(&root_for_results)), cx);
    });
    vcx.run_until_parked();
    (panel, vcx, opened, root)
}

fn click(vcx: &mut VisualTestContext, selector: &'static str) {
    let bounds = vcx.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} not rendered"));
    vcx.simulate_click(bounds.center(), Modifiers::none());
    vcx.run_until_parked();
}

#[gpui::test]
fn clicking_a_match_opens_it_at_the_match(cx: &mut TestAppContext) {
    let (_panel, vcx, opened, root) = mount(cx);
    click(vcx, "search-match-row-0-0");
    assert_eq!(*opened.lock().unwrap(), vec![(root.join("services/control/CLAUDE.md"), 45, 27)]);
    // Second file, first match: byte column 4 is UTF-16 column 3 after "é ".
    click(vcx, "search-match-row-1-0");
    assert_eq!(opened.lock().unwrap()[1], (root.join("README.md"), 50, 3));
}

#[gpui::test]
fn clicking_a_file_header_collapses_and_expands_it(cx: &mut TestAppContext) {
    let (panel, vcx, opened, root) = mount(cx);
    let key = root.join("services/control/CLAUDE.md");
    click(vcx, "search-file-row-0");
    panel.read_with(vcx, |p, _| assert!(p.collapsed_files.contains(&key)));
    assert!(vcx.debug_bounds("search-match-row-0-0").is_none(), "collapsed match still rendered");
    click(vcx, "search-file-row-0");
    panel.read_with(vcx, |p, _| assert!(!p.collapsed_files.contains(&key)));
    assert!(vcx.debug_bounds("search-match-row-0-0").is_some());
    assert!(opened.lock().unwrap().is_empty(), "a header click must not open a file");
}
