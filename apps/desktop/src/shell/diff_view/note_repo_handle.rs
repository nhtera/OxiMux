//! Process-wide handle to the persisted review-note repository.
//!
//! A diff view is constructed deep inside the pane/sidebar tree, far from the
//! single `Db` the app opens at boot. Rather than thread a `DiffReviewNoteRepo`
//! through every intermediate constructor (`ProjectPanes` → `PaneGroup` →
//! tab, and the sidebar tree), the repo handle is installed once at hydrate
//! and read on demand here. This is sound because there is exactly one SQLite
//! database per process — the handle is a genuine singleton, not shared
//! mutable state (`DiffReviewNoteRepo` is a cheap `Clone` newtype over the
//! pooled `Db`).
//!
//! Diff views that boot before the handle is installed (pure unit tests that
//! never call `hydrate`) read `None` and degrade to in-memory-only notes.

use std::sync::OnceLock;

use oximux_storage::DiffReviewNoteRepo;

static NOTE_REPO: OnceLock<DiffReviewNoteRepo> = OnceLock::new();

/// Install the review-note repo. Called once from `state::hydrate`. Later
/// calls are ignored (the first install wins) so repeated test hydrations
/// don't swap a live handle out from under an open diff view.
pub fn init_note_repo(repo: DiffReviewNoteRepo) {
    let _ = NOTE_REPO.set(repo);
}

/// The installed review-note repo, or `None` before hydrate. Returns a clone
/// so callers can move it into a closure without borrowing the static.
pub fn note_repo() -> Option<DiffReviewNoteRepo> {
    NOTE_REPO.get().cloned()
}
