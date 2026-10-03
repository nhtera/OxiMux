use crate::shell::search_palette::rank::*;

fn one(query: &str, text: &str) -> Option<Scored> {
    score_fields(&prepare(query), &[Field { text, role: Role::Primary }])
}

fn quality(query: &str, text: &str) -> Option<Quality> {
    one(query, text).map(|s| s.rank.worst)
}

#[test]
fn prepare_folds_dedupes_and_caps() {
    let p = prepare("  Main  main MAIN ");
    assert_eq!(p.tokens.len(), 1);
    assert_eq!(p.tokens[0], vec!['m', 'a', 'i', 'n']);
    let many = (0..40).map(|i| format!("t{i}")).collect::<Vec<_>>().join(" ");
    assert_eq!(prepare(&many).tokens.len(), 16);
    assert!(prepare("   ").is_empty());
}

#[test]
fn quality_ladder() {
    assert_eq!(quality("main", "main"), Some(Quality::FieldExact));
    assert_eq!(quality("main", "fix main branch"), Some(Quality::WordExact));
    assert_eq!(quality("main", "maintenance"), Some(Quality::FieldPrefix));
    assert_eq!(quality("sear", "feat/searchPalette"), Some(Quality::WordPrefix));
    assert_eq!(quality("search-pal", "feat/search-palette"), Some(Quality::BoundarySubstring));
    assert_eq!(quality("arch", "feat/search"), Some(Quality::Substring));
    assert_eq!(quality("featsearch", "feat/search"), Some(Quality::Compact));
    assert_eq!(quality("fsrch", "feat/search"), Some(Quality::Subsequence));
    assert_eq!(quality("xyz", "feat/search"), None);
}

#[test]
fn camel_case_words_are_boundaries() {
    assert_eq!(quality("palette", "searchPalette"), Some(Quality::WordExact));
}

#[test]
fn exact_beats_prefix() {
    let exact = one("main", "main").unwrap().rank;
    let prefix = one("main", "maintenance").unwrap().rank;
    assert!(exact > prefix);
}

#[test]
fn single_letter_matches_only_exact_or_prefix() {
    assert_eq!(quality("m", "main"), Some(Quality::FieldPrefix));
    assert_eq!(quality("m", "fix main"), Some(Quality::WordPrefix));
    assert_eq!(quality("a", "main"), None);
}

#[test]
fn every_token_must_match_some_field() {
    let fields = [
        Field { text: "main", role: Role::Primary },
        Field { text: "OxiMux", role: Role::Container },
    ];
    assert!(score_fields(&prepare("ox ma"), &fields).is_some());
    assert!(score_fields(&prepare("ox zzz"), &fields).is_none());
}

#[test]
fn container_only_tokens_rank_below_primary_hits() {
    let p = prepare("oxi");
    let in_title = score_fields(&p, &[
        Field { text: "oxi notes", role: Role::Primary },
        Field { text: "Other", role: Role::Container },
    ])
    .unwrap();
    let in_project = score_fields(&p, &[
        Field { text: "notes", role: Role::Primary },
        Field { text: "oxi", role: Role::Container },
    ])
    .unwrap();
    // Same quality tier is not required — a primary hit must still win.
    assert!(in_title.rank.primary_hits > in_project.rank.primary_hits);
    assert_eq!(in_project.rank.container_only, 1);
}

#[test]
fn shorter_title_wins_on_coverage() {
    let short = one("api", "api").unwrap().rank;
    let long = one("api", "api gateway service").unwrap().rank;
    assert!(short > long);
}

#[test]
fn highlight_ranges_are_byte_offsets_into_original_text() {
    let s = one("PAL", "Search Palette").unwrap();
    assert_eq!(s.ranges[0], vec![7..10]);
    // Non-ASCII before the hit shifts byte offsets, not char offsets.
    let s = one("tab", "é tab").unwrap();
    assert_eq!(&"é tab"[s.ranges[0][0].clone()], "tab");
}

#[test]
fn simple_scorer_ignores_filler_and_needs_two_chars() {
    let p = prepare("open keys");
    assert_eq!(score_simple(&p, "Keybindings keys shortcuts"), Some(3));
    assert_eq!(score_simple(&prepare("k"), "Keybindings"), None);
    assert_eq!(score_simple(&prepare("open the"), "Open Settings"), None);
    // Half or fewer tokens covered → no match.
    assert_eq!(score_simple(&prepare("theme zzz"), "Appearance theme"), None);
    assert_eq!(score_simple(&prepare("app theme zzz"), "Appearance theme"), Some(5));
}
