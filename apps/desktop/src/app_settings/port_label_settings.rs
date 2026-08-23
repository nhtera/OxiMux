//! Persistence for the names a user gives a detected port.
//!
//! A port number is not a name. Three `node` processes on 3000, 3001 and 9229
//! are the API, the docs site and a debugger, and only the person who started
//! them knows which. So the label is theirs to write, and it has to outlive
//! the process it was written against — the whole point is to still be there
//! tomorrow when the same server comes back on the same port.
//!
//! Keyed by **project and port**, not by pid: a pid is gone by the next
//! restart, and a bare port would put the API's label on an unrelated
//! project's 3000.
//!
//! Stored as decoded rows in the global `SettingsRepo` key/value store, the
//! same shape `left_rail_layout` and `scm_layout_settings` use. A storage
//! error is logged and swallowed: a label is a convenience, and losing one
//! must not be able to take down a panel.

use std::collections::HashMap;
use std::path::Path;

use oximux_storage::SettingsRepo;

/// Key prefix for every port label. Listing by prefix is how the panel loads
/// them all in one read instead of one read per visible row.
pub const KEY_PREFIX: &str = "ports.label.";

/// Longest label kept. Long enough for "Storefront (staging data)", short
/// enough that a paste accident cannot fill the store with a log file.
pub const MAX_LABEL_LEN: usize = 64;

/// A label's settings key.
///
/// The port leads because it is the fixed-width half: `ports.label.3000@/work/api`
/// sorts and reads sensibly, and a project path — which may contain almost
/// anything, `@` included — can only be the trailing part without needing an
/// escape scheme nobody would remember.
pub fn label_key(project: &Path, port: u16) -> String {
    format!("{KEY_PREFIX}{port}@{}", project.display())
}

/// Trim and bound a label the user typed.
///
/// Returns `None` for anything that is only whitespace — that is a request to
/// clear, not a name.
pub fn normalize(label: &str) -> Option<String> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_LABEL_LEN).collect())
}

/// Every persisted label, keyed by [`label_key`].
///
/// One read for the whole panel. A storage error yields an empty map, which
/// renders as "nothing has been labelled yet" — the same thing a fresh
/// install shows, and the only honest fallback when the store cannot be read.
pub fn load_labels(repo: &SettingsRepo) -> HashMap<String, String> {
    match repo.list_prefixed(KEY_PREFIX) {
        Ok(rows) => rows.into_iter().collect(),
        Err(err) => {
            tracing::warn!(
                target: "oximux_app::port_label_settings",
                "failed to read port labels: {err}"
            );
            HashMap::new()
        }
    }
}

/// Persist `label` for `project`'s `port`, or delete it when the label is
/// empty. Clearing deletes the row rather than storing `""` so the store does
/// not accumulate tombstones for every port ever seen.
pub fn save_label(repo: &SettingsRepo, project: &Path, port: u16, label: &str) {
    let key = label_key(project, port);
    let result = match normalize(label) {
        Some(value) => repo.set(&key, &value),
        None => repo.delete(&key),
    };
    if let Err(err) = result {
        tracing::warn!(
            target: "oximux_app::port_label_settings",
            "failed to persist port label: {err}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_storage::open_memory;
    use std::path::PathBuf;

    fn repo() -> SettingsRepo {
        SettingsRepo::new(open_memory().expect("open memory"))
    }

    #[test]
    fn a_key_names_both_halves_of_the_identity() {
        let key = label_key(&PathBuf::from("/work/api"), 3000);
        assert!(key.starts_with(KEY_PREFIX));
        assert!(key.contains("3000"));
        assert!(key.ends_with("/work/api"));
    }

    #[test]
    fn the_same_port_in_two_projects_is_two_labels() {
        let repo = repo();
        save_label(&repo, &PathBuf::from("/work/api"), 3000, "API");
        save_label(&repo, &PathBuf::from("/work/web"), 3000, "Web");
        let labels = load_labels(&repo);
        assert_eq!(
            labels.get(&label_key(&PathBuf::from("/work/api"), 3000)),
            Some(&"API".to_string())
        );
        assert_eq!(
            labels.get(&label_key(&PathBuf::from("/work/web"), 3000)),
            Some(&"Web".to_string())
        );
    }

    #[test]
    fn a_label_survives_being_written_twice() {
        let repo = repo();
        let project = PathBuf::from("/work/api");
        save_label(&repo, &project, 3000, "first");
        save_label(&repo, &project, 3000, "second");
        assert_eq!(
            load_labels(&repo).get(&label_key(&project, 3000)),
            Some(&"second".to_string())
        );
    }

    #[test]
    fn clearing_a_label_removes_the_row() {
        let repo = repo();
        let project = PathBuf::from("/work/api");
        save_label(&repo, &project, 3000, "API");
        save_label(&repo, &project, 3000, "   ");
        assert!(
            load_labels(&repo).is_empty(),
            "an empty label is a request to clear, not a row that says nothing"
        );
    }

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(normalize("  API  "), Some("API".to_string()));
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("\t\n "), None);
        let long = "x".repeat(MAX_LABEL_LEN * 4);
        assert_eq!(normalize(&long).map(|s| s.chars().count()), Some(MAX_LABEL_LEN));
    }

    #[test]
    fn a_multibyte_label_is_bounded_by_characters_not_bytes() {
        // Truncating at a byte offset would panic or corrupt the label.
        let long = "é".repeat(MAX_LABEL_LEN * 2);
        let cut = normalize(&long).expect("non-empty");
        assert_eq!(cut.chars().count(), MAX_LABEL_LEN);
    }

    #[test]
    fn an_unlabelled_store_reads_as_no_labels() {
        assert!(load_labels(&repo()).is_empty());
    }
}
