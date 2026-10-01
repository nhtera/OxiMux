//! Turn `(Snapshot, query, filter, expansions)` into the palette's ordered
//! row list: section headers, items, "N more" hints and the trailing
//! "Create worktree" row. Pure — no GPUI.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::shell::search_palette::model::{ItemRef, Snapshot, TabItem, WorktreeItem};
use crate::shell::search_palette::rank::{Field, Prepared, Rank, Role, score_fields, score_simple};

/// Recent tabs shown before the first "See more".
const RECENT_TABS_CAP: usize = 6;
/// Recent tabs after expanding, and the step for each further expansion.
const RECENT_TABS_EXPANDED: usize = 70;
const RECENT_WORKTREES_STEP: usize = 20;
/// Typed-query caps: lead / trailing section while both tabs and worktrees
/// match, the per-section cap otherwise, and the step per "See more".
const LEAD_CAP: usize = 6;
const TRAIL_CAP: usize = 3;
const SECTION_CAP: usize = 50;
const SECTION_STEP: usize = 20;
/// ⌘1…⌘9 quick-select.
const MAX_DIGITS: usize = 9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    RecentTabs,
    RecentWorktrees,
    Tabs,
    Worktrees,
    Projects,
    ActionsSettings,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::RecentTabs => "RECENT CHATS & TERMINALS",
            Section::RecentWorktrees => "RECENT WORKTREES",
            Section::Tabs => "OPEN TABS",
            Section::Worktrees => "WORKTREES",
            Section::Projects => "PROJECTS",
            Section::ActionsSettings => "ACTIONS & SETTINGS",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Header(Section),
    Item {
        item: ItemRef,
        /// Byte ranges to emphasise in the row's title.
        title_ranges: Vec<Range<usize>>,
        /// ⌘N quick-select digit (empty query, first nine recent tabs).
        digit: Option<u8>,
    },
    More { section: Section, hidden: usize },
    CreateWorktree(String),
}

impl Entry {
    /// Rows the keyboard can land on (items, hints, create).
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Entry::Header(_))
    }
}

/// Restrict tabs / worktrees / projects to these projects. Empty = no filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFilter {
    pub project_ids: HashSet<String>,
}

impl ProjectFilter {
    pub fn allows(&self, project_id: &str) -> bool {
        self.project_ids.is_empty() || self.project_ids.contains(project_id)
    }
}

/// "See more" presses per section since the query last changed.
pub type Expansions = HashMap<Section, usize>;

pub fn layout(snap: &Snapshot, q: &Prepared, filter: &ProjectFilter, exp: &Expansions) -> Vec<Entry> {
    if q.is_empty() {
        recent_layout(snap, filter, exp)
    } else {
        typed_layout(snap, q, filter, exp)
    }
}

fn recent_layout(snap: &Snapshot, filter: &ProjectFilter, exp: &Expansions) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut tabs: Vec<usize> = (0..snap.tabs.len())
        .filter(|&i| {
            let t = &snap.tabs[i];
            filter.allows(&t.project_id) && (!t.is_current || t.needs_attention)
        })
        .collect();
    tabs.sort_by_key(|&i| (Reverse(snap.tabs[i].last_ms), snap.tabs[i].uid));
    let presses = exp.get(&Section::RecentTabs).copied().unwrap_or(0);
    let tab_cap = if presses == 0 {
        RECENT_TABS_CAP
    } else {
        RECENT_TABS_EXPANDED * presses
    };
    let visible_tabs = tabs.len().min(tab_cap);
    if !tabs.is_empty() {
        out.push(Entry::Header(Section::RecentTabs));
        for (n, &i) in tabs.iter().take(visible_tabs).enumerate() {
            out.push(Entry::Item {
                item: ItemRef::Tab(i),
                title_ranges: Vec::new(),
                digit: (n < MAX_DIGITS).then_some(n as u8 + 1),
            });
        }
        push_more(&mut out, Section::RecentTabs, tabs.len() - visible_tabs);
    }

    let mut worktrees: Vec<usize> = (0..snap.worktrees.len())
        .filter(|&i| {
            let w = &snap.worktrees[i];
            filter.allows(&w.project_id) && !w.is_current
        })
        .collect();
    worktrees.sort_by(|&a, &b| {
        let (a, b) = (&snap.worktrees[a], &snap.worktrees[b]);
        // Visited this run first (most recent first), then by agent activity.
        b.last_visited_ms
            .is_some()
            .cmp(&a.last_visited_ms.is_some())
            .then(b.last_visited_ms.cmp(&a.last_visited_ms))
            .then(b.last_activity_ms.cmp(&a.last_activity_ms))
            .then_with(|| a.name.cmp(&b.name))
    });
    let base = if visible_tabs == 0 { 10 } else { 5 };
    let wt_cap = base.min(10usize.saturating_sub(visible_tabs).max(1))
        + RECENT_WORKTREES_STEP * exp.get(&Section::RecentWorktrees).copied().unwrap_or(0);
    if !worktrees.is_empty() {
        out.push(Entry::Header(Section::RecentWorktrees));
        let shown = worktrees.len().min(wt_cap);
        out.extend(worktrees.iter().take(shown).map(|&i| item(ItemRef::Worktree(i), Vec::new())));
        push_more(&mut out, Section::RecentWorktrees, worktrees.len() - shown);
    }
    out
}

/// One ranked hit: list index, rank, title ranges.
type Hit = (usize, Rank, Vec<Range<usize>>);

fn typed_layout(snap: &Snapshot, q: &Prepared, filter: &ProjectFilter, exp: &Expansions) -> Vec<Entry> {
    let now = snap.now_ms;
    let mut tabs: Vec<Hit> = snap
        .tabs
        .iter()
        .enumerate()
        .filter(|(_, t)| filter.allows(&t.project_id))
        .filter_map(|(i, t)| score_tab(q, t).map(|(r, hl)| (i, r, hl)))
        .collect();
    tabs.sort_by(|a, b| {
        let (ta, tb) = (&snap.tabs[a.0], &snap.tabs[b.0]);
        b.1.cmp(&a.1)
            .then(age_bucket(ta.last_ms, now).cmp(&age_bucket(tb.last_ms, now)))
            .then(tb.last_ms.cmp(&ta.last_ms))
            .then(ta.is_current.cmp(&tb.is_current))
            .then(ta.uid.cmp(&tb.uid))
    });
    let mut worktrees: Vec<Hit> = snap
        .worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| filter.allows(&w.project_id))
        .filter_map(|(i, w)| score_worktree(q, w).map(|(r, hl)| (i, r, hl)))
        .collect();
    worktrees.sort_by(|a, b| {
        let (wa, wb) = (&snap.worktrees[a.0], &snap.worktrees[b.0]);
        let (ma, mb) = (worktree_ms(wa), worktree_ms(wb));
        b.1.cmp(&a.1)
            .then(age_bucket(ma, now).cmp(&age_bucket(mb, now)))
            .then(mb.cmp(&ma))
            .then(wa.is_current.cmp(&wb.is_current))
            .then_with(|| wa.workspace_id.cmp(&wb.workspace_id))
    });

    // Lead with the kind whose best hit ranks higher — full rank, so a
    // worktree matched on its own name beats tabs that only match through
    // their worktree field (ties → tabs).
    let best = |hits: &[Hit]| hits.first().map(|h| h.1);
    let tabs_lead = worktrees.is_empty() || (!tabs.is_empty() && best(&tabs) >= best(&worktrees));
    let both = !tabs.is_empty() && !worktrees.is_empty();
    let cap = |section: Section, initial: usize| {
        let presses = exp.get(&section).copied().unwrap_or(0);
        if presses == 0 { initial } else { SECTION_CAP.max(initial) + SECTION_STEP * (presses - 1) }
    };
    let (lead_cap, trail_cap) = if both { (LEAD_CAP, TRAIL_CAP) } else { (SECTION_CAP, SECTION_CAP) };

    let mut out = Vec::new();
    let tab_block = |out: &mut Vec<Entry>, initial| {
        push_section(out, Section::Tabs, cap(Section::Tabs, initial), tabs.iter().map(|h| (ItemRef::Tab(h.0), h.2.clone())));
    };
    let wt_block = |out: &mut Vec<Entry>, initial| {
        let rows = worktrees.iter().map(|h| (ItemRef::Worktree(h.0), h.2.clone()));
        push_section(out, Section::Worktrees, cap(Section::Worktrees, initial), rows);
    };
    if tabs_lead {
        tab_block(&mut out, lead_cap);
        wt_block(&mut out, trail_cap);
    } else {
        wt_block(&mut out, lead_cap);
        tab_block(&mut out, trail_cap);
    }

    let mut projects: Vec<Hit> = snap
        .projects
        .iter()
        .enumerate()
        .filter(|(_, p)| filter.allows(&p.project_id))
        .filter_map(|(i, p)| {
            let fields = [
                Field { text: &p.name, role: Role::Primary },
                Field { text: &p.root_path, role: Role::Secondary },
            ];
            score_fields(q, &fields).map(|s| (i, s.rank, s.ranges[0].clone()))
        })
        .collect();
    projects.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let rows = projects.iter().map(|h| (ItemRef::Project(h.0), h.2.clone()));
    push_section(&mut out, Section::Projects, cap(Section::Projects, SECTION_CAP), rows);

    let mut misc: Vec<(ItemRef, u32, &str)> = Vec::new();
    for (i, s) in snap.settings.iter().enumerate() {
        if let Some(score) = score_simple(q, &format!("{} {}", s.label, s.keywords)) {
            misc.push((ItemRef::Setting(i), score, s.label));
        }
    }
    for (i, a) in snap.actions.iter().enumerate() {
        if let Some(score) = score_simple(q, &a.search_text) {
            misc.push((ItemRef::Action(i), score, a.label.as_str()));
        }
    }
    misc.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(b.2)));
    let rows = misc.iter().map(|(r, _, label)| {
        let hl = crate::shell::command_palette::match_engine::match_ranges(&q.raw, label);
        (*r, hl)
    });
    push_section(&mut out, Section::ActionsSettings, cap(Section::ActionsSettings, SECTION_CAP), rows);

    let name = q.raw.trim();
    if !name.is_empty() {
        out.push(Entry::CreateWorktree(name.to_string()));
    }
    out
}

fn score_tab(q: &Prepared, t: &TabItem) -> Option<(Rank, Vec<Range<usize>>)> {
    let aliases = t.aliases.join(" ");
    let path = t.path.as_deref().unwrap_or("");
    let fields = [
        Field { text: &t.title, role: Role::Primary },
        Field { text: &t.worktree_name, role: Role::Secondary },
        Field { text: &t.branch, role: Role::Secondary },
        Field { text: path, role: Role::Secondary },
        Field { text: &aliases, role: Role::Alias },
        Field { text: &t.project_name, role: Role::Container },
    ];
    score_fields(q, &fields).map(|s| (s.rank, s.ranges[0].clone()))
}

fn score_worktree(q: &Prepared, w: &WorktreeItem) -> Option<(Rank, Vec<Range<usize>>)> {
    let fields = [
        Field { text: &w.name, role: Role::Primary },
        Field { text: &w.branch, role: Role::Secondary },
        Field { text: &w.project_name, role: Role::Container },
    ];
    score_fields(q, &fields).map(|s| (s.rank, s.ranges[0].clone()))
}

fn worktree_ms(w: &WorktreeItem) -> i64 {
    w.last_visited_ms.unwrap_or(w.last_activity_ms).max(w.last_activity_ms)
}

/// Coarse recency for tie-breaks: < 1h, < 1d, < 1w, older, unknown.
fn age_bucket(ms: i64, now: i64) -> u8 {
    if ms <= 0 {
        return 4;
    }
    match now.saturating_sub(ms) {
        d if d < 3_600_000 => 0,
        d if d < 86_400_000 => 1,
        d if d < 604_800_000 => 2,
        _ => 3,
    }
}

fn item(item: ItemRef, title_ranges: Vec<Range<usize>>) -> Entry {
    Entry::Item { item, title_ranges, digit: None }
}

fn push_more(out: &mut Vec<Entry>, section: Section, hidden: usize) {
    if hidden > 0 {
        out.push(Entry::More { section, hidden });
    }
}

fn push_section(
    out: &mut Vec<Entry>,
    section: Section,
    cap: usize,
    rows: impl ExactSizeIterator<Item = (ItemRef, Vec<Range<usize>>)>,
) {
    let total = rows.len();
    if total == 0 {
        return;
    }
    out.push(Entry::Header(section));
    out.extend(rows.take(cap).map(|(r, hl)| item(r, hl)));
    push_more(out, section, total.saturating_sub(cap));
}
