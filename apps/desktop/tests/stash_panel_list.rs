//! Pure-unit tests for the stash row formatters.

use oximux_app::shell::stash_panel::list_render::{
    absolute_date_in, row_message, row_meta, row_tooltip,
};
use oximux_core::{StashEntry, StashRef};

/// 2026-08-03 14:30:06 UTC — the fixed instant every date assertion below
/// reads, so none of them depends on when the suite runs.
const AUG_3_2026: i64 = 1_785_767_406;

fn entry(index: usize, branch: &str, message: &str) -> StashEntry {
    StashEntry {
        stash_ref: StashRef { index },
        branch: branch.to_string(),
        message: message.to_string(),
        sha: format!("{:040x}", index + 1),
        created_at: AUG_3_2026,
        relative: "2 hours ago".to_string(),
    }
}

/// UTC, spelled as a fixed offset: the formatter takes a timezone precisely so
/// a test never has to ask the machine what its clock is set to.
fn utc() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(0).unwrap()
}

// --- row_message -----------------------------------------------------------

#[test]
fn the_row_shows_the_message_and_not_the_address() {
    let e = entry(0, "main", "feat(api): hello");
    assert_eq!(row_message(&e), "feat(api): hello");
}

#[test]
fn empty_message_becomes_no_message_placeholder() {
    assert_eq!(row_message(&entry(0, "main", "")), "(no message)");
}

#[test]
fn whitespace_only_message_becomes_no_message_placeholder() {
    assert_eq!(row_message(&entry(0, "main", "   \n\t  ")), "(no message)");
}

#[test]
fn message_with_surrounding_whitespace_is_trimmed() {
    assert_eq!(row_message(&entry(0, "main", "  WIP  ")), "WIP");
}

// --- row_meta --------------------------------------------------------------

#[test]
fn meta_joins_branch_and_relative_age() {
    let e = entry(0, "feat/windows", "temp");
    assert_eq!(row_meta(&e), "feat/windows · 2 hours ago");
}

/// A `git stash store` entry carries no `On <branch>:` prefix at all
/// (verified), so a blank branch is normal data. It must not leave a dangling
/// separator that reads as a parse bug.
#[test]
fn a_branchless_stash_shows_only_its_age() {
    let e = entry(0, "", "renamed msg");
    assert_eq!(row_meta(&e), "2 hours ago");
}

#[test]
fn a_stash_with_no_age_shows_only_its_branch() {
    let mut e = entry(0, "main", "temp");
    e.relative = String::new();
    assert_eq!(row_meta(&e), "main");
}

#[test]
fn meta_is_empty_when_git_gave_us_neither_field() {
    let mut e = entry(0, "", "temp");
    e.relative = String::new();
    assert_eq!(row_meta(&e), "");
}

// --- row_tooltip -----------------------------------------------------------

#[test]
fn the_tooltip_carries_the_address_the_row_drops() {
    let e = entry(3, "feat/windows", "temp");
    let tip = row_tooltip(&e, Some(2));
    assert!(tip.starts_with("temp\n"), "message leads the tooltip: {tip}");
    assert!(tip.contains("stash@{3}"), "address is in: {tip}");
    assert!(tip.contains("2026"), "absolute date is in: {tip}");
    assert!(tip.contains("2 files"), "count is in: {tip}");
}

#[test]
fn one_file_is_not_pluralised() {
    assert!(row_tooltip(&entry(0, "main", "temp"), Some(1)).contains("1 file"));
}

/// The count is `Option` because Phase 5 fetches the file list lazily. An
/// unexpanded row must say nothing rather than claim zero files.
#[test]
fn an_unfetched_count_is_omitted_entirely() {
    let tip = row_tooltip(&entry(0, "main", "temp"), None);
    assert!(!tip.ends_with("files"), "no count claimed: {tip}");
    assert!(
        tip.ends_with(|c: char| c.is_ascii_digit()),
        "ends on the date: {tip}"
    );
}

#[test]
fn a_branchless_stash_leaves_no_dangling_separator_in_the_tooltip() {
    let tip = row_tooltip(&entry(0, "", "renamed msg"), None);
    // The date itself is asserted against a fixed offset below, not here:
    // `row_tooltip` formats in the viewer's own timezone, and no single
    // instant lands on one calendar day across a 26-hour span of offsets.
    assert!(!tip.contains("· ·"), "no empty field: {tip}");
    assert!(tip.contains("stash@{0} · "), "address then date: {tip}");
    assert!(tip.contains("2026"), "{tip}");
}

#[test]
fn an_empty_message_is_placeheld_in_the_tooltip_too() {
    assert!(row_tooltip(&entry(0, "main", ""), None).starts_with("(no message)\n"));
}

/// Phase 5 produces exactly this pair for a `git stash store` entry: a count
/// to show and no branch to show it beside.
#[test]
fn a_counted_branchless_stash_reads_cleanly() {
    let tip = row_tooltip(&entry(0, "", "renamed msg"), Some(3));
    assert!(!tip.contains("· ·"), "no empty field: {tip}");
    assert!(tip.ends_with("3 files"), "{tip}");
    assert!(tip.contains("stash@{0} · "), "{tip}");
}

/// GPUI does not wrap a tooltip and the popup has no max width, so an
/// uncapped message would paint a tooltip wider than the window.
#[test]
fn a_very_long_message_is_clipped_in_the_tooltip() {
    let long = "x".repeat(400);
    let tip = row_tooltip(&entry(0, "main", &long), None);
    let first = tip.lines().next().unwrap();
    assert_eq!(first.chars().count(), 121, "120 chars plus the ellipsis");
    assert!(first.ends_with('…'), "{first}");
}

/// A message that fits is shown whole — the cap must not nibble the common
/// case, and it counts characters rather than bytes.
#[test]
fn a_message_that_fits_keeps_every_character() {
    let msg = "refactor the pane-group cwd resolution — ährlich, naïve, 日本語";
    let tip = row_tooltip(&entry(0, "main", msg), None);
    assert_eq!(tip.lines().next().unwrap(), msg);
}

// --- absolute_date ---------------------------------------------------------

#[test]
fn the_absolute_date_is_day_month_year_without_padding() {
    assert_eq!(absolute_date_in(AUG_3_2026, &utc()), "3 Aug 2026");
}

/// Outside chrono's representable range the date is simply absent, and
/// `row_tooltip` drops the empty field rather than printing `· ·`. A stash's
/// `created_at` is `%ct` off a real commit, so this needs corrupt repo data to
/// reach — the point is that it degrades quietly instead of panicking.
#[test]
fn an_unrepresentable_timestamp_yields_no_date_rather_than_a_panic() {
    assert_eq!(absolute_date_in(i64::MAX, &utc()), "");
    assert_eq!(absolute_date_in(i64::MIN, &utc()), "");
    let mut e = entry(0, "main", "temp");
    e.created_at = i64::MAX;
    assert_eq!(row_tooltip(&e, None), "temp\nstash@{0} · main");
}

#[test]
fn the_absolute_date_follows_the_viewers_timezone() {
    // 14:30 UTC is still 3 Aug in Saigon (+07:00) but already 4 Aug in
    // Auckland (+12:00) — the reason this takes a timezone at all.
    let saigon = chrono::FixedOffset::east_opt(7 * 3600).unwrap();
    let auckland = chrono::FixedOffset::east_opt(12 * 3600).unwrap();
    assert_eq!(absolute_date_in(AUG_3_2026, &saigon), "3 Aug 2026");
    assert_eq!(absolute_date_in(AUG_3_2026, &auckland), "4 Aug 2026");
}
