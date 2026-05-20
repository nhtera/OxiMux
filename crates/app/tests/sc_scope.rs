//! Scope-enum behavior: defaults to `All`, only `All` shows the graph.

use oximux_app::shell::source_control::scope::SourceControlScope;

#[test]
fn default_is_all() {
    assert_eq!(SourceControlScope::default(), SourceControlScope::All);
}

#[test]
fn only_all_shows_graph() {
    assert!(SourceControlScope::All.shows_graph());
    assert!(!SourceControlScope::Uncommitted.shows_graph());
}

#[test]
fn labels_match_design() {
    assert_eq!(SourceControlScope::All.label(), "All");
    assert_eq!(SourceControlScope::Uncommitted.label(), "Uncommitted");
}
