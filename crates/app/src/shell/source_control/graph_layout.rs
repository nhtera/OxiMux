//! Pure swimlane layout for the commit graph (the DAG drawing).
//!
//! Turns an ordered (newest-first) slice of commits — each carrying its
//! parent SHAs — into a per-row drawing model: which lane (column) the
//! commit's node sits in, the lines entering that node from above, the
//! lines leaving it below, and the pass-through lanes that skip the row
//! entirely. The renderer in `graph_gutter.rs` consumes this model and
//! paints; this module does no drawing and no GPUI work, so the whole
//! lane assignment is exercised by ordinary unit tests.
//!
//! The algorithm is the standard incremental swimlane walk: process
//! commits top-to-bottom (child before parent), carrying a vector of
//! "active lanes" where each lane tracks the SHA it is waiting to reach.
//! A commit consumes every active lane pointing at it (those lines fold
//! into its node), then its first parent re-occupies the node's column
//! while any extra (merge) parents open fresh lanes to the right. Lanes
//! whose commit isn't this row simply pass straight through.
//!
//! A lane carries a colour *index* (0..[`LANE_COLORS`]) rather than a
//! resolved colour: the first parent inherits its lane's colour so a
//! branch keeps one hue down its whole length, while each newly opened
//! lane rotates to the next palette slot. The renderer maps the index
//! through `Theme::graph_lane_colors` at paint time.

use oximux_core::CommitInfo;

/// Number of distinct lane hues (matches `Theme::graph_lane_colors`).
/// Lane colour index is always taken modulo this.
pub const LANE_COLORS: usize = 5;

/// Hard cap on lanes drawn in the gutter. A pathological merge-heavy
/// window could otherwise open dozens of columns and push the commit
/// text off the panel; lanes past the cap still participate in the
/// layout (so colours/positions stay correct for the ones shown) but the
/// gutter width — and thus the painted area — is clamped here. 12 lanes
/// is far past what a readable sidebar graph ever needs.
pub const MAX_LANES: usize = 12;

/// One active column in the swimlane walk: the SHA this lane is currently
/// tracking plus its palette colour index.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Lane {
    id: String,
    color: u8,
}

/// A line that spans the full row height, entering at `top_col` and
/// leaving at `bottom_col` (equal columns ⇒ a straight vertical; differing
/// columns ⇒ an S-curve as the lane shifts sideways).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PassEdge {
    pub top_col: usize,
    pub bottom_col: usize,
    pub color: u8,
}

/// A half-height line touching the commit's node. For an *in* edge it runs
/// from `col` at the top of the row into the node at mid-height; for an
/// *out* edge it runs from the node at mid-height out to `col` at the
/// bottom of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeEdge {
    pub col: usize,
    pub color: u8,
}

/// The full drawing model for one commit row.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowLayout {
    /// Column the commit's node circle sits in.
    pub node_lane: usize,
    /// Palette colour index for the node circle and its outgoing stem.
    pub node_color: u8,
    /// `true` when the commit has two or more parents (a merge) — the
    /// renderer draws a ring instead of a solid dot.
    pub is_merge: bool,
    /// Lanes passing straight through this row (neither starting nor
    /// ending at the node).
    pub passthrough: Vec<PassEdge>,
    /// Lines folding into the node from above (the commit's children /
    /// converging branches).
    pub node_in: Vec<NodeEdge>,
    /// Lines leaving the node downward — the first-parent continuation
    /// plus one per extra merge parent.
    pub node_out: Vec<NodeEdge>,
    /// Lane count this row occupies (`max(inputs, outputs)`), before the
    /// [`MAX_LANES`] clamp. Drives the gutter width.
    pub lanes: usize,
}

/// Positive modulo — keeps the rotating colour cursor in `0..modulo`
/// regardless of sign. Mirrors VS Code's `rot()`.
fn rot(index: i32, modulo: i32) -> u8 {
    (((index % modulo) + modulo) % modulo) as u8
}

/// Build the per-row swimlane layout for `commits` (newest-first, each
/// with its parent SHAs populated). The returned vec is 1:1 with the
/// input so the renderer can index it by row.
///
/// Parents that fall outside the loaded window (older than the last
/// commit) simply leave their lanes hanging off the bottom edge of the
/// final rows — exactly what git history graphs show when paginated.
pub fn compute_graph(commits: &[CommitInfo]) -> Vec<RowLayout> {
    let mut rows = Vec::with_capacity(commits.len());
    // Active lanes entering the current row (== previous row's outputs).
    let mut input: Vec<Lane> = Vec::new();
    // Rotating palette cursor; -1 so the first new lane lands on slot 0.
    let mut color_cursor: i32 = -1;

    for c in commits {
        let mut output: Vec<Lane> = Vec::new();
        let mut node_in: Vec<NodeEdge> = Vec::new();
        let mut passthrough: Vec<PassEdge> = Vec::new();
        let mut first_parent_added = false;

        // Pass 1 — walk existing lanes. Lanes pointing at this commit fold
        // into its node; the first such lane is replaced by the commit's
        // first parent (same colour, so the branch keeps its hue). Other
        // lanes pass straight through, keeping their relative order.
        for (i, lane) in input.iter().enumerate() {
            if lane.id == c.oid {
                node_in.push(NodeEdge { col: i, color: lane.color });
                if !first_parent_added && !c.parents.is_empty() {
                    output.push(Lane {
                        id: c.parents[0].clone(),
                        color: lane.color,
                    });
                    first_parent_added = true;
                }
                // Further lanes pointing at the same commit (diamond
                // convergence) drop out — their fold-in edge is already
                // recorded above and they leave no output lane.
            } else {
                let bottom_col = output.len();
                output.push(lane.clone());
                passthrough.push(PassEdge {
                    top_col: i,
                    bottom_col,
                    color: lane.color,
                });
            }
        }

        // The node sits in the column of the first lane that reached it, or
        // — for a branch tip not tracked by any child in view — in a fresh
        // column appended at the right.
        let node_lane = node_in.first().map(|e| e.col).unwrap_or(input.len());

        // Pass 2 — open lanes for parents not yet placed. For a tip this
        // includes the first parent (start = 0); for an in-history commit
        // it's only the extra merge parents (start = 1). A parent that
        // already has an open lane (the merge re-joins a branch still in
        // view) reuses it rather than opening a duplicate.
        let start = usize::from(first_parent_added);
        for parent in c.parents.iter().skip(start) {
            if output.iter().any(|l| &l.id == parent) {
                continue;
            }
            color_cursor = i32::from(rot(color_cursor + 1, LANE_COLORS as i32));
            output.push(Lane {
                id: parent.clone(),
                color: color_cursor as u8,
            });
        }

        // Resolve the node's colour from where its first parent landed
        // (its hue, inherited or freshly rotated), falling back to the
        // incoming lane for a parentless commit that still had children.
        let node_color = output
            .get(node_lane)
            .map(|l| l.color)
            .or_else(|| node_in.first().map(|e| e.color))
            .unwrap_or(0);

        // Outgoing edges: first the straight first-parent stem at the
        // node's own column, then one fan-out per merge parent. We scan
        // the output lanes for each parent SHA so the edge lands on that
        // parent's actual column (which may be a reused lane).
        let mut node_out: Vec<NodeEdge> = Vec::new();
        if !c.parents.is_empty() {
            if let Some(lane) = output.get(node_lane)
                && lane.id == c.parents[0]
            {
                node_out.push(NodeEdge {
                    col: node_lane,
                    color: lane.color,
                });
            }
            for parent in c.parents.iter().skip(1) {
                if let Some(col) = output.iter().position(|l| &l.id == parent) {
                    // Skip the node's own column — already emitted as the
                    // first-parent stem when first==this parent.
                    if col == node_lane && node_out.iter().any(|e| e.col == col) {
                        continue;
                    }
                    node_out.push(NodeEdge {
                        col,
                        color: output[col].color,
                    });
                }
            }
        }

        rows.push(RowLayout {
            node_lane,
            node_color,
            is_merge: c.parents.len() > 1,
            passthrough,
            node_in,
            node_out,
            lanes: input.len().max(output.len()).max(1),
        });

        input = output;
    }

    rows
}

/// Widest lane count across `rows`, clamped to [`MAX_LANES`]. The gutter
/// renders at one fixed width (this value) for every row so the commit
/// text to its right stays vertically aligned instead of jittering as the
/// graph narrows and widens.
pub fn max_lanes(rows: &[RowLayout]) -> usize {
    rows.iter().map(|r| r.lanes).max().unwrap_or(1).min(MAX_LANES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_core::CommitInfo;

    /// Terse commit builder: id + parents. Other fields are irrelevant to
    /// the layout so they're left blank.
    fn commit(id: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            oid: id.to_string(),
            short_oid: id.chars().take(7).collect(),
            subject: String::new(),
            author: String::new(),
            short_date: String::new(),
            body: String::new(),
            refs: Vec::new(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn linear_history_is_one_lane() {
        // C -> B -> A : a single column, each node continuing straight down.
        let commits = [
            commit("C", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ];
        let rows = compute_graph(&commits);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert_eq!(r.node_lane, 0, "linear history stays in lane 0");
            assert_eq!(r.lanes, 1);
            assert!(r.passthrough.is_empty(), "no lanes skip a linear row");
            assert!(!r.is_merge);
        }
        // The tip has nothing entering from above; the root has nothing
        // leaving below.
        assert!(rows[0].node_in.is_empty(), "tip has no incoming line");
        assert_eq!(rows[0].node_out.len(), 1, "tip continues to its parent");
        assert_eq!(rows[1].node_in.len(), 1);
        assert_eq!(rows[1].node_out.len(), 1);
        assert!(rows[2].node_out.is_empty(), "root has no outgoing line");
    }

    #[test]
    fn linear_history_keeps_one_color() {
        let commits = [
            commit("C", &["B"]),
            commit("B", &["A"]),
            commit("A", &[]),
        ];
        let rows = compute_graph(&commits);
        let c0 = rows[0].node_color;
        assert!(rows.iter().all(|r| r.node_color == c0), "one branch, one hue");
    }

    #[test]
    fn merge_opens_and_closes_a_second_lane() {
        // M merges A and B; both descend from root X.
        //   M (A, B)
        //   A (X)
        //   B (X)
        //   X ()
        let commits = [
            commit("M", &["A", "B"]),
            commit("A", &["X"]),
            commit("B", &["X"]),
            commit("X", &[]),
        ];
        let rows = compute_graph(&commits);

        // Row M: node in lane 0, two outgoing edges (to A in lane 0, B in lane 1).
        assert_eq!(rows[0].node_lane, 0);
        assert!(rows[0].is_merge);
        assert_eq!(rows[0].node_out.len(), 2, "merge fans to both parents");
        assert_eq!(rows[0].lanes, 2, "second lane opens at the merge");

        // Row A sits in lane 0; lane 1 (B) passes straight through.
        assert_eq!(rows[1].node_lane, 0);
        assert_eq!(rows[1].passthrough.len(), 1, "B's lane skips row A");
        assert_eq!(rows[1].passthrough[0].top_col, 1);

        // Row B sits in lane 1.
        assert_eq!(rows[2].node_lane, 1, "B took the second lane");

        // Row X: both lanes converged onto X — two lines fold in, one
        // (root) leaves below, and the graph collapses back to a single lane.
        assert_eq!(rows[3].node_in.len(), 2, "both branches fold into the root");
        assert!(rows[3].node_out.is_empty(), "root has no parent");
    }

    #[test]
    fn merge_parents_get_distinct_colors() {
        let commits = [
            commit("M", &["A", "B"]),
            commit("A", &["X"]),
            commit("B", &["X"]),
            commit("X", &[]),
        ];
        let rows = compute_graph(&commits);
        // The two outgoing edges from the merge must differ in hue so the
        // branches read apart.
        let colors: Vec<u8> = rows[0].node_out.iter().map(|e| e.color).collect();
        assert_eq!(colors.len(), 2);
        assert_ne!(colors[0], colors[1], "merge parents take different lanes/hues");
    }

    #[test]
    fn parents_outside_window_leave_lanes_hanging() {
        // Only the merge and one side are loaded; the other parent and the
        // shared ancestor are below the page boundary. The layout must not
        // panic and must keep the open lane's column stable.
        let commits = [commit("M", &["A", "B"]), commit("A", &["Z"])];
        let rows = compute_graph(&commits);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].node_out.len(), 2);
        // Row A in lane 0; B's lane still hangs in column 1.
        assert_eq!(rows[1].node_lane, 0);
        assert!(
            rows[1].passthrough.iter().any(|p| p.bottom_col == 1),
            "the unresolved merge lane keeps flowing off the bottom"
        );
    }

    #[test]
    fn branch_tip_not_in_view_opens_fresh_lane() {
        // Two unrelated tips loaded (no child references them): each opens
        // its own lane with its own colour.
        let commits = [
            commit("P", &["Q"]),
            commit("R", &["S"]),
            commit("Q", &[]),
            commit("S", &[]),
        ];
        let rows = compute_graph(&commits);
        // P opens lane 0; R is unrelated so it opens lane 1 (P's lane to Q
        // passes through R's row).
        assert_eq!(rows[0].node_lane, 0);
        assert_eq!(rows[1].node_lane, 1, "second unrelated tip takes a new lane");
        assert_ne!(
            rows[0].node_color, rows[1].node_color,
            "unrelated branches get distinct hues"
        );
    }

    #[test]
    fn max_lanes_is_clamped() {
        let commits = [commit("A", &[])];
        let rows = compute_graph(&commits);
        assert_eq!(max_lanes(&rows), 1);
    }

    #[test]
    fn rot_wraps_positively() {
        assert_eq!(rot(0, 5), 0);
        assert_eq!(rot(5, 5), 0);
        assert_eq!(rot(6, 5), 1);
        assert_eq!(rot(-1, 5), 4);
    }
}
