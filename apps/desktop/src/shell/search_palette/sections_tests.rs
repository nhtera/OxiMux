use std::collections::HashSet;
use std::time::Instant;

use crate::shell::search_palette::model::{ItemRef, ProjectItem, Snapshot, TabItem, WorktreeItem};
use crate::shell::search_palette::rank::prepare;
use crate::shell::search_palette::sections::*;

const NOW: i64 = 1_800_000_000_000;

fn tab(uid: u64, title: &str, project: &str, last_ms: i64) -> TabItem {
    TabItem {
        project_id: project.into(),
        uid,
        title: title.into(),
        icon: "icons/square-terminal.svg",
        aliases: vec!["terminal".into()],
        project_name: project.into(),
        worktree_name: "main".into(),
        branch: "main".into(),
        last_ms,
        ..Default::default()
    }
}

fn wt(id: &str, name: &str, project: &str, activity: i64) -> WorktreeItem {
    WorktreeItem {
        workspace_id: id.into(),
        project_id: project.into(),
        worktree_path: format!("/{project}/{name}"),
        name: name.into(),
        branch: name.into(),
        project_name: project.into(),
        last_activity_ms: activity,
        ..Default::default()
    }
}

fn snap(tabs: Vec<TabItem>, worktrees: Vec<WorktreeItem>) -> Snapshot {
    let projects = ["OxiMux", "Other"]
        .iter()
        .map(|p| ProjectItem { project_id: (*p).into(), name: (*p).into(), ..Default::default() })
        .collect();
    Snapshot { tabs, worktrees, projects, now_ms: NOW, ..Default::default() }
}

fn run(s: &Snapshot, query: &str) -> Vec<Entry> {
    layout(s, &prepare(query), &ProjectFilter::default(), &Expansions::new())
}

fn items(entries: &[Entry]) -> Vec<ItemRef> {
    entries
        .iter()
        .filter_map(|e| match e {
            Entry::Item { item, .. } => Some(*item),
            _ => None,
        })
        .collect()
}

fn count(entries: &[Entry], f: impl Fn(&ItemRef) -> bool) -> usize {
    items(entries).iter().filter(|i| f(i)).count()
}

fn is_tab(i: &ItemRef) -> bool {
    matches!(i, ItemRef::Tab(_))
}

fn is_wt(i: &ItemRef) -> bool {
    matches!(i, ItemRef::Worktree(_))
}

fn many_worktrees(n: usize) -> Vec<WorktreeItem> {
    (0..n).map(|i| wt(&format!("w{i}"), &format!("wt-{i}"), "OxiMux", NOW - i as i64)).collect()
}

#[test]
fn empty_query_orders_recent_tabs_and_excludes_current() {
    let mut current = tab(3, "current", "OxiMux", NOW);
    current.is_current = true;
    let s = snap(vec![tab(1, "old", "OxiMux", NOW - 5_000), tab(2, "new", "OxiMux", NOW - 10), current], vec![]);
    let e = run(&s, "");
    assert_eq!(e[0], Entry::Header(Section::RecentTabs));
    assert_eq!(items(&e), vec![ItemRef::Tab(1), ItemRef::Tab(0)]);
}

#[test]
fn current_tab_shown_when_it_needs_attention() {
    let mut current = tab(1, "current", "OxiMux", NOW);
    current.is_current = true;
    current.needs_attention = true;
    assert_eq!(items(&run(&snap(vec![current], vec![]), "")), vec![ItemRef::Tab(0)]);
}

#[test]
fn empty_query_worktree_caps() {
    // No tabs → 10 worktrees.
    let e = run(&snap(vec![], many_worktrees(30)), "");
    assert_eq!(count(&e, is_wt), 10);
    // Six tabs → min(5, 10 - 6) = 4 worktrees.
    let tabs = (0..6).map(|i| tab(i, &format!("t{i}"), "OxiMux", NOW - i as i64)).collect();
    let e = run(&snap(tabs, many_worktrees(30)), "");
    assert_eq!(count(&e, is_tab), 6);
    assert_eq!(count(&e, is_wt), 4);
}

#[test]
fn recent_tabs_cap_and_see_more() {
    let tabs: Vec<_> = (0..20).map(|i| tab(i, &format!("t{i}"), "OxiMux", NOW - i as i64)).collect();
    let s = snap(tabs, vec![]);
    let e = run(&s, "");
    assert_eq!(count(&e, is_tab), 6);
    assert!(e.contains(&Entry::More { section: Section::RecentTabs, hidden: 14 }));
    let mut exp = Expansions::new();
    exp.insert(Section::RecentTabs, 1);
    let e = layout(&s, &prepare(""), &ProjectFilter::default(), &exp);
    assert_eq!(count(&e, is_tab), 20);
    assert!(!e.iter().any(|x| matches!(x, Entry::More { .. })));
}

#[test]
fn digits_only_on_first_nine_recent_tabs() {
    let tabs: Vec<_> = (0..12).map(|i| tab(i, &format!("t{i}"), "OxiMux", NOW - i as i64)).collect();
    let mut exp = Expansions::new();
    exp.insert(Section::RecentTabs, 1);
    let e = layout(&snap(tabs, vec![]), &prepare(""), &ProjectFilter::default(), &exp);
    let digits: Vec<_> = e
        .iter()
        .filter_map(|x| match x {
            Entry::Item { digit, .. } => Some(*digit),
            _ => None,
        })
        .collect();
    assert_eq!(digits.iter().flatten().count(), 9);
    assert_eq!(digits[0], Some(1));
    assert_eq!(digits[8], Some(9));
    assert_eq!(digits[9], None);
    // Typed queries carry no digits.
    let e = run(&snap(vec![tab(1, "alpha", "OxiMux", NOW)], vec![]), "alpha");
    assert!(e.iter().all(|x| !matches!(x, Entry::Item { digit: Some(_), .. })));
}

#[test]
fn visited_worktrees_come_first() {
    let mut visited = wt("v", "visited", "OxiMux", NOW - 1_000_000);
    visited.last_visited_ms = Some(NOW - 50);
    let s = snap(vec![], vec![wt("a", "active", "OxiMux", NOW), visited]);
    assert_eq!(items(&run(&s, "")), vec![ItemRef::Worktree(1), ItemRef::Worktree(0)]);
}

#[test]
fn typed_query_multi_token_across_fields() {
    let s = snap(vec![], vec![wt("m", "main", "OxiMux", NOW), wt("o", "main", "Other", NOW)]);
    let e = run(&s, "ox ma");
    assert_eq!(items(&e).into_iter().filter(is_wt).collect::<Vec<_>>(), vec![ItemRef::Worktree(0)]);
}

#[test]
fn exact_name_ranks_above_prefix() {
    let s = snap(vec![], vec![wt("a", "maintenance", "OxiMux", NOW), wt("b", "main", "OxiMux", NOW - 99_999_999)]);
    assert_eq!(items(&run(&s, "main"))[0], ItemRef::Worktree(1));
}

#[test]
fn both_kinds_matching_uses_lead_and_trail_caps() {
    let tabs: Vec<_> = (0..10).map(|i| tab(i, &format!("deploy {i}"), "OxiMux", NOW - i as i64)).collect();
    let wts: Vec<_> = (0..10).map(|i| wt(&format!("w{i}"), &format!("deploy-{i}"), "OxiMux", NOW)).collect();
    let e = run(&snap(tabs, wts), "deploy");
    // Tabs win the tie → lead with 6, worktrees trail with 3.
    assert_eq!(e[0], Entry::Header(Section::Tabs));
    assert_eq!(count(&e, is_tab), 6);
    assert_eq!(count(&e, is_wt), 3);
    assert!(e.contains(&Entry::More { section: Section::Tabs, hidden: 4 }));
    assert!(e.contains(&Entry::More { section: Section::Worktrees, hidden: 7 }));
    assert_eq!(e.last(), Some(&Entry::CreateWorktree("deploy".into())));
}

#[test]
fn better_worktree_match_leads() {
    let s = snap(vec![tab(1, "release notes", "OxiMux", NOW)], vec![wt("r", "rel", "OxiMux", NOW)]);
    assert_eq!(run(&s, "rel")[0], Entry::Header(Section::Worktrees));
}

#[test]
fn worktree_name_hit_leads_tabs_matching_only_via_worktree() {
    // Every tab lives in worktree "main"; the worktree itself is named "main".
    let tabs = (0..3).map(|i| tab(i, &format!("Terminal {i}"), "OxiMux", NOW)).collect();
    let s = snap(tabs, vec![wt("m", "main", "OxiMux", NOW)]);
    assert_eq!(run(&s, "main")[0], Entry::Header(Section::Worktrees));
}

#[test]
fn filter_excludes_other_projects() {
    let s = snap(
        vec![tab(1, "a", "OxiMux", NOW), tab(2, "b", "Other", NOW)],
        vec![wt("x", "x", "OxiMux", NOW), wt("y", "y", "Other", NOW)],
    );
    let filter = ProjectFilter { project_ids: HashSet::from(["Other".to_string()]) };
    let e = layout(&s, &prepare(""), &filter, &Expansions::new());
    assert_eq!(items(&e), vec![ItemRef::Tab(1), ItemRef::Worktree(1)]);
}

#[test]
fn layout_is_fast_for_large_snapshots() {
    let tabs: Vec<_> = (0..500).map(|i| tab(i, &format!("agent session {i} refactor"), "OxiMux", NOW - i as i64)).collect();
    let wts: Vec<_> = (0..200).map(|i| wt(&format!("w{i}"), &format!("feat/branch-{i}"), "OxiMux", NOW)).collect();
    let s = snap(tabs, wts);
    let start = Instant::now();
    for q in ["", "re", "agent ref", "feat bra 1"] {
        let _ = run(&s, q);
    }
    // Generous bound: debug builds are ~10× slower than release.
    assert!(start.elapsed().as_millis() < 500, "layout too slow: {:?}", start.elapsed());
}
