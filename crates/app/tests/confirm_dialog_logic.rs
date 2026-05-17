//! Pure-unit tests for the type-to-confirm match predicate.

use oximux_app::shell::confirm_dialog::logic::is_match;

#[test]
fn empty_expected_never_matches_even_empty_input() {
    // Guard against a UX trap: an empty expected string would auto-enable
    // the destructive button regardless of input.
    assert!(!is_match("", ""));
    assert!(!is_match("anything", ""));
}

#[test]
fn exact_match_passes() {
    assert!(is_match("delete-me", "delete-me"));
}

#[test]
fn trailing_whitespace_in_typed_input_is_trimmed() {
    assert!(is_match("delete-me   ", "delete-me"));
    assert!(is_match("\tdelete-me\n", "delete-me"));
}

#[test]
fn case_sensitive_match() {
    assert!(!is_match("Delete-Me", "delete-me"));
    assert!(!is_match("DELETE-ME", "delete-me"));
}

#[test]
fn partial_match_does_not_pass() {
    assert!(!is_match("delete", "delete-me"));
    assert!(!is_match("delete-me-and-extra", "delete-me"));
}

#[test]
fn path_with_slashes_matches_exactly() {
    assert!(is_match("src/main.rs", "src/main.rs"));
    assert!(!is_match("main.rs", "src/main.rs"));
}
