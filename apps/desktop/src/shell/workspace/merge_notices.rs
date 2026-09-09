//! The pointer to a stash a merge left on the stack.
//!
//! When `merge_branch` auto-stashes a dirty project root and then either
//! conflicts or fails to pop, the user's uncommitted work is sitting in the
//! stash stack and **nothing else in the app says so**. A toast is not good
//! enough: a dismissed toast loses the only pointer to that work, which makes
//! any code path where `auto_stash` is `Some` and it reaches only a toast a
//! release-blocking bug rather than a polish item.
//!
//! So the pointer is persisted here until the user acts on it.
//!
//! **Two deliberate choices:**
//!
//! 1. **Not on the workspace row.** The row's only free-text column is
//!    `comment`, and `WorktreeProgressWire` rides the *coordination* read gate
//!    precisely because it carries "only an id and agent-authored text, never
//!    the host paths and branch names". A repository path and a branch name in
//!    `comment` would push exactly that across a gate designed to withhold it.
//!    This store is the desktop's own settings DB, which no remote peer reads.
//!
//! 2. **The stash is remembered by MESSAGE, never by index.** `StashRef.index`
//!    "is only stable while no other process mutates the stash stack", and a
//!    recovery notice is precisely a long suspension: the user resolves
//!    conflicts for an hour and stashes twice in their own terminal, and
//!    `stash@{0}` now names something else. A stored `stash@{N}` that has since
//!    renumbered is *worse* than no pointer, because it pops the wrong entry
//!    confidently. [`resolve`] re-reads `stash_list` at the moment of recovery
//!    and matches on the message.

use chrono::Utc;
use oximux_git::Repository;
use oximux_storage::SettingsRepo;
use serde::{Deserialize, Serialize};

/// Settings key holding the whole notice list as one JSON array. One key rather
/// than one per notice: the list is read whole on every project activation and
/// is never more than a handful of entries.
const NOTICES_KEY: &str = "merge_stash_notices";

/// Upper bound on retained notices, oldest dropped first. A notice the user has
/// ignored through this many stranded merges is not going to be acted on, and
/// an unbounded list would grow forever in a settings blob nobody prunes.
const MAX_NOTICES: usize = 16;

/// Why the stash is still on the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeReason {
    /// The merge conflicted, so the stash was deliberately not popped — the
    /// user has to resolve the conflict before their own edits can go back on
    /// top of it.
    Conflicted,
    /// The merge succeeded and the pop then failed, because the stashed edit no
    /// longer applies to the merged result.
    PopFailed,
    /// The merge itself failed, and putting the stash back afterwards failed
    /// too. Rare — it is the exception `crates/git/src/merge.rs`'s recovery
    /// contract names — and the only case where nothing landed AND the user's
    /// edits are missing, so it must not borrow either of the other messages.
    MergeFailed,
}

/// A stash a merge left behind, and enough context to find it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StashNotice {
    pub id: String,
    pub project_id: String,
    /// The repository the stash lives in — always a project root, because that
    /// is the only repository a merge mutates.
    pub project_root: String,
    /// The branch that was being landed, for the human reading the notice.
    pub branch: String,
    /// `git stash push -m` text, which is what [`resolve`] matches on.
    pub stash_message: String,
    pub created_at: String,
    pub reason: NoticeReason,
}

impl StashNotice {
    pub fn new(
        project_id: &str,
        project_root: &str,
        branch: &str,
        stash_message: &str,
        reason: NoticeReason,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            project_id: project_id.to_string(),
            project_root: project_root.to_string(),
            branch: branch.to_string(),
            stash_message: stash_message.to_string(),
            created_at: Utc::now().to_rfc3339(),
            reason,
        }
    }

    /// Dialog body. Says which half went wrong, because "the merge failed" and
    /// "only the pop failed" call for completely different next actions.
    pub fn message(&self) -> String {
        match self.reason {
            NoticeReason::Conflicted => format!(
                "Merging \u{201c}{}\u{201d} hit conflicts, so your uncommitted changes were set \
                 aside first and have not been put back. Resolve the conflicts, then restore \
                 them.",
                self.branch
            ),
            NoticeReason::PopFailed => format!(
                "\u{201c}{}\u{201d} merged successfully, but your uncommitted changes could not be \
                 put back on top of the result. They are safe \u{2014} they are still stashed. \
                 Git left conflict markers in the project folder while trying; resolve those \
                 first, or restoring will fail again.",
                self.branch
            ),
            NoticeReason::MergeFailed => format!(
                "Merging \u{201c}{}\u{201d} failed, and your uncommitted changes could not be put \
                 back afterwards. Nothing was merged \u{2014} but the changes are safe, they are \
                 still stashed.",
                self.branch
            ),
        }
    }
}

/// Every notice, oldest first. A corrupt or absent blob reads as empty rather
/// than erroring: a settings blob that will not parse must not be able to stop
/// the app from merging.
pub fn load(repo: &SettingsRepo) -> Vec<StashNotice> {
    let Ok(Some(raw)) = repo.get(NOTICES_KEY) else {
        return Vec::new();
    };
    match serde_json::from_str(&raw) {
        Ok(list) => list,
        Err(err) => {
            tracing::warn!(?err, "merge stash notices blob is unreadable; starting empty");
            Vec::new()
        }
    }
}

fn save(repo: &SettingsRepo, notices: &[StashNotice]) {
    match serde_json::to_string(notices) {
        Ok(json) => {
            if let Err(err) = repo.set(NOTICES_KEY, &json) {
                tracing::error!(?err, "could not persist the merge stash notice");
            }
        }
        Err(err) => tracing::error!(?err, "could not serialize merge stash notices"),
    }
}

/// Persist one notice. Returns the stored list.
pub fn record(repo: &SettingsRepo, notice: StashNotice) -> Vec<StashNotice> {
    let mut notices = load(repo);
    notices.push(notice);
    if notices.len() > MAX_NOTICES {
        let excess = notices.len() - MAX_NOTICES;
        notices.drain(0..excess);
    }
    save(repo, &notices);
    notices
}

/// Drop one notice — the user restored the stash, or said they were done with
/// it. Idempotent: acknowledging an id that is not there is not an error.
pub fn acknowledge(repo: &SettingsRepo, id: &str) {
    let mut notices = load(repo);
    let before = notices.len();
    notices.retain(|n| n.id != id);
    if notices.len() != before {
        save(repo, &notices);
    }
}

/// Notices belonging to one project, oldest first.
pub fn for_project(notices: &[StashNotice], project_id: &str) -> Vec<StashNotice> {
    notices
        .iter()
        .filter(|n| n.project_id == project_id)
        .cloned()
        .collect()
}

/// What `git stash list` says about a remembered stash, right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StashLookup {
    /// Exactly one entry carries this message. Safe to pop.
    Found(oximux_core::StashRef),
    /// Nothing carries it any more — the user already popped or dropped it.
    Gone,
    /// Several entries carry it, so no index can be chosen without guessing.
    /// Two stranded merges in the same repository produce this, because
    /// `merge_branch` writes a constant auto-stash message.
    Ambiguous(usize),
    /// The stash list could not be read.
    Failed(String),
}

/// Re-resolve a notice against the live stash stack.
///
/// This is the whole reason the message is what gets stored. Never render a
/// remembered `stash@{N}`: by the time a notice is acted on it may name a
/// different entry, and popping the wrong one confidently is the worse failure.
pub async fn resolve(repo: &Repository, notice: &StashNotice) -> StashLookup {
    let entries = match repo.stash_list().await {
        Ok(entries) => entries,
        Err(err) => return StashLookup::Failed(err.to_string()),
    };
    let matches: Vec<_> = entries
        .iter()
        .filter(|e| e.message == notice.stash_message)
        .collect();
    match matches.len() {
        0 => StashLookup::Gone,
        1 => StashLookup::Found(matches[0].stash_ref.clone()),
        n => StashLookup::Ambiguous(n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::open_memory;

    fn store() -> SettingsRepo {
        SettingsRepo::new(open_memory().expect("open memory"))
    }

    fn notice(project: &str, branch: &str) -> StashNotice {
        StashNotice::new(
            project,
            "/repos/app",
            branch,
            "oximux: auto-stash before merge",
            NoticeReason::Conflicted,
        )
    }

    /// The guarantee the whole module exists for: the pointer outlives the
    /// toast that first showed it.
    #[test]
    fn a_recorded_notice_is_still_there_on_the_next_read() {
        let repo = store();
        let n = notice("p1", "oximux/feat");
        record(&repo, n.clone());
        assert_eq!(load(&repo), vec![n]);
    }

    #[test]
    fn acknowledging_removes_only_that_notice() {
        let repo = store();
        let a = notice("p1", "oximux/a");
        let b = notice("p1", "oximux/b");
        record(&repo, a.clone());
        record(&repo, b.clone());

        acknowledge(&repo, &a.id);
        assert_eq!(load(&repo), vec![b]);
        // Idempotent — acknowledging twice is not an error.
        acknowledge(&repo, &a.id);
    }

    #[test]
    fn notices_are_scoped_to_their_project() {
        let repo = store();
        let mine = notice("p1", "oximux/a");
        record(&repo, mine.clone());
        record(&repo, notice("p2", "oximux/b"));

        assert_eq!(for_project(&load(&repo), "p1"), vec![mine]);
    }

    /// A blob that will not parse must not be able to stop the app from
    /// merging — the worst it may do is forget.
    #[test]
    fn an_unreadable_blob_reads_as_empty_rather_than_failing() {
        let repo = store();
        repo.set(NOTICES_KEY, "{not json").expect("write garbage");
        assert!(load(&repo).is_empty());
        // ...and recording over it still works.
        let n = notice("p1", "oximux/a");
        record(&repo, n.clone());
        assert_eq!(load(&repo), vec![n]);
    }

    #[test]
    fn the_list_is_capped_dropping_the_oldest() {
        let repo = store();
        for i in 0..MAX_NOTICES + 3 {
            record(&repo, notice("p1", &format!("oximux/b{i}")));
        }
        let stored = load(&repo);
        assert_eq!(stored.len(), MAX_NOTICES);
        assert_eq!(stored[0].branch, "oximux/b3", "oldest dropped first");
    }

    /// Both reasons must produce distinct copy: a conflicted merge and a failed
    /// pop call for different next actions, and only one of them means the
    /// merge succeeded.
    #[test]
    fn a_failed_pop_says_the_merge_succeeded_and_a_conflict_does_not() {
        let mut popped = notice("p1", "oximux/feat");
        popped.reason = NoticeReason::PopFailed;
        let msg = popped.message();
        assert!(msg.contains("merged successfully"), "{msg}");

        let conflicted = notice("p1", "oximux/feat").message();
        assert!(conflicted.contains("conflicts"), "{conflicted}");
        assert!(
            !conflicted.contains("merged successfully"),
            "a conflicted merge did not succeed: {conflicted}"
        );
    }

    /// The rarest reason, and the only one where NOTHING landed. It must not
    /// borrow either other message: "merged successfully" would be false, and
    /// "resolve the conflicts" would send the user looking for conflicts that
    /// do not exist.
    #[test]
    fn a_failed_merge_says_nothing_was_merged_and_the_changes_are_still_safe() {
        let mut n = notice("p1", "oximux/feat");
        n.reason = NoticeReason::MergeFailed;
        let msg = n.message();
        assert!(msg.contains("Nothing was merged"), "{msg}");
        assert!(msg.contains("still stashed"), "{msg}");
        assert!(!msg.contains("merged successfully"), "{msg}");
        assert!(!msg.contains("Resolve the conflicts"), "{msg}");
    }

    /// Every reason must produce its own copy. A reason that reuses another's
    /// message is a reason the user cannot act on.
    #[test]
    fn no_two_reasons_share_a_message() {
        let mut seen = std::collections::HashSet::new();
        for reason in [
            NoticeReason::Conflicted,
            NoticeReason::PopFailed,
            NoticeReason::MergeFailed,
        ] {
            let mut n = notice("p1", "oximux/feat");
            n.reason = reason;
            assert!(seen.insert(n.message()), "duplicate copy for {reason:?}");
        }
    }
}
