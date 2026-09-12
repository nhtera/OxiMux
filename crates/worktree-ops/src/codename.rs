//! Codenames: the name a workspace gets when the user does not type one.
//!
//! The create dialog no longer insists on a name, because naming the work
//! before knowing what it is was the step that made "one keystroke to a
//! worktree" impossible. An empty name gets a codename from the list below,
//! and the branch and directory are cut on it exactly as they would be on a
//! typed name.
//!
//! The list is deliberately boring. These words appear in branch names, in
//! pull-request titles and in `git worktree list` output of a public
//! repository, so they are short, neutral, natural-material nouns with no
//! theme and no joke in them. Anything whimsical becomes a support question.
//!
//! Recognising a codename later is derived, not stored: a slug is one this
//! module produced iff, with any `-N` uniquing suffix stripped, it is a member
//! of the list. That is checkable at any time, survives a database restore,
//! and cannot desynchronise from reality the way a boolean column can. The
//! cost is that a user who types a word from the list gets a workspace
//! indistinguishable from a generated one — accepted, and the reason the list
//! is boring.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;

use oximux_git::validate_slug;

/// The vocabulary. Every entry is lowercase ASCII letters only, so each passes
/// [`validate_slug`] and [`oximux_git::derive_slug`] returns it unchanged —
/// pinned by a test, because a codename that git refuses is a create that
/// fails after the dialog closed.
pub const CODENAMES: &[&str] = &[
    "amber", "aspen", "basalt", "birch", "brook", "canyon", "cedar", "cobalt",
    "coral", "cove", "delta", "dune", "ember", "fern", "flint", "granite",
    "harbor", "hazel", "heron", "indigo", "iris", "jade", "juniper", "kelp",
    "lagoon", "larch", "lichen", "linen", "maple", "marble", "meadow", "mesa",
    "moss", "oak", "ochre", "olive", "onyx", "opal", "orchid", "pebble",
    "pine", "plume", "quartz", "reed", "ridge", "river", "rowan", "sage",
    "sand", "sedge", "shale", "slate", "spruce", "summit", "tide", "topaz",
    "tundra", "umber", "valley", "velvet", "willow", "wren", "yarrow", "zinc",
];

/// Pick a codename not already in `existing`, uniquing with `-2`, `-3`, …
/// once the whole list is taken.
///
/// `existing` is the set of slugs the caller wants to avoid — every active
/// *and archived* workspace slug in the project, because an archived worktree
/// is still a directory on disk and still a branch. Random so two projects
/// created a minute apart do not both land on the list's first word; the
/// randomness is process-local and needs no dependency.
pub fn select_codename(existing: &[String]) -> String {
    select_codename_seeded(existing, random_seed())
}

/// The deterministic half of [`select_codename`], for tests that want to pin
/// the uniquing behaviour rather than the dice.
pub fn select_codename_seeded(existing: &[String], seed: u64) -> String {
    let n = CODENAMES.len();
    let start = (seed % n as u64) as usize;
    let taken = |s: &str| existing.iter().any(|e| e == s);
    // First pass: an unsuffixed word, starting at a random index and
    // wrapping, so the list is consumed evenly rather than front-first.
    if let Some(free) = (0..n)
        .map(|i| CODENAMES[(start + i) % n])
        .find(|c| !taken(c))
    {
        return free.to_string();
    }
    // Every word is in use: suffix the word the seed landed on. `-1` is never
    // produced — the bare word *is* the first — so the sequence reads
    // `amber`, `amber-2`, `amber-3`.
    let base = CODENAMES[start];
    (2u32..)
        .map(|k| format!("{base}-{k}"))
        .find(|c| !taken(c))
        .expect("an unbounded suffix sequence always finds a free name")
}

/// True iff `slug` is one this module generated: a codename, optionally with
/// a `-N` uniquing suffix.
///
/// A user-typed name that happens to be a codename is indistinguishable and
/// is reported as generated — accepted, see the module doc. A suffix is only
/// the `-N` shape [`select_codename`] mints: `amber-2` is generated,
/// `amber-fix` is not, and a bare number is not a codename at all.
pub fn is_generated_codename(slug: &str) -> bool {
    let base = strip_uniquing_suffix(slug);
    CODENAMES.contains(&base)
}

/// `amber-2` → `amber`; `amber` → `amber`; `amber-fix` → `amber-fix`.
fn strip_uniquing_suffix(slug: &str) -> &str {
    match slug.rsplit_once('-') {
        Some((base, suffix))
            if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => slug,
    }
}

/// A per-process random `u64` with no dependency: `RandomState` is seeded
/// from the OS once per process and per instance, which is all a starting
/// index in a 64-word list needs.
fn random_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    RandomState::new().hash_one(nanos)
}

/// Every codename is a slug git accepts and `derive_slug` leaves alone.
/// Public so a host can assert it too; the crate's own test does.
pub fn every_codename_is_a_valid_slug() -> Result<(), String> {
    for name in CODENAMES {
        validate_slug(name).map_err(|e| format!("{name:?}: {e}"))?;
        let derived = oximux_git::derive_slug(name);
        if derived != *name {
            return Err(format!("{name:?} derives to {derived:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_list_is_boring_valid_and_free_of_duplicates() {
        every_codename_is_a_valid_slug().unwrap();
        let unique: HashSet<&str> = CODENAMES.iter().copied().collect();
        assert_eq!(unique.len(), CODENAMES.len(), "duplicate codename");
        for name in CODENAMES {
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase()),
                "{name:?} is not plain lowercase letters"
            );
            assert!(name.len() <= 8, "{name:?} is longer than a codename should be");
        }
    }

    #[test]
    fn a_free_word_is_returned_bare() {
        let picked = select_codename_seeded(&[], 0);
        assert_eq!(picked, CODENAMES[0]);
        assert!(is_generated_codename(&picked));
    }

    #[test]
    fn a_taken_word_is_skipped_for_the_next_free_one() {
        let existing = vec![CODENAMES[0].to_string(), CODENAMES[1].to_string()];
        assert_eq!(select_codename_seeded(&existing, 0), CODENAMES[2]);
    }

    #[test]
    fn the_seed_wraps_around_the_list() {
        let n = CODENAMES.len() as u64;
        assert_eq!(select_codename_seeded(&[], n - 1), CODENAMES[CODENAMES.len() - 1]);
        assert_eq!(select_codename_seeded(&[], n), CODENAMES[0]);
        // Landing on the last word and finding it taken wraps to the first.
        let existing = vec![CODENAMES[CODENAMES.len() - 1].to_string()];
        assert_eq!(select_codename_seeded(&existing, n - 1), CODENAMES[0]);
    }

    #[test]
    fn a_full_list_uniques_with_a_numeric_suffix_starting_at_two() {
        let mut existing: Vec<String> = CODENAMES.iter().map(|s| s.to_string()).collect();
        let first = select_codename_seeded(&existing, 3);
        assert_eq!(first, format!("{}-2", CODENAMES[3]));
        assert!(is_generated_codename(&first));
        existing.push(first);
        assert_eq!(select_codename_seeded(&existing, 3), format!("{}-3", CODENAMES[3]));
    }

    #[test]
    fn the_random_pick_is_always_a_codename() {
        for _ in 0..16 {
            assert!(is_generated_codename(&select_codename(&[])));
        }
    }

    #[test]
    fn recognises_bare_and_suffixed_codenames() {
        assert!(is_generated_codename("amber"));
        assert!(is_generated_codename("amber-2"));
        assert!(is_generated_codename("amber-17"));
    }

    #[test]
    fn a_user_name_is_not_a_codename() {
        assert!(!is_generated_codename("fix-login"));
        assert!(!is_generated_codename("amber-fix"));
        assert!(!is_generated_codename("ambers"));
        assert!(!is_generated_codename("2"));
        assert!(!is_generated_codename(""));
        assert!(!is_generated_codename("-2"));
    }

    /// Documented and accepted: a typed name that IS a codename cannot be
    /// told apart from a generated one. The list being boring is the
    /// mitigation, not a flag.
    #[test]
    fn a_user_name_that_coincides_with_a_codename_reads_as_generated() {
        assert!(is_generated_codename("willow"));
    }
}
