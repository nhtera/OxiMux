//! Codenames from the desktop: the host half of
//! `oximux-worktree-ops::codename`.
//!
//! The crate owns the vocabulary and the uniquing rule; this owns the one
//! thing the crate refuses to guess at — which slugs already exist. Two
//! callers need that answer: the create dialog, which picks the codename an
//! empty Name will submit, and the chat's fresh-worktree request, whose
//! codename was picked by a leaf that has no repository to consult.
//!
//! Lifted out of `workspace_ops.rs`, which sits at the 3000-LOC hard cap
//! `xtask file-size-lint` enforces in CI.

use oximux_core::Project;
use oximux_storage::WorkspaceRepo;
use oximux_worktree_ops::{is_generated_codename, select_codename};

/// Every slug already in use across `projects`, active **and archived**, so
/// a codename can be picked that collides with nothing. Archived rows count:
/// an archived worktree is still a directory on disk and still a branch. A
/// project whose rows cannot be listed contributes nothing — the create path
/// re-checks for a duplicate slug before it touches git anyway.
///
/// Wider than it strictly needs to be (slugs are unique per project, this
/// avoids them across every open project) — a codename taken in one project
/// is simply skipped in another, which costs nothing from a 64-word list.
pub(crate) fn existing_slugs_across(repo: &WorkspaceRepo, projects: &[Project]) -> Vec<String> {
    let mut slugs = Vec::new();
    for project in projects {
        for rows in [
            repo.list_for_project(&project.id),
            repo.list_archived_for_project(&project.id),
        ] {
            match rows {
                Ok(rows) => slugs.extend(rows.into_iter().map(|w| w.slug)),
                Err(err) => {
                    tracing::debug!(?err, project_id = %project.id, "existing_slugs: list failed")
                }
            }
        }
    }
    slugs
}

/// A slug that will not collide with a row, for a create request whose slug
/// the **chat leaf** picked.
///
/// The *New Agent* draft's fresh-worktree toggle picks its codename with no
/// existing-slug list — the leaf owns no repository, by design — so the pick
/// can land on a word already in use. This is the seam that does own the
/// repository, so the collision is resolved here, against every open
/// project's active and archived slugs, rather than left for git's failure
/// path: a collision between two application-generated values is expected
/// bookkeeping, not an error to show. A slug the user typed is returned as
/// is — a typed collision is theirs to see and change — and so is a codename
/// that is free. The outcome carries the real branch back to the chat, which
/// relabels from it, so the rare re-pick is visible where it matters.
pub(crate) fn dedup_codename_slug(
    slug: String,
    repo: &WorkspaceRepo,
    projects: &[Project],
) -> String {
    if !is_generated_codename(&slug) {
        return slug;
    }
    let existing = existing_slugs_across(repo, projects);
    if !existing.contains(&slug) {
        return slug;
    }
    let fresh = select_codename(&existing);
    tracing::info!(from = %slug, to = %fresh, "codename already in use; re-picked");
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::{ProjectRepo, open_memory};

    /// A leaf-picked codename that is already a row (active OR archived, in
    /// ANY open project) is re-picked; a free codename and a typed slug pass
    /// through untouched.
    #[test]
    fn a_taken_codename_is_repicked_and_everything_else_passes_through() {
        let db = open_memory().expect("memory db");
        let projects = ProjectRepo::new(db.clone());
        let a = projects.insert("A", "/tmp/a", "main").expect("project a");
        let b = projects.insert("B", "/tmp/b", "main").expect("project b");
        let repo = WorkspaceRepo::new(db);
        // `amber` lives in project A (active); `birch` in project B, archived.
        repo.insert(&a.id, "amber", "amber", "oximux/amber", "/wt/amber", true).expect("row");
        let archived = repo
            .insert(&b.id, "birch", "birch", "oximux/birch", "/wt/birch", true)
            .expect("row");
        repo.mark_archived(&archived.id).expect("archive");
        let open = vec![a, b];

        let fresh = dedup_codename_slug("amber".into(), &repo, &open);
        assert_ne!(fresh, "amber");
        assert!(is_generated_codename(&fresh));
        // Archived rows count: the directory and branch still exist.
        assert_ne!(dedup_codename_slug("birch".into(), &repo, &open), "birch");
        // Free codename and typed slug: untouched.
        assert_eq!(dedup_codename_slug("cedar".into(), &repo, &open), "cedar");
        assert_eq!(dedup_codename_slug("fix-login".into(), &repo, &open), "fix-login");
        // A typed slug that collides is left for the user to see.
        repo.insert(&open[0].id, "fix-login", "fix-login", "oximux/fix-login", "/wt/fl", true)
            .expect("row");
        assert_eq!(dedup_codename_slug("fix-login".into(), &repo, &open), "fix-login");
    }

    /// The slug list is the union over projects, active and archived.
    #[test]
    fn existing_slugs_span_projects_and_include_archived_rows() {
        let db = open_memory().expect("memory db");
        let projects = ProjectRepo::new(db.clone());
        let a = projects.insert("A", "/tmp/a", "main").expect("project a");
        let b = projects.insert("B", "/tmp/b", "main").expect("project b");
        let repo = WorkspaceRepo::new(db);
        repo.insert(&a.id, "amber", "amber", "oximux/amber", "/wt/amber", true).expect("row");
        let gone = repo
            .insert(&b.id, "birch", "birch", "oximux/birch", "/wt/birch", true)
            .expect("row");
        repo.mark_archived(&gone.id).expect("archive");
        let mut slugs = existing_slugs_across(&repo, &[a, b]);
        slugs.sort();
        assert_eq!(slugs, vec!["amber".to_string(), "birch".to_string()]);
    }
}
