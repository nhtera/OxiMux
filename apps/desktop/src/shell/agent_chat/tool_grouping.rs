//! Turn-grouping for the transcript: long runs of consecutive tool cards
//! collapse to a head + "N more" expander + tail, so a tool-heavy turn doesn't
//! flood the view. Pure and unit-testable — it decides per-entry visibility
//! from three parallel bool/index inputs and knows nothing about rendering, and
//! summarizes what a collapsed run did ([`summarize_tool_run`]) so the expander
//! reads as work ("Edited 3 files · ran 2 commands") rather than a bare count.

use std::collections::HashSet;

use oximux_agents::thread::ToolDetail;

/// Runs of >8 consecutive tool cards collapse to first-3 + "N more" + last-2.
const TOOL_RUN_COLLAPSE_THRESHOLD: usize = 8;
const TOOL_RUN_HEAD: usize = 3;
const TOOL_RUN_TAIL: usize = 2;

/// Per-entry render decision for the turn-grouping pass.
#[derive(Debug, PartialEq)]
pub(super) enum EntryDisplay {
    /// Render this entry normally.
    Show,
    /// Skip this entry (collapsed inside a long tool run).
    Hide,
    /// Render this entry, then an expander for a collapsed run of `hidden`
    /// entries, keyed by `run_start` (the run's first entry index).
    ShowThenExpander { run_start: usize, hidden: usize },
}

/// Decide, for each transcript entry, whether it shows / hides / is followed by
/// an expander. `is_tool`/`force_show` are parallel to the entry list;
/// `force_show[i]` marks a tool card that must stay visible (pending / failed).
/// `expanded` holds the run-start indices the user has opened.
pub(super) fn plan_tool_grouping(
    is_tool: &[bool],
    force_show: &[bool],
    expanded: &HashSet<usize>,
) -> Vec<EntryDisplay> {
    let n = is_tool.len();
    let mut plan: Vec<EntryDisplay> = (0..n).map(|_| EntryDisplay::Show).collect();
    let mut i = 0;
    while i < n {
        if !is_tool[i] {
            i += 1;
            continue;
        }
        // Maximal run of consecutive tool entries [i, j).
        let mut j = i;
        while j < n && is_tool[j] {
            j += 1;
        }
        let len = j - i;
        if len > TOOL_RUN_COLLAPSE_THRESHOLD && !expanded.contains(&i) {
            let mut hidden = 0;
            let mut first_hidden = None;
            for (k, slot) in plan.iter_mut().enumerate().take(j).skip(i) {
                let pos = k - i;
                let in_head = pos < TOOL_RUN_HEAD;
                let in_tail = pos >= len - TOOL_RUN_TAIL;
                if in_head || in_tail || force_show[k] {
                    *slot = EntryDisplay::Show;
                } else {
                    *slot = EntryDisplay::Hide;
                    hidden += 1;
                    first_hidden.get_or_insert(k);
                }
            }
            // Anchor the expander to the SHOWN entry immediately before the
            // first hidden one, so "N more" sits directly above the collapsed
            // block rather than above a fully-visible head card. `first_hidden`
            // is always past the head (head positions are force-shown), so the
            // prior index is a shown entry.
            if let (Some(fh), true) = (first_hidden, hidden > 0) {
                plan[fh - 1] = EntryDisplay::ShowThenExpander { run_start: i, hidden };
            }
        }
        i = j;
    }
    plan
}

/// Verb clauses shown before the label elides — three reads as a summary, more
/// reads as the log the collapse exists to avoid.
const MAX_VERB_CLAUSES: usize = 3;

/// One collapsed tool card's contribution to its run's summary label.
pub(super) struct GroupedTool {
    pub kind: ToolDetail,
    pub failed: bool,
    /// The card's target (a file path for the edit family). Only used to count
    /// DISTINCT files — three edits to one file are one file edited.
    pub target: Option<String>,
}

/// What a collapsed run of tool cards did.
pub(super) struct GroupSummary {
    /// e.g. `Edited 3 files · ran 2 commands · 1 search`. Empty when the run has
    /// nothing to describe, in which case the caller keeps its bare count.
    pub label: String,
    /// Failed cards in the run — rendered in the error tint by the caller, so it
    /// stays out of `label`.
    pub failed: usize,
}

/// The archetypes that share one label bucket. Grouping is by the NOUN the
/// clause would use, so two kinds that both read "tool calls" count together
/// instead of producing two clauses with the same noun.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum Verb {
    Edit,
    Run,
    Read,
    Search,
    Lookup,
    Agent,
    Other,
}

impl Verb {
    fn of(kind: ToolDetail) -> Verb {
        match kind {
            ToolDetail::Edit | ToolDetail::Write => Verb::Edit,
            ToolDetail::Shell => Verb::Run,
            ToolDetail::Read => Verb::Read,
            ToolDetail::Search => Verb::Search,
            ToolDetail::Fetch | ToolDetail::WebSearch => Verb::Lookup,
            ToolDetail::SubAgent => Verb::Agent,
            ToolDetail::Mcp | ToolDetail::Plan | ToolDetail::Plain | ToolDetail::Unknown => {
                Verb::Other
            }
        }
    }

    /// The clause for `n` occurrences, lowercase (the caller capitalizes the
    /// leading one). Reads as a past-tense verb phrase where there is a natural
    /// one, and as a plain count where there isn't.
    fn clause(self, n: usize) -> String {
        let plural = |s: &str| if n == 1 { s.to_string() } else { format!("{s}s") };
        match self {
            Verb::Edit => format!("edited {n} {}", plural("file")),
            Verb::Run => format!("ran {n} {}", plural("command")),
            Verb::Read => format!("read {n} {}", plural("file")),
            Verb::Search => format!("{n} {}", plural("search")),
            Verb::Lookup => format!("{n} {}", plural("lookup")),
            Verb::Agent => format!("{n} {}", plural("agent")),
            Verb::Other => format!("{n} tool {}", plural("call")),
        }
    }
}

/// Summarize a collapsed run by what it did, most-numerous clause first.
///
/// The edit clause counts DISTINCT targets — a run that edits one file five
/// times edited one file, and saying "5 files" would be a lie the user can't
/// check without expanding. Every other verb counts calls, which is what its
/// noun already means. Ties break by verb order (stable across renders).
pub(super) fn summarize_tool_run(tools: &[GroupedTool]) -> GroupSummary {
    let failed = tools.iter().filter(|t| t.failed).count();
    let order = [Verb::Edit, Verb::Run, Verb::Read, Verb::Search, Verb::Lookup, Verb::Agent, Verb::Other];
    let mut counts: Vec<(Verb, usize)> = Vec::new();
    for verb in order {
        let group = tools.iter().filter(|t| Verb::of(t.kind) == verb);
        let n = if verb == Verb::Edit {
            let mut paths: Vec<&str> = group
                .filter_map(|t| t.target.as_deref())
                .collect();
            paths.sort_unstable();
            paths.dedup();
            // An edit card with no target still edited something — count it, or a
            // run of untargeted edits would vanish from its own summary.
            let untargeted =
                tools.iter().filter(|t| Verb::of(t.kind) == verb && t.target.is_none()).count();
            paths.len() + untargeted
        } else {
            group.count()
        };
        if n > 0 {
            counts.push((verb, n));
        }
    }
    // Most-numerous first; `sort_by_key` is stable, so the verb order above
    // breaks ties the same way on every repaint.
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));

    let mut clauses: Vec<String> = counts
        .iter()
        .take(MAX_VERB_CLAUSES)
        .map(|(v, n)| v.clause(*n))
        .collect();
    if counts.len() > MAX_VERB_CLAUSES {
        clauses.push("…".to_string());
    }
    let mut label = clauses.join(" · ");
    // Sentence-case the leading clause; the rest stay lowercase so the line
    // reads as one phrase rather than a row of headings.
    if let Some(first) = label.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    GroupSummary { label, failed }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grouped card of `kind` targeting `target`.
    fn t(kind: ToolDetail, target: Option<&str>) -> GroupedTool {
        GroupedTool { kind, failed: false, target: target.map(str::to_string) }
    }

    #[test]
    fn summary_reads_as_verb_phrases_with_counts() {
        let s = summarize_tool_run(&[
            t(ToolDetail::Edit, Some("a.rs")),
            t(ToolDetail::Edit, Some("b.rs")),
            t(ToolDetail::Write, Some("c.rs")),
            t(ToolDetail::Shell, None),
            t(ToolDetail::Shell, None),
            t(ToolDetail::Search, None),
        ]);
        assert_eq!(s.label, "Edited 3 files · ran 2 commands · 1 search");
        assert_eq!(s.failed, 0);
    }

    #[test]
    fn edits_count_distinct_files_not_calls() {
        // Five edits to one file edited ONE file.
        let s = summarize_tool_run(&[
            t(ToolDetail::Edit, Some("a.rs")),
            t(ToolDetail::Edit, Some("a.rs")),
            t(ToolDetail::Edit, Some("a.rs")),
        ]);
        assert_eq!(s.label, "Edited 1 file");
        // An untargeted edit still counts — it edited something.
        let s = summarize_tool_run(&[t(ToolDetail::Edit, Some("a.rs")), t(ToolDetail::Edit, None)]);
        assert_eq!(s.label, "Edited 2 files");
    }

    #[test]
    fn singular_and_plural_read_correctly() {
        assert_eq!(summarize_tool_run(&[t(ToolDetail::Shell, None)]).label, "Ran 1 command");
        assert_eq!(summarize_tool_run(&[t(ToolDetail::Read, Some("a"))]).label, "Read 1 file");
        assert_eq!(summarize_tool_run(&[t(ToolDetail::Search, None)]).label, "1 search");
        assert_eq!(summarize_tool_run(&[t(ToolDetail::SubAgent, None)]).label, "1 agent");
        assert_eq!(summarize_tool_run(&[t(ToolDetail::Fetch, None)]).label, "1 lookup");
        assert_eq!(summarize_tool_run(&[t(ToolDetail::Unknown, None)]).label, "1 tool call");
    }

    #[test]
    fn mcp_and_unknown_share_one_clause() {
        // Both read "tool calls" — two clauses with the same noun would be noise.
        let s = summarize_tool_run(&[
            t(ToolDetail::Mcp, None),
            t(ToolDetail::Unknown, None),
            t(ToolDetail::Plan, None),
        ]);
        assert_eq!(s.label, "3 tool calls");
    }

    #[test]
    fn failed_cards_are_counted_apart_from_the_label() {
        let mut tools = vec![t(ToolDetail::Shell, None), t(ToolDetail::Shell, None)];
        tools[1].failed = true;
        let s = summarize_tool_run(&tools);
        // The failure count is the caller's to tint, so it stays out of `label`…
        assert_eq!(s.label, "Ran 2 commands");
        // …but a failed card still counts as work that happened.
        assert_eq!(s.failed, 1);
    }

    #[test]
    fn label_orders_by_count_and_elides_past_three_kinds() {
        let s = summarize_tool_run(&[
            t(ToolDetail::Search, None),
            t(ToolDetail::Shell, None),
            t(ToolDetail::Shell, None),
            t(ToolDetail::Read, Some("a")),
            t(ToolDetail::Read, Some("b")),
            t(ToolDetail::Read, Some("c")),
            t(ToolDetail::SubAgent, None),
        ]);
        // read(3) > ran(2) > search(1) == agent(1) → ties break by verb order,
        // and the fourth kind elides rather than extending the line.
        assert_eq!(s.label, "Read 3 files · ran 2 commands · 1 search · …");
    }

    #[test]
    fn an_empty_run_summarizes_to_nothing() {
        let s = summarize_tool_run(&[]);
        assert!(s.label.is_empty(), "no cards, nothing to say");
        assert_eq!(s.failed, 0);
    }

    #[test]
    fn below_threshold_shows_all() {
        let is_tool = vec![true; 8];
        let force = vec![false; 8];
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        assert!(plan.iter().all(|d| matches!(d, EntryDisplay::Show)));
    }

    #[test]
    fn collapses_long_run_head_and_tail() {
        // 10 tool cards → show 3 head + 2 tail, hide 5, expander after entry 2.
        let is_tool = vec![true; 10];
        let force = vec![false; 10];
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        let shown: Vec<usize> = (0..10)
            .filter(|&i| !matches!(plan[i], EntryDisplay::Hide))
            .collect();
        assert_eq!(shown, vec![0, 1, 2, 8, 9], "first 3 + last 2 visible");
        match plan[2] {
            EntryDisplay::ShowThenExpander { run_start, hidden } => {
                assert_eq!(run_start, 0);
                assert_eq!(hidden, 5);
            }
            ref other => panic!("expected expander at index 2, got {other:?}"),
        }
    }

    #[test]
    fn keeps_pending_visible_and_expands() {
        // A pending card in the collapsed middle stays visible.
        let is_tool = vec![true; 12];
        let mut force = vec![false; 12];
        force[6] = true; // a pending card mid-run
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        assert!(!matches!(plan[6], EntryDisplay::Hide), "pending card forced visible");
        // Expander anchors at the SHOWN entry right before the first hidden one
        // (index 2 = last head). First hidden is index 3, so anchor is 2.
        match plan[2] {
            EntryDisplay::ShowThenExpander { run_start, .. } => assert_eq!(run_start, 0),
            ref other => panic!("expected expander at index 2, got {other:?}"),
        }
        assert!(matches!(plan[3], EntryDisplay::Hide), "first hidden right after the expander");

        // Expanding the run shows everything.
        let mut expanded = HashSet::new();
        expanded.insert(0);
        let plan = plan_tool_grouping(&is_tool, &force, &expanded);
        assert!(plan.iter().all(|d| matches!(d, EntryDisplay::Show)), "expanded run shows all");
    }

    #[test]
    fn leaves_short_runs_and_messages_alone() {
        // messages interleaved with short tool runs: nothing collapses.
        let is_tool = vec![false, true, true, false, true, false];
        let force = vec![false; 6];
        let plan = plan_tool_grouping(&is_tool, &force, &HashSet::new());
        assert!(plan.iter().all(|d| matches!(d, EntryDisplay::Show)));
    }
}
