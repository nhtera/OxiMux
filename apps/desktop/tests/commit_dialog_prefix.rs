//! Pure-unit tests for the conventional-commit prefix table + message
//! assembly. No GPUI, no tokio.

use oximux_app::shell::commit_dialog::prefix::{
    PREFIXES, SUBJECT_WARN_LEN, assemble_message, next_prefix_index, prefix_label,
    subject_over_warn,
};

#[test]
fn prefix_table_has_no_prefix_slot_first() {
    assert_eq!(PREFIXES[0], None);
}

#[test]
fn standard_conventional_prefixes_present() {
    let labels: Vec<&str> = PREFIXES.iter().filter_map(|p| *p).collect();
    for required in ["feat", "fix", "docs", "refactor", "test", "chore"] {
        assert!(labels.contains(&required), "missing prefix: {required}");
    }
}

#[test]
fn cycle_wraps_at_end_of_table() {
    let last = PREFIXES.len() - 1;
    assert_eq!(next_prefix_index(last), 0);
    assert_eq!(next_prefix_index(0), 1);
}

#[test]
fn label_for_none_slot_is_descriptive() {
    assert_eq!(prefix_label(0), "(no prefix)");
}

#[test]
fn label_for_feat_returns_feat() {
    let feat_idx = PREFIXES.iter().position(|p| *p == Some("feat")).unwrap();
    assert_eq!(prefix_label(feat_idx), "feat");
}

#[test]
fn empty_subject_returns_none() {
    assert!(assemble_message(0, "", "body").is_none());
    assert!(assemble_message(0, "   ", "").is_none());
    assert!(assemble_message(1, "\n\n", "").is_none());
}

#[test]
fn no_prefix_no_body_returns_bare_subject() {
    assert_eq!(
        assemble_message(0, "hello world", "").unwrap(),
        "hello world"
    );
}

#[test]
fn no_prefix_with_body_separates_with_blank_line() {
    assert_eq!(
        assemble_message(0, "subject", "body line").unwrap(),
        "subject\n\nbody line"
    );
}

#[test]
fn feat_prefix_prepends_with_colon_space() {
    let feat_idx = PREFIXES.iter().position(|p| *p == Some("feat")).unwrap();
    assert_eq!(
        assemble_message(feat_idx, "add login", "").unwrap(),
        "feat: add login"
    );
}

#[test]
fn feat_prefix_with_body_uses_full_format() {
    let feat_idx = PREFIXES.iter().position(|p| *p == Some("feat")).unwrap();
    assert_eq!(
        assemble_message(feat_idx, "add login", "context line").unwrap(),
        "feat: add login\n\ncontext line"
    );
}

#[test]
fn whitespace_only_body_is_dropped() {
    let feat_idx = PREFIXES.iter().position(|p| *p == Some("feat")).unwrap();
    assert_eq!(
        assemble_message(feat_idx, "subj", "   \n  \t ").unwrap(),
        "feat: subj"
    );
}

#[test]
fn subject_warn_triggers_only_over_50_chars() {
    let at_limit = "a".repeat(SUBJECT_WARN_LEN);
    assert!(!subject_over_warn(&at_limit));
    let over = "a".repeat(SUBJECT_WARN_LEN + 1);
    assert!(subject_over_warn(&over));
}

#[test]
fn subject_warn_counts_chars_not_bytes() {
    // 50 ascii + 1 multi-byte char → exceeds
    let mut s = "a".repeat(50);
    s.push('é');
    assert!(subject_over_warn(&s));
    // 49 ascii + 1 multi-byte char → still at limit, no warn
    let mut s = "a".repeat(49);
    s.push('é');
    assert!(!subject_over_warn(&s));
}
