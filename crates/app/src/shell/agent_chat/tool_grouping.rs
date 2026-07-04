//! Turn-grouping for the transcript: long runs of consecutive tool cards
//! collapse to a head + "N more" expander + tail, so a tool-heavy turn doesn't
//! flood the view. Pure and unit-testable — it decides per-entry visibility
//! from three parallel bool/index inputs and knows nothing about rendering.

use std::collections::HashSet;

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

#[cfg(test)]
mod tests {
    use super::*;

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
