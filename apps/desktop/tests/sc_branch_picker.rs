//! Pure-data tests for the SCM branch-picker primitive. Source lives at
//! `apps/desktop/src/shell/source_control/branch_picker.rs`; tests
//! extracted here so the production module stays under the file-size
//! warn cap (matching the `sc_dropdown_items.rs` extraction precedent).
//!
//! Only the pure helpers (`filter_branches`, `build_rows`,
//! `selectable_count`, `selectable_to_absolute`, `wrap_selectable`)
//! are exercised — the GPUI entity surface (`BranchPicker`) needs a
//! `TestAppContext` and is covered by the integration tests added when
//! the picker is wired into the toolbar.

use oximux_app::shell::source_control::branch_picker::{
    DisplayRow, PickerMode, PickerOutcome, build_rows, filter_branches, selectable_count,
    selectable_to_absolute, wrap_selectable,
};

fn names(strs: &[&str]) -> Vec<String> {
    strs.iter().map(|s| s.to_string()).collect()
}

#[test]
fn filter_empty_query_returns_all_in_order() {
    let cands = names(&["main", "feat-a", "feat-b"]);
    assert_eq!(filter_branches("", &cands), vec![0, 1, 2]);
}

#[test]
fn filter_substring_case_insensitive() {
    let cands = names(&["main", "Feature-X", "stash"]);
    // Mixed-case needle and mixed-case candidate both fold to lower.
    assert_eq!(filter_branches("FEAT", &cands), vec![1]);
    // Substring anywhere in the name matches.
    assert_eq!(filter_branches("ash", &cands), vec![2]);
}

#[test]
fn filter_no_match_returns_empty() {
    let cands = names(&["main", "release"]);
    assert!(filter_branches("zzz", &cands).is_empty());
}

#[test]
fn filter_trims_query() {
    let cands = names(&["main"]);
    assert_eq!(filter_branches("  main  ", &cands), vec![0]);
    // Whitespace-only query → treat as empty.
    assert_eq!(filter_branches("   ", &cands), vec![0]);
}

#[test]
fn rows_switch_empty_query_no_candidates() {
    let rows = build_rows(PickerMode::Switch, &[], None, "");
    assert!(rows.is_empty());
}

#[test]
fn rows_switch_lists_candidates_marking_current() {
    let cands = names(&["main", "feat-a"]);
    let rows = build_rows(PickerMode::Switch, &cands, Some("main"), "");
    assert_eq!(
        rows,
        vec![
            DisplayRow::Branch {
                name: "main".into(),
                is_current: true
            },
            DisplayRow::Branch {
                name: "feat-a".into(),
                is_current: false
            },
        ]
    );
}

#[test]
fn rows_switch_appends_create_when_no_exact_match() {
    let cands = names(&["main", "feat-a"]);
    let rows = build_rows(PickerMode::Switch, &cands, Some("main"), "new-thing");
    // Filter drops both candidates → only the Create row remains.
    assert_eq!(rows, vec![DisplayRow::CreateFromHead("new-thing".into())]);
}

#[test]
fn rows_switch_separator_before_create_when_branches_visible() {
    // "feat" matches "feat-a" → branch row + separator + Create row.
    let cands = names(&["main", "feat-a"]);
    let rows = build_rows(PickerMode::Switch, &cands, Some("main"), "feat");
    assert_eq!(
        rows,
        vec![
            DisplayRow::Branch {
                name: "feat-a".into(),
                is_current: false
            },
            DisplayRow::Separator,
            DisplayRow::CreateFromHead("feat".into()),
        ]
    );
}

#[test]
fn rows_switch_suppresses_create_on_exact_match() {
    let cands = names(&["main", "feat-a"]);
    let rows = build_rows(PickerMode::Switch, &cands, Some("main"), "feat-a");
    // Exact match → no Create row.
    assert_eq!(
        rows,
        vec![DisplayRow::Branch {
            name: "feat-a".into(),
            is_current: false
        }]
    );
}

#[test]
fn rows_switch_exact_match_is_case_insensitive() {
    // Filter folds case; exact-match check must too, or the user types
    // "FEAT-A" against existing "feat-a" and sees both the match row
    // AND a spurious "Create branch FEAT-A from HEAD" row (which would
    // collide on case-insensitive file systems if the user picked it).
    let cands = names(&["main", "feat-a"]);
    let rows = build_rows(PickerMode::Switch, &cands, Some("main"), "FEAT-A");
    assert_eq!(
        rows,
        vec![DisplayRow::Branch {
            name: "feat-a".into(),
            is_current: false
        }]
    );
}

#[test]
fn rows_baseref_starts_with_repo_default() {
    let cands = names(&["origin/main", "origin/release"]);
    let rows = build_rows(PickerMode::BaseRef, &cands, None, "");
    assert_eq!(
        rows,
        vec![
            DisplayRow::RepoDefault,
            DisplayRow::Separator,
            DisplayRow::Branch {
                name: "origin/main".into(),
                is_current: false
            },
            DisplayRow::Branch {
                name: "origin/release".into(),
                is_current: false
            },
        ]
    );
}

#[test]
fn rows_baseref_drops_separator_when_no_filtered_branches() {
    let cands = names(&["origin/main"]);
    // Query filters everything out → no separator after RepoDefault.
    let rows = build_rows(PickerMode::BaseRef, &cands, None, "zzz");
    assert_eq!(rows, vec![DisplayRow::RepoDefault]);
}

#[test]
fn rows_baseref_does_not_offer_create_from_head() {
    // Even with a non-matching query in BaseRef mode, no Create row.
    let cands = names(&["origin/main"]);
    let rows = build_rows(PickerMode::BaseRef, &cands, None, "new-base");
    assert_eq!(rows, vec![DisplayRow::RepoDefault]);
}

#[test]
fn selectable_count_skips_separators() {
    let rows = vec![
        DisplayRow::Branch {
            name: "a".into(),
            is_current: false,
        },
        DisplayRow::Separator,
        DisplayRow::Branch {
            name: "b".into(),
            is_current: false,
        },
        DisplayRow::CreateFromHead("x".into()),
    ];
    assert_eq!(selectable_count(&rows), 3);
}

#[test]
fn selectable_to_absolute_maps_through_separators() {
    let rows = vec![
        DisplayRow::RepoDefault,
        DisplayRow::Separator,
        DisplayRow::Branch {
            name: "a".into(),
            is_current: false,
        },
    ];
    assert_eq!(selectable_to_absolute(&rows, 0), Some(0));
    assert_eq!(selectable_to_absolute(&rows, 1), Some(2));
    assert_eq!(selectable_to_absolute(&rows, 2), None);
}

#[test]
fn wrap_selectable_cycles_in_both_directions() {
    assert_eq!(wrap_selectable(0, 1, 3), 1);
    assert_eq!(wrap_selectable(2, 1, 3), 0);
    assert_eq!(wrap_selectable(0, -1, 3), 2);
    assert_eq!(wrap_selectable(1, -1, 3), 0);
}

#[test]
fn wrap_selectable_zero_count_returns_zero() {
    // Defensive: no selectable rows shouldn't panic on rem_euclid by 0.
    assert_eq!(wrap_selectable(0, 1, 0), 0);
}

#[test]
fn display_row_outcome_translates_each_variant() {
    assert_eq!(
        DisplayRow::Branch {
            name: "main".into(),
            is_current: false
        }
        .outcome(),
        Some(PickerOutcome::Branch("main".into()))
    );
    assert_eq!(
        DisplayRow::CreateFromHead("feat".into()).outcome(),
        Some(PickerOutcome::CreateFromHead("feat".into()))
    );
    assert_eq!(
        DisplayRow::RepoDefault.outcome(),
        Some(PickerOutcome::UseRepoDefault)
    );
    assert_eq!(DisplayRow::Separator.outcome(), None);
}

#[test]
fn display_row_separator_is_not_selectable() {
    assert!(!DisplayRow::Separator.is_selectable());
    assert!(DisplayRow::RepoDefault.is_selectable());
    assert!(
        DisplayRow::Branch {
            name: "a".into(),
            is_current: false
        }
        .is_selectable()
    );
    assert!(DisplayRow::CreateFromHead("x".into()).is_selectable());
}

#[test]
fn build_rows_caps_branches_with_more_hint() {
    use oximux_app::shell::source_control::branch_picker::MAX_BRANCH_ROWS;
    let cands: Vec<String> = (0..20).map(|i| format!("feature-{i:02}")).collect();
    let rows = build_rows(PickerMode::Switch, &cands, None, "");
    let branch_rows = rows
        .iter()
        .filter(|r| matches!(r, DisplayRow::Branch { .. }))
        .count();
    assert_eq!(branch_rows, MAX_BRANCH_ROWS);
    assert!(rows.contains(&DisplayRow::MoreHint(20 - MAX_BRANCH_ROWS)));
    // The hint never participates in keyboard navigation.
    assert!(!DisplayRow::MoreHint(3).is_selectable());
    assert_eq!(DisplayRow::MoreHint(3).outcome(), None);
}

#[test]
fn build_rows_no_hint_at_or_under_cap() {
    use oximux_app::shell::source_control::branch_picker::MAX_BRANCH_ROWS;
    let cands: Vec<String> = (0..MAX_BRANCH_ROWS)
        .map(|i| format!("b-{i:02}"))
        .collect();
    let rows = build_rows(PickerMode::Switch, &cands, None, "");
    assert!(!rows.iter().any(|r| matches!(r, DisplayRow::MoreHint(_))));
}
