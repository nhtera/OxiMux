//! Turning "these pids are listening" into "this project is serving this".
//!
//! The kernel answers about pids. A person thinks about projects. Everything
//! in this file is the join between those two, kept pure so that the rules —
//! which port is shown, whose it is, what happens when two terminals in one
//! project both contain it — are decided by tests rather than by whatever the
//! render pass happened to do.
//!
//! One function here is not pure: [`gather`], the syscall bridge, which walks
//! process trees and reads the socket table. It sits beside the rules it feeds
//! rather than in the view, because what it produces is only meaningful
//! against them — but it is the *only* thing in this file that touches the
//! kernel, and it is called from a background thread. Everything else is a
//! function of its arguments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oximux_proc_ports::ListeningPort;

/// One terminal's process tree, flattened.
///
/// Carries names as well as pids because the names come from the same walk. A
/// second lookup at attribution time would be a second round of syscalls for
/// something already in hand — and would be asking about a pid that may have
/// exited in between, which is how a row acquires a blank process name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeSnapshot {
    /// Working directory of the pane group the terminal belongs to. This is
    /// the grain a person recognises: they started the server "in OxiMux",
    /// not "in pid 21044".
    pub project: PathBuf,
    /// The shell, then its descendants. Bounded by the tree walk itself.
    pub procs: Vec<(u32, String)>,
}

/// A listening port, attributed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortRow {
    pub port: u16,
    pub pid: u32,
    /// Executable name of the listening process, or empty when the walk saw
    /// the pid but not a name for it.
    pub process: String,
    pub loopback: bool,
}

/// Every port found under one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortGroup {
    pub project: PathBuf,
    pub rows: Vec<PortRow>,
}

/// What the panel draws: ports grouped by the project they were started in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortInventory {
    pub groups: Vec<PortGroup>,
}

impl PortInventory {
    /// Total rows across every group — the status bar's metric.
    pub fn total(&self) -> usize {
        self.groups.iter().map(|g| g.rows.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Every pid in `trees`, deduplicated — the set to ask the kernel about.
pub fn candidate_pids(trees: &[TreeSnapshot]) -> Vec<u32> {
    let mut pids: Vec<u32> = trees
        .iter()
        .flat_map(|t| t.procs.iter().map(|(pid, _)| *pid))
        .collect();
    pids.sort_unstable();
    pids.dedup();
    pids
}

/// Group `ports` by the project whose tree owns each one.
///
/// A port with no owning tree is dropped rather than shown as unattributed.
/// The pid set handed to the kernel came *from* these trees, so a port
/// arriving without one means the process exited mid-scan — and a row that
/// says "something, somewhere, is listening on 3000" helps nobody.
///
/// Projects with nothing listening are omitted entirely: an empty group is a
/// heading with nothing under it, which reads as a bug.
pub fn attribute(trees: &[TreeSnapshot], ports: &[ListeningPort]) -> PortInventory {
    // pid → (project, process name). Built once; a port is then a lookup.
    let mut owner: HashMap<u32, (&Path, &str)> = HashMap::new();
    for tree in trees {
        for (pid, name) in &tree.procs {
            // First tree wins. A pid can only be reached twice when one
            // terminal's tree contains another's, and in that case the
            // outermost is the one the user started — so insertion order,
            // which is pane order, is the answer rather than a coin toss.
            owner
                .entry(*pid)
                .or_insert((tree.project.as_path(), name.as_str()));
        }
    }

    let mut by_project: Vec<(&Path, Vec<PortRow>)> = Vec::new();
    for port in ports {
        let Some(&(project, process)) = owner.get(&port.pid) else {
            continue;
        };
        let row = PortRow {
            port: port.port,
            pid: port.pid,
            process: process.to_string(),
            loopback: port.loopback,
        };
        match by_project.iter_mut().find(|(p, _)| *p == project) {
            Some((_, rows)) => rows.push(row),
            None => by_project.push((project, vec![row])),
        }
    }

    let mut groups: Vec<PortGroup> = by_project
        .into_iter()
        .map(|(project, mut rows)| {
            rows.sort_by_key(|r| (r.port, r.pid));
            PortGroup {
                project: project.to_path_buf(),
                rows,
            }
        })
        .collect();
    // Stable heading order: the panel is re-rendered every poll, and a list
    // whose sections reshuffle under the cursor is unusable.
    groups.sort_by(|a, b| a.project.cmp(&b.project));
    PortInventory { groups }
}

/// Walk `roots` down to their listening ports. **Blocking — background
/// executor only.**
///
/// `roots` is `(project working directory, terminal shell pid)`, as
/// `ProjectPanes::terminal_roots` reports it.
///
/// The root itself is included in its own tree, not just its descendants: a
/// terminal whose command replaced the shell outright — `oximux run npm start`
/// rather than a shell that then ran it — has the listener at the root, and
/// walking only downward would miss exactly the case a user is most likely to
/// have set up on purpose.
pub fn gather(roots: Vec<(PathBuf, u32)>) -> PortInventory {
    let trees: Vec<TreeSnapshot> = roots
        .into_iter()
        .map(|(project, root)| {
            let mut procs: Vec<(u32, String)> = oximux_proc_tree::process(root)
                .into_iter()
                .chain(oximux_proc_tree::descendants(root))
                .map(|p| (p.pid, p.name))
                .collect();
            // A shell whose name the kernel would not give up is still a pid
            // worth asking the socket table about.
            if procs.is_empty() {
                procs.push((root, String::new()));
            }
            TreeSnapshot { project, procs }
        })
        .collect();
    let ports = oximux_proc_ports::listening_ports_of(&candidate_pids(&trees));
    attribute(&trees, &ports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(project: &str, procs: &[(u32, &str)]) -> TreeSnapshot {
        TreeSnapshot {
            project: PathBuf::from(project),
            procs: procs.iter().map(|(p, n)| (*p, n.to_string())).collect(),
        }
    }

    fn port(pid: u32, port: u16) -> ListeningPort {
        ListeningPort {
            pid,
            port,
            loopback: true,
        }
    }

    #[test]
    fn a_port_lands_under_the_project_whose_tree_holds_it() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/web", &[(20, "bash"), (21, "vite")]),
        ];
        let inv = attribute(&trees, &[port(21, 5173), port(11, 3000)]);
        assert_eq!(inv.groups.len(), 2);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
        assert_eq!(inv.groups[0].rows[0].port, 3000);
        assert_eq!(inv.groups[0].rows[0].process, "node");
        assert_eq!(inv.groups[1].project, PathBuf::from("/work/web"));
        assert_eq!(inv.groups[1].rows[0].port, 5173);
    }

    #[test]
    fn a_port_nobody_owns_is_not_shown() {
        let trees = vec![tree("/work/api", &[(10, "bash")])];
        // pid 99 exited between the walk and the socket read.
        assert!(attribute(&trees, &[port(99, 3000)]).is_empty());
    }

    #[test]
    fn a_project_with_nothing_listening_gets_no_heading() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/quiet", &[(20, "bash")]),
        ];
        let inv = attribute(&trees, &[port(11, 3000)]);
        assert_eq!(inv.groups.len(), 1, "an empty section reads as a bug");
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
    }

    #[test]
    fn two_terminals_in_one_project_share_a_heading() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/api", &[(20, "bash"), (21, "node")]),
        ];
        let inv = attribute(&trees, &[port(11, 3000), port(21, 3001)]);
        assert_eq!(inv.groups.len(), 1);
        assert_eq!(
            inv.groups[0].rows.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 3001]
        );
    }

    #[test]
    fn rows_are_ordered_by_port_not_by_discovery() {
        let trees = vec![tree("/work/api", &[(10, "a"), (11, "b"), (12, "c")])];
        let inv = attribute(&trees, &[port(12, 9229), port(10, 3000), port(11, 5173)]);
        assert_eq!(
            inv.groups[0].rows.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 5173, 9229]
        );
    }

    #[test]
    fn headings_are_ordered_the_same_way_every_poll() {
        // Same input, opposite tree order: the panel must not reshuffle.
        let a = tree("/work/api", &[(10, "node")]);
        let b = tree("/work/web", &[(20, "vite")]);
        let ports = [port(10, 3000), port(20, 5173)];
        let forward = attribute(&[a.clone(), b.clone()], &ports);
        let backward = attribute(&[b, a], &ports);
        assert_eq!(forward, backward);
    }

    #[test]
    fn a_pid_reachable_from_two_trees_is_counted_once() {
        // One terminal's shell running inside another's is the only way this
        // happens; the row belongs to the outer project, and only to it.
        let trees = vec![
            tree("/work/outer", &[(10, "bash"), (11, "node")]),
            tree("/work/inner", &[(11, "node")]),
        ];
        let inv = attribute(&trees, &[port(11, 3000)]);
        assert_eq!(inv.total(), 1);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/outer"));
    }

    #[test]
    fn the_pid_set_is_every_tree_deduplicated() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/web", &[(11, "node"), (20, "bash")]),
        ];
        assert_eq!(candidate_pids(&trees), vec![10, 11, 20]);
    }

    #[test]
    fn no_trees_means_nothing_to_ask_about() {
        assert!(candidate_pids(&[]).is_empty());
        assert!(attribute(&[], &[port(10, 3000)]).is_empty());
    }

    #[test]
    fn the_total_counts_rows_across_every_group() {
        let trees = vec![
            tree("/work/api", &[(10, "node"), (11, "node")]),
            tree("/work/web", &[(20, "vite")]),
        ];
        let inv = attribute(&trees, &[port(10, 3000), port(11, 3001), port(20, 5173)]);
        assert_eq!(inv.total(), 3);
    }
}
