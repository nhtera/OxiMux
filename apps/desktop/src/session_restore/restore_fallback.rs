//! Restore-path shape validation + corrupt-payload preservation.
//!
//! A persisted layout blob can be damaged two ways: it fails to parse
//! (truncated mid-autosave), or it parses but violates a tree invariant
//! the live `PaneTree` code relies on (a `Split` whose `weights` length
//! doesn't match `children`, empty splits, non-finite weights, group/tab
//! counts out of sync). The second class is the dangerous one — it flows
//! through `restore_tree` unchecked and panics much later, e.g. in
//! `PaneTree::remove_leaf`'s unconditional parallel-array removal.
//!
//! `validate_persisted_tabs` is the single gate: every blob is checked
//! right after parse, BEFORE any live tree is built. On failure the raw
//! payload is preserved aside (`*.corrupt.json`, newest 3 kept) so the
//! layout is recoverable by hand, and the caller falls back to the
//! default single-pane layout with a toast.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::persisted_terminals::{PersistedTabs, PersistedTree, count_leaves};

/// Hard cap on persisted-tree nesting. Real layouts are a handful of
/// levels; serde_json's own 128-level parse limit already rejects deeper
/// JSON, so this is an independent belt-and-suspenders bound in case
/// that limit ever changes.
const MAX_TREE_DEPTH: usize = 64;

/// Why a parsed layout blob was rejected before restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreShapeError {
    /// A `Split` node with zero children — `count_leaves` returns 0 and
    /// downstream id allocation produces an empty group set.
    EmptySplit,
    /// `weights.len() != children.len()` on a `Split` — the live tree's
    /// parallel-array invariant would be violated from the first frame.
    WeightMismatch,
    /// A weight that is NaN, infinite, or not strictly positive — ratio
    /// math (`w / sum`) would propagate NaN into every pane size.
    BadWeight,
    /// v3 blob: `group_tree` leaf count doesn't match `groups.len()`.
    GroupCountMismatch,
    /// v3 blob: per-group `tab_count`s don't sum to `tabs.len()`.
    TabCountMismatch,
    /// A tab whose `sub_panes` list doesn't pair 1:1 with its tree's
    /// leaves (only checked when `sub_panes` is present at all — legacy
    /// pre-v4 blobs legitimately carry an empty list).
    SubPaneCountMismatch,
    /// Tree nesting beyond [`MAX_TREE_DEPTH`] — no real layout gets
    /// close; only hand-crafted or corrupted payloads do.
    TooDeep,
}

impl fmt::Display for RestoreShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::EmptySplit => "split node with no children",
            Self::WeightMismatch => "split weights/children length mismatch",
            Self::BadWeight => "non-finite or non-positive split weight",
            Self::GroupCountMismatch => "group tree leaves do not match groups",
            Self::TabCountMismatch => "group tab counts do not match tab list",
            Self::SubPaneCountMismatch => "sub-pane list does not match tree leaves",
            Self::TooDeep => "tree nested beyond the depth cap",
        };
        f.write_str(s)
    }
}

/// Recursively check one persisted tree's shape invariants.
fn validate_tree(t: &PersistedTree) -> Result<(), RestoreShapeError> {
    validate_tree_at(t, 0)
}

fn validate_tree_at(t: &PersistedTree, depth: usize) -> Result<(), RestoreShapeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(RestoreShapeError::TooDeep);
    }
    match t {
        PersistedTree::Leaf => Ok(()),
        PersistedTree::Split {
            children, weights, ..
        } => {
            if children.is_empty() {
                return Err(RestoreShapeError::EmptySplit);
            }
            if weights.len() != children.len() {
                return Err(RestoreShapeError::WeightMismatch);
            }
            if weights.iter().any(|w| !w.is_finite() || *w <= 0.0) {
                return Err(RestoreShapeError::BadWeight);
            }
            children
                .iter()
                .try_for_each(|c| validate_tree_at(c, depth + 1))
        }
    }
}

/// Validate every tree + cross-count invariant in a parsed snapshot.
/// Called once per load, before any live entity is built. Healthy blobs
/// written by `save_persisted_tabs` always pass — the writer derives all
/// counts from the same live state.
pub fn validate_persisted_tabs(snap: &PersistedTabs) -> Result<(), RestoreShapeError> {
    if let Some(tree) = &snap.group_tree {
        validate_tree(tree)?;
        // The multi-group path only engages when `groups` is non-empty
        // (otherwise the restorer takes the legacy flat path), so the
        // cross-counts only need to hold in that case.
        if !snap.groups.is_empty() {
            if count_leaves(tree) != snap.groups.len() {
                return Err(RestoreShapeError::GroupCountMismatch);
            }
            let total: usize = snap.groups.iter().map(|g| g.tab_count).sum();
            if total != snap.tabs.len() {
                return Err(RestoreShapeError::TabCountMismatch);
            }
        }
    }
    for tab in &snap.tabs {
        validate_tree(&tab.tree)?;
        if !tab.sub_panes.is_empty() && count_leaves(&tab.tree) != tab.sub_panes.len() {
            return Err(RestoreShapeError::SubPaneCountMismatch);
        }
    }
    Ok(())
}

/// Where rejected layout payloads are preserved for manual recovery.
/// `OXIMUX_CORRUPT_LAYOUTS_DIR` overrides the default (used by tests and
/// crash drills to keep artifacts out of the real data dir).
pub fn corrupt_layouts_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("OXIMUX_CORRUPT_LAYOUTS_DIR")
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    crate::app_paths::data_dir().map(|d| d.join("corrupt-layouts"))
}

/// Save a rejected payload as `<key>-<epoch-secs>.corrupt.json` under
/// `dir`, then trim that key's preserved copies to the newest 3. Best
/// effort — IO failures are logged, never propagated (the restore
/// fallback must proceed regardless).
pub fn preserve_corrupt_payload(dir: &Path, key: &str, raw: &str) -> Option<PathBuf> {
    let safe_key: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if let Err(err) = std::fs::create_dir_all(dir) {
        tracing::warn!(?err, ?dir, "corrupt-layout preserve: create_dir_all failed");
        return None;
    }
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{safe_key}-{epoch}.corrupt.json"));
    if let Err(err) = std::fs::write(&path, raw) {
        tracing::warn!(?err, ?path, "corrupt-layout preserve: write failed");
        return None;
    }
    trim_preserved(dir, &safe_key, 3);
    Some(path)
}

/// Keep only the newest `keep` preserved copies for `safe_key`, ordered
/// by file modification time (oldest removed first).
fn trim_preserved(dir: &Path, safe_key: &str, keep: usize) {
    let prefix = format!("{safe_key}-");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut matches: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".corrupt.json"))
        })
        .filter_map(|e| {
            let mtime = e.metadata().and_then(|m| m.modified()).ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    if matches.len() <= keep {
        return;
    }
    matches.sort_by_key(|(mtime, _)| *mtime);
    let excess = matches.len() - keep;
    for (_, stale) in &matches[..excess] {
        let _ = std::fs::remove_file(stale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persisted_terminals::{
        PersistedAxis, PersistedGroup, PersistedSubPane, PersistedTab,
    };

    fn split(children: Vec<PersistedTree>, weights: Vec<f32>) -> PersistedTree {
        PersistedTree::Split {
            axis: PersistedAxis::Horizontal,
            children,
            weights,
        }
    }

    fn tab_with_tree(tree: PersistedTree, sub_panes: usize) -> PersistedTab {
        PersistedTab {
            tree,
            sub_panes: (0..sub_panes).map(|_| PersistedSubPane::default()).collect(),
            ..PersistedTab::default()
        }
    }

    #[test]
    fn healthy_round_trip_passes() {
        // Mirrors what save_persisted_tabs emits: counts derived from one
        // live state, weights parallel to children.
        let snap = PersistedTabs {
            tabs: vec![
                tab_with_tree(PersistedTree::Leaf, 1),
                tab_with_tree(
                    split(vec![PersistedTree::Leaf, PersistedTree::Leaf], vec![1.0, 1.0]),
                    2,
                ),
            ],
            group_tree: Some(split(
                vec![PersistedTree::Leaf, PersistedTree::Leaf],
                vec![3.0, 1.0],
            )),
            groups: vec![
                PersistedGroup { tab_count: 1, ..Default::default() },
                PersistedGroup { tab_count: 1, ..Default::default() },
            ],
            ..PersistedTabs::default()
        };
        assert_eq!(validate_persisted_tabs(&snap), Ok(()));
    }

    #[test]
    fn legacy_flat_blob_passes() {
        // Pre-v3: no group tree, no groups, no sub_panes.
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(PersistedTree::Leaf, 0)],
            ..PersistedTabs::default()
        };
        assert_eq!(validate_persisted_tabs(&snap), Ok(()));
    }

    #[test]
    fn weight_mismatch_rejected() {
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(
                split(vec![PersistedTree::Leaf, PersistedTree::Leaf], vec![1.0]),
                0,
            )],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::WeightMismatch)
        );
    }

    #[test]
    fn empty_split_rejected() {
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(PersistedTree::Leaf, 0)],
            group_tree: Some(split(Vec::new(), Vec::new())),
            groups: vec![PersistedGroup { tab_count: 1, ..Default::default() }],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::EmptySplit)
        );
    }

    #[test]
    fn nan_and_nonpositive_weights_rejected() {
        for bad in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            let snap = PersistedTabs {
                tabs: vec![tab_with_tree(
                    split(
                        vec![PersistedTree::Leaf, PersistedTree::Leaf],
                        vec![1.0, bad],
                    ),
                    0,
                )],
                ..PersistedTabs::default()
            };
            assert_eq!(
                validate_persisted_tabs(&snap),
                Err(RestoreShapeError::BadWeight),
                "weight {bad} must be rejected"
            );
        }
    }

    #[test]
    fn group_leaf_count_mismatch_rejected() {
        // 2-leaf group tree but 3 group records — a v2→v3 style desync.
        let snap = PersistedTabs {
            tabs: vec![
                tab_with_tree(PersistedTree::Leaf, 0),
                tab_with_tree(PersistedTree::Leaf, 0),
                tab_with_tree(PersistedTree::Leaf, 0),
            ],
            group_tree: Some(split(
                vec![PersistedTree::Leaf, PersistedTree::Leaf],
                vec![1.0, 1.0],
            )),
            groups: vec![
                PersistedGroup { tab_count: 1, ..Default::default() },
                PersistedGroup { tab_count: 1, ..Default::default() },
                PersistedGroup { tab_count: 1, ..Default::default() },
            ],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::GroupCountMismatch)
        );
    }

    #[test]
    fn group_tab_count_sum_mismatch_rejected() {
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(PersistedTree::Leaf, 0)],
            group_tree: Some(split(
                vec![PersistedTree::Leaf, PersistedTree::Leaf],
                vec![1.0, 1.0],
            )),
            groups: vec![
                PersistedGroup { tab_count: 1, ..Default::default() },
                PersistedGroup { tab_count: 1, ..Default::default() },
            ],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::TabCountMismatch)
        );
    }

    #[test]
    fn sub_pane_count_mismatch_rejected() {
        // 2-leaf tab tree but 3 sub-pane records.
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(
                split(vec![PersistedTree::Leaf, PersistedTree::Leaf], vec![1.0, 1.0]),
                3,
            )],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::SubPaneCountMismatch)
        );
    }

    #[test]
    fn depth_beyond_cap_rejected() {
        // Build a chain nested one past MAX_TREE_DEPTH. Each level is a
        // well-formed 2-child split so only the depth check can fire.
        let mut tree = PersistedTree::Leaf;
        for _ in 0..(MAX_TREE_DEPTH + 1) {
            tree = split(vec![tree, PersistedTree::Leaf], vec![1.0, 1.0]);
        }
        let snap = PersistedTabs {
            tabs: vec![tab_with_tree(tree, 0)],
            ..PersistedTabs::default()
        };
        assert_eq!(
            validate_persisted_tabs(&snap),
            Err(RestoreShapeError::TooDeep)
        );
    }

    #[test]
    fn preserve_writes_and_trims_to_three() {
        let dir = std::env::temp_dir().join(format!(
            "oximux-corrupt-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // Stage 5 older copies for the key; the real preserve below adds
        // a 6th (newest by mtime) and must trim down to 3.
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..5 {
            std::fs::write(
                dir.join(format!("terminal_tabs_p1_main-{n}.corrupt.json")),
                "{}",
            )
            .unwrap();
        }
        // An unrelated key's file must survive the trim untouched.
        std::fs::write(dir.join("terminal_tabs_p2_main-0.corrupt.json"), "{}").unwrap();
        // Ensure the preserved copy's mtime strictly exceeds the staged
        // files' so the trim can never pick it as oldest on a tie.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let written = preserve_corrupt_payload(&dir, "terminal_tabs:p1:main", "{broken")
            .expect("preserve writes");
        assert!(written.exists());
        let p1_files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("terminal_tabs_p1_main-"))
            .collect();
        assert_eq!(p1_files.len(), 3, "newest 3 kept for the corrupt key");
        assert!(dir.join("terminal_tabs_p2_main-0.corrupt.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
