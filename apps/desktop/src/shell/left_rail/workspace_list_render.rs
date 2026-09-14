//! The rail's workspace-list ordering: sort and group modes, their persisted
//! keys, and the pure `sort_workspaces` the list renderer calls. No GPUI
//! runtime needed.

use oximux_core::Workspace;

/// How workspace rows within a project group are ordered.
///
/// The primary row (the repo-root worktree) is always pinned first in every
/// mode — it's the project's anchor. The remaining worktree rows are ordered
/// per the active mode. The choice is persisted across restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceSortMode {
    /// Attention-weighted: workspaces needing action (approval / waiting) or
    /// running float above idle / finished ones. Stable within a tier.
    #[default]
    Smart,
    /// Most recently created first.
    Recent,
    /// Insertion order as stored — no reordering.
    Manual,
    /// Alphabetical by workspace display name (case-insensitive).
    Name,
    /// Ordered by owning project name, then workspace name. Within a single
    /// project group every row shares one project, so this falls back to the
    /// name ordering; the distinction only matters in the flat (ungrouped)
    /// list where rows from different projects intermingle.
    Project,
}

impl WorkspaceSortMode {
    /// Short human label shown in the display-options menu.
    pub fn label(self) -> &'static str {
        match self {
            WorkspaceSortMode::Smart => "Smart",
            WorkspaceSortMode::Recent => "Recent",
            WorkspaceSortMode::Manual => "Manual",
            WorkspaceSortMode::Name => "Name",
            WorkspaceSortMode::Project => "Project",
        }
    }

    /// Stable persistence key.
    pub fn as_key(self) -> &'static str {
        match self {
            WorkspaceSortMode::Smart => "smart",
            WorkspaceSortMode::Recent => "recent",
            WorkspaceSortMode::Manual => "manual",
            WorkspaceSortMode::Name => "name",
            WorkspaceSortMode::Project => "project",
        }
    }

    /// Parse a persisted key; unknown / missing values fall back to default.
    pub fn from_key(raw: &str) -> WorkspaceSortMode {
        match raw.trim() {
            "recent" => WorkspaceSortMode::Recent,
            "manual" => WorkspaceSortMode::Manual,
            "name" => WorkspaceSortMode::Name,
            "project" => WorkspaceSortMode::Project,
            _ => WorkspaceSortMode::Smart,
        }
    }

    /// All modes in menu-display order.
    pub const ALL: [WorkspaceSortMode; 5] = [
        WorkspaceSortMode::Name,
        WorkspaceSortMode::Smart,
        WorkspaceSortMode::Recent,
        WorkspaceSortMode::Project,
        WorkspaceSortMode::Manual,
    ];
}

/// Whether workspace rows are grouped under project headers or shown flat.
///
/// `Project` (default) nests rows under their owning project's collapsible
/// header — the current behaviour. `None` shows every workspace in a single
/// flat list ordered purely by the active `WorkspaceSortMode`, with no project
/// headers and no collapse affordance. The choice is persisted across restarts.
/// Named `Flat` (not `None`) so a future `use WorkspaceGroupMode::*` can't
/// shadow `Option::None`. The dropdown still labels it "None" for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkspaceGroupMode {
    /// Rows nested under their owning project group header.
    #[default]
    Project,
    /// All rows shown as one flat list; no project headers.
    Flat,
}

impl WorkspaceGroupMode {
    /// Stable persistence key.
    pub fn as_key(self) -> &'static str {
        match self {
            WorkspaceGroupMode::Project => "project",
            WorkspaceGroupMode::Flat => "flat",
        }
    }

    /// Parse a persisted key; unknown / missing values fall back to default.
    /// Accepts the legacy `"none"` spelling as well as `"flat"`.
    pub fn from_key(raw: &str) -> WorkspaceGroupMode {
        match raw.trim() {
            "flat" | "none" => WorkspaceGroupMode::Flat,
            _ => WorkspaceGroupMode::Project,
        }
    }
}

/// Order one project group's workspace rows for display.
///
/// Display order is `[primary] , [pinned…] , [unpinned…]`: the primary row
/// (`worktree_path == project_root`) always anchors first, then explicitly
/// pinned rows float above unpinned ones — in *every* mode — and the active
/// `mode` orders the rows *within* each of the pinned and unpinned groups.
/// `attention_for` resolves a row's attention tier (lower = higher priority) —
/// injected so this stays pure and free of the status / live-worktree maps.
/// Sorts are stable, so equal-tier / equal-timestamp rows keep their input
/// order.
///
/// Partition contract: if no row matches `project_root`, all rows fall into
/// the pinned/unpinned tail. If several rows match (not expected — the
/// synthesized primary is unique), all are pinned first in their input order.
pub fn sort_workspaces(
    workspaces: &[Workspace],
    project_root: &str,
    mode: WorkspaceSortMode,
    attention_for: impl Fn(&Workspace) -> u8,
) -> Vec<Workspace> {
    let (mut primary, rest): (Vec<Workspace>, Vec<Workspace>) = workspaces
        .iter()
        .cloned()
        .partition(|ws| ws.worktree_path == project_root);

    // Pinned rows float above unpinned ones regardless of mode; the active
    // mode orders within each group so a pin never scrambles the ordering the
    // user already sees.
    let (mut pinned, mut unpinned): (Vec<Workspace>, Vec<Workspace>) =
        rest.into_iter().partition(|ws| ws.pinned);

    let order_group = |list: &mut Vec<Workspace>| match mode {
        // Stable sort by ascending attention tier floats action-needing rows up.
        WorkspaceSortMode::Smart => list.sort_by_key(|ws| attention_for(ws)),
        // created_at is an RFC3339 UTC string; lexicographic desc = newest first.
        WorkspaceSortMode::Recent => list.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
        // Drag-assigned rank, ascending. `total_cmp` keeps the sort total even
        // if a rank is ever NaN (it never should be). Equal ranks keep input
        // order (stable sort).
        WorkspaceSortMode::Manual => list.sort_by(|a, b| a.sort_order.total_cmp(&b.sort_order)),
        // Case-insensitive alphabetical by display name. `Project` collapses to
        // the same within-group order because every row here shares one project.
        WorkspaceSortMode::Name | WorkspaceSortMode::Project => {
            list.sort_by_key(|a| a.name.to_lowercase())
        }
    };
    order_group(&mut pinned);
    order_group(&mut unpinned);

    primary.extend(pinned);
    primary.extend(unpinned);
    primary
}

#[cfg(test)]
mod tests {

    use super::*;

    // `created_at` literals use the `+00:00` offset suffix to match what the
    // storage layer actually writes (`Utc::now().to_rfc3339()`), so the
    // Recent-sort comparison is exercised against the real value domain.
    fn ws(id: &str, worktree_path: &str, created_at: &str) -> Workspace {
        Workspace {
            id: id.to_string(),
            project_id: "p1".to_string(),
            // Not a branch OxiMux minted: a synthesized row or a
            // fixture. `false` is the reading that never deletes.
            branch_minted: false,
            name: id.to_string(),
            slug: id.to_string(),
            branch: format!("oximux/{id}"),
            worktree_path: worktree_path.to_string(),
            status: "active".to_string(),
            created_at: created_at.to_string(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: String::new(),
        }
    }

    /// Like `ws` but with an explicit display name distinct from the id, for
    /// exercising name-based ordering.
    fn ws_named(id: &str, worktree_path: &str, name: &str, created_at: &str) -> Workspace {
        Workspace {
            name: name.to_string(),
            ..ws(id, worktree_path, created_at)
        }
    }

    #[test]
    fn sort_mode_round_trips_through_key() {
        for mode in WorkspaceSortMode::ALL {
            assert_eq!(WorkspaceSortMode::from_key(mode.as_key()), mode);
        }
    }

    #[test]
    fn sort_mode_unknown_key_falls_back_to_default() {
        assert_eq!(WorkspaceSortMode::from_key("bogus"), WorkspaceSortMode::Smart);
        assert_eq!(WorkspaceSortMode::default(), WorkspaceSortMode::Smart);
    }

    #[test]
    fn group_mode_round_trips_through_key() {
        assert_eq!(
            WorkspaceGroupMode::from_key(WorkspaceGroupMode::Project.as_key()),
            WorkspaceGroupMode::Project
        );
        assert_eq!(
            WorkspaceGroupMode::from_key(WorkspaceGroupMode::Flat.as_key()),
            WorkspaceGroupMode::Flat
        );
        // Legacy "none" spelling still parses to Flat.
        assert_eq!(WorkspaceGroupMode::from_key("none"), WorkspaceGroupMode::Flat);
        assert_eq!(
            WorkspaceGroupMode::from_key("bogus"),
            WorkspaceGroupMode::Project
        );
    }

    #[test]
    fn name_sort_is_case_insensitive_alphabetical_primary_pinned() {
        // Primary anchors first regardless of name; the rest sort
        // alphabetically without case sensitivity.
        let root = "/tmp/p1";
        let list = vec![
            ws_named("primary", root, "zzz", "2026-01-01T00:00:00+00:00"),
            ws_named("w1", "/tmp/p1/w1", "Beta", "2026-02-01T00:00:00+00:00"),
            ws_named("w2", "/tmp/p1/w2", "alpha", "2026-03-01T00:00:00+00:00"),
        ];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Name, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["primary", "w2", "w1"]);
    }

    #[test]
    fn manual_sort_equal_ranks_preserve_input_order() {
        // With all ranks equal (0.0), the stable sort keeps input order and
        // the primary is pinned first.
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("b", "/tmp/p1/b", "2026-03-01T00:00:00+00:00"),
            ws("a", "/tmp/p1/a", "2026-02-01T00:00:00+00:00"),
        ];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Manual, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["primary", "b", "a"]);
    }

    #[test]
    fn manual_sort_orders_by_sort_order_with_primary_pinned() {
        let root = "/tmp/p1";
        let mut primary = ws("primary", root, "2026-01-01T00:00:00+00:00");
        primary.sort_order = 99.0; // primary stays first regardless of rank
        let mut a = ws("a", "/tmp/p1/a", "2026-02-01T00:00:00+00:00");
        a.sort_order = 3.0;
        let mut b = ws("b", "/tmp/p1/b", "2026-03-01T00:00:00+00:00");
        b.sort_order = 1.0;
        let mut c = ws("c", "/tmp/p1/c", "2026-04-01T00:00:00+00:00");
        c.sort_order = 2.0;
        // Input deliberately out of rank order.
        let list = vec![primary, a, b, c];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Manual, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        // Primary pinned first; rest by ascending sort_order: b(1) c(2) a(3).
        assert_eq!(ids, ["primary", "b", "c", "a"]);
    }

    #[test]
    fn smart_sort_pins_primary_then_floats_by_attention() {
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("idle", "/tmp/p1/idle", "2026-02-01T00:00:00+00:00"),
            ws("needs", "/tmp/p1/needs", "2026-03-01T00:00:00+00:00"),
        ];
        // Tier 0 for "needs", tier 2 for everything else.
        let attention = |w: &Workspace| if w.id == "needs" { 0 } else { 2 };
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Smart, attention);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        // Primary stays first; "needs" floats above "idle".
        assert_eq!(ids, ["primary", "needs", "idle"]);
    }

    #[test]
    fn recent_sort_pins_primary_then_newest_first() {
        let root = "/tmp/p1";
        let list = vec![
            ws("primary", root, "2026-01-01T00:00:00+00:00"),
            ws("old", "/tmp/p1/old", "2026-02-01T00:00:00+00:00"),
            ws("new", "/tmp/p1/new", "2026-05-01T00:00:00+00:00"),
        ];
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Recent, |_| 0);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["primary", "new", "old"]);
    }

    #[test]
    fn pinned_rows_float_above_unpinned_in_every_mode() {
        let root = "/tmp/p1";
        let mut primary = ws("primary", root, "2026-01-01T00:00:00+00:00");
        primary.sort_order = 50.0;
        // "z" is pinned but otherwise sorts last in every mode (oldest, highest
        // manual rank, idle attention) — pinning must still float it to second.
        let mut z = ws("z", "/tmp/p1/z", "2026-01-02T00:00:00+00:00");
        z.sort_order = 9.0;
        z.pinned = true;
        let mut a = ws("a", "/tmp/p1/a", "2026-05-01T00:00:00+00:00");
        a.sort_order = 1.0;
        let mut b = ws("b", "/tmp/p1/b", "2026-04-01T00:00:00+00:00");
        b.sort_order = 2.0;
        let list = vec![primary, a, b, z];

        // Manual: primary, then pinned group (z), then unpinned by rank (a,b).
        let manual = sort_workspaces(&list, root, WorkspaceSortMode::Manual, |_| 0);
        assert_eq!(
            manual.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["primary", "z", "a", "b"]
        );

        // Recent: primary, pinned (z), then unpinned newest-first (a newer than b).
        let recent = sort_workspaces(&list, root, WorkspaceSortMode::Recent, |_| 0);
        assert_eq!(
            recent.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["primary", "z", "a", "b"]
        );

        // Smart: pinned floats up even though its attention tier is worst.
        let attention = |w: &Workspace| if w.id == "a" { 0 } else { 2 };
        let smart = sort_workspaces(&list, root, WorkspaceSortMode::Smart, attention);
        assert_eq!(smart.first().map(|w| w.id.as_str()), Some("primary"));
        assert_eq!(smart.get(1).map(|w| w.id.as_str()), Some("z"));
    }

    #[test]
    fn multiple_pinned_rows_keep_within_group_mode_order() {
        let root = "/tmp/p1";
        let primary = ws("primary", root, "2026-01-01T00:00:00+00:00");
        let mut p_old = ws("p_old", "/tmp/p1/po", "2026-02-01T00:00:00+00:00");
        p_old.pinned = true;
        let mut p_new = ws("p_new", "/tmp/p1/pn", "2026-06-01T00:00:00+00:00");
        p_new.pinned = true;
        let u = ws("u", "/tmp/p1/u", "2026-03-01T00:00:00+00:00");
        let list = vec![primary, p_old, p_new, u];
        // Recent: within the pinned group, newest first → p_new before p_old.
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Recent, |_| 0);
        assert_eq!(
            out.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(),
            ["primary", "p_new", "p_old", "u"]
        );
    }

    #[test]
    fn smart_sort_without_primary_still_orders_rest() {
        // Defensive: if no row matches the root, all are treated as rest.
        let root = "/tmp/p1";
        let list = vec![
            ws("idle", "/tmp/p1/idle", "2026-02-01T00:00:00+00:00"),
            ws("needs", "/tmp/p1/needs", "2026-03-01T00:00:00+00:00"),
        ];
        let attention = |w: &Workspace| if w.id == "needs" { 0 } else { 2 };
        let out = sort_workspaces(&list, root, WorkspaceSortMode::Smart, attention);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["needs", "idle"]);
    }
}
