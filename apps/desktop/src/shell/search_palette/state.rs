//! The palette's non-render state machine: query → entries, selection,
//! "See more" expansion, filter, activation. Pure (no GPUI) so it is
//! unit-tested; the view only forwards input and renders `entries`.

use crate::shell::search_palette::model::{ItemRef, Snapshot, Target};
use crate::shell::search_palette::rank::{Prepared, prepare};
use crate::shell::search_palette::sections::{Entry, Expansions, ProjectFilter, Section, layout};

/// What activating a row produced.
#[derive(Clone)]
pub enum Outcome {
    /// Close the palette and run this.
    Run(Target),
    /// A "See more" row grew its section; the palette stays open.
    Expanded,
}

pub struct PaletteState {
    pub snapshot: Snapshot,
    query: String,
    prepared: Prepared,
    pub filter: ProjectFilter,
    expansions: Expansions,
    pub entries: Vec<Entry>,
    /// Index into `entries` of the highlighted row (always selectable).
    pub selected: Option<usize>,
}

impl PaletteState {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut s = Self {
            snapshot,
            query: String::new(),
            prepared: Prepared::default(),
            filter: ProjectFilter::default(),
            expansions: Expansions::new(),
            entries: Vec::new(),
            selected: None,
        };
        s.relayout_and_select_first();
        s
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// A new query resets "See more" caps and lands on the first item.
    pub fn set_query(&mut self, query: &str) {
        if query == self.query {
            return;
        }
        self.query = query.to_string();
        self.prepared = prepare(query);
        self.expansions.clear();
        self.relayout_and_select_first();
    }

    pub fn toggle_project_filter(&mut self, project_id: &str) {
        if !self.filter.project_ids.remove(project_id) {
            self.filter.project_ids.insert(project_id.to_string());
        }
        self.relayout_and_select_first();
    }

    pub fn clear_filter(&mut self) {
        self.filter.project_ids.clear();
        self.relayout_and_select_first();
    }

    /// Move the highlight `delta` selectable rows, wrapping at both ends.
    pub fn move_selection(&mut self, delta: isize) {
        let selectable: Vec<usize> = (0..self.entries.len())
            .filter(|&i| self.entries[i].is_selectable())
            .collect();
        if selectable.is_empty() {
            self.selected = None;
            return;
        }
        let n = selectable.len() as isize;
        let pos = self
            .selected
            .and_then(|s| selectable.iter().position(|&i| i == s))
            .map(|p| p as isize)
            .unwrap_or(if delta > 0 { -1 } else { 0 });
        self.selected = Some(selectable[(pos + delta).rem_euclid(n) as usize]);
    }

    pub fn activate_selected(&mut self) -> Option<Outcome> {
        self.activate_at(self.selected?)
    }

    pub fn activate_at(&mut self, idx: usize) -> Option<Outcome> {
        match self.entries.get(idx)? {
            Entry::Header(_) => None,
            Entry::Item { item, .. } => Some(Outcome::Run(self.snapshot.target(*item))),
            Entry::CreateWorktree(name) => Some(Outcome::Run(Target::CreateWorktree(name.clone()))),
            Entry::More { section, .. } => {
                *self.expansions.entry(*section).or_default() += 1;
                self.entries = layout(&self.snapshot, &self.prepared, &self.filter, &self.expansions);
                // Keep the highlight where it was: the first newly revealed
                // row now sits at the hint's old index.
                self.selected = Some(idx.min(self.entries.len().saturating_sub(1)));
                if !self.entries.get(idx).is_some_and(Entry::is_selectable) {
                    self.move_selection(1);
                }
                Some(Outcome::Expanded)
            }
        }
    }

    /// ⌘N: the recent tab carrying digit `n` — empty query only. ⌘1–9
    /// reach the nine most recent tabs even while only six are shown.
    pub fn quick_select(&self, n: u8) -> Option<Target> {
        if !self.prepared.is_empty() {
            return None;
        }
        let find = |entries: &[Entry]| {
            entries.iter().find_map(|e| match e {
                Entry::Item { item: item @ ItemRef::Tab(_), digit: Some(d), .. } if *d == n => Some(*item),
                _ => None,
            })
        };
        let item = find(&self.entries).or_else(|| {
            let mut expanded = self.expansions.clone();
            expanded.insert(Section::RecentTabs, 1);
            find(&layout(&self.snapshot, &self.prepared, &self.filter, &expanded))
        })?;
        Some(self.snapshot.target(item))
    }

    /// Per-project row counts (tabs + worktrees) for the filter popover.
    pub fn project_counts(&self) -> Vec<(String, String, usize)> {
        self.snapshot
            .projects
            .iter()
            .map(|p| {
                let n = self.snapshot.tabs.iter().filter(|t| t.project_id == p.project_id).count()
                    + self.snapshot.worktrees.iter().filter(|w| w.project_id == p.project_id).count();
                (p.project_id.clone(), p.name.clone(), n)
            })
            .collect()
    }

    fn relayout_and_select_first(&mut self) {
        self.entries = layout(&self.snapshot, &self.prepared, &self.filter, &self.expansions);
        // The first item, never a hint. "Create worktree" only when it is
        // the one thing Enter could do — never ahead of a real match.
        self.selected = self
            .entries
            .iter()
            .position(|e| matches!(e, Entry::Item { .. }))
            .or_else(|| self.entries.iter().position(|e| matches!(e, Entry::CreateWorktree(_))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::search_palette::model::{ProjectItem, TabItem, WorktreeItem};

    fn snapshot(tabs: usize, worktrees: usize) -> Snapshot {
        Snapshot {
            tabs: (0..tabs)
                .map(|i| TabItem {
                    project_id: "p".into(),
                    uid: i as u64 + 1,
                    title: format!("tab {i}"),
                    last_ms: 1_000 - i as i64,
                    ..Default::default()
                })
                .collect(),
            worktrees: (0..worktrees)
                .map(|i| WorktreeItem {
                    workspace_id: format!("w{i}"),
                    project_id: "p".into(),
                    name: format!("wt {i}"),
                    ..Default::default()
                })
                .collect(),
            projects: vec![ProjectItem { project_id: "p".into(), name: "P".into(), ..Default::default() }],
            now_ms: 2_000,
            ..Default::default()
        }
    }

    fn selected_entry(s: &PaletteState) -> &Entry {
        &s.entries[s.selected.unwrap()]
    }

    #[test]
    fn opens_on_first_item_and_wraps_both_ways() {
        let mut s = PaletteState::new(snapshot(2, 0));
        assert!(matches!(selected_entry(&s), Entry::Item { item: ItemRef::Tab(0), .. }));
        s.move_selection(-1);
        assert!(matches!(selected_entry(&s), Entry::Item { item: ItemRef::Tab(1), .. }));
        s.move_selection(1);
        assert!(matches!(selected_entry(&s), Entry::Item { item: ItemRef::Tab(0), .. }));
    }

    #[test]
    fn see_more_keeps_selection_index_and_query_change_resets() {
        let mut s = PaletteState::new(snapshot(10, 0));
        let more = s.entries.iter().position(|e| matches!(e, Entry::More { .. })).unwrap();
        s.selected = Some(more);
        assert!(matches!(s.activate_selected(), Some(Outcome::Expanded)));
        assert_eq!(s.selected, Some(more));
        assert!(matches!(selected_entry(&s), Entry::Item { item: ItemRef::Tab(6), .. }));
        assert!(!s.entries.iter().any(|e| matches!(e, Entry::More { .. })));
        // Typing then clearing restores the collapsed caps.
        s.set_query("tab");
        s.set_query("");
        assert!(s.entries.iter().any(|e| matches!(e, Entry::More { .. })));
        assert!(matches!(selected_entry(&s), Entry::Item { item: ItemRef::Tab(0), .. }));
    }

    #[test]
    fn create_worktree_row_is_selected_only_when_alone() {
        let mut s = PaletteState::new(snapshot(0, 0));
        s.set_query("brand-new");
        assert!(matches!(selected_entry(&s), Entry::CreateWorktree(n) if n == "brand-new"));
        assert!(matches!(s.activate_selected(), Some(Outcome::Run(Target::CreateWorktree(_)))));
        // With a real match, the match is selected, not the create row.
        let mut s = PaletteState::new(snapshot(2, 0));
        s.set_query("tab");
        assert!(matches!(selected_entry(&s), Entry::Item { .. }));
    }

    #[test]
    fn quick_select_reaches_tabs_behind_see_more() {
        let s = PaletteState::new(snapshot(9, 0));
        assert!(s.quick_select(9).is_some(), "⌘9 works while only six rows show");
        assert!(PaletteState::new(snapshot(9, 0)).quick_select(10).is_none());
    }

    #[test]
    fn quick_select_only_with_empty_query() {
        let mut s = PaletteState::new(snapshot(3, 0));
        assert!(matches!(s.quick_select(2), Some(Target::Tab { uid: 2, .. })));
        assert!(s.quick_select(9).is_none());
        s.set_query("tab");
        assert!(s.quick_select(1).is_none());
    }

    #[test]
    fn filter_toggle_round_trips() {
        let mut s = PaletteState::new(snapshot(1, 1));
        s.toggle_project_filter("other");
        assert!(s.entries.is_empty());
        s.toggle_project_filter("other");
        assert!(!s.entries.is_empty());
        assert_eq!(s.project_counts(), vec![("p".into(), "P".into(), 2)]);
    }
}
