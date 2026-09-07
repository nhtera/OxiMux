//! Turning "these pids are listening" into "this project is serving this".
//!
//! The kernel answers about pids. A person thinks about projects. Everything
//! in this file is the join between those two, kept pure so that the rules —
//! which port is shown, whose it is, what happens when two terminals in one
//! project both contain it — are decided by tests rather than by whatever the
//! render pass happened to do.
//!
//! **Why the scan is machine-wide.** It was not, once: it walked only the
//! process trees of terminals this window owns, which meant a dev server
//! started in another terminal app, a database in a container, or a daemon
//! already holding the port a build is about to want were all invisible. A
//! panel that can only see the servers you started *here* cannot tell "nothing
//! is running" apart from "it is running, somewhere else", and those are
//! different answers to the only question a person opens this list to ask.
//! So the socket table is read whole and the rows are *attributed* afterwards.
//!
//! **Three kinds of evidence, in order.** A port is claimed by a project when:
//!
//! 1. its pid is inside the process tree of a terminal this window owns — the
//!    strongest signal there is, because the user demonstrably started it here;
//! 2. failing that, the process's working directory is inside a known project
//!    root — how a server started in an outside terminal is recognised;
//! 3. failing that, the project's path appears as one of the process's
//!    arguments — how `node /work/api/server.js` launched from `/` is caught.
//!
//! Anything left over is *external*: real, listed, actionable to open and copy,
//! but not claimed by a project and never offered a stop button. Attribution
//! evidence is carried on the row rather than discarded, because "why does the
//! panel think this is mine" is a question with a real answer.
//!
//! **Why labels are persisted.** Three `node` rows on 3000, 3001 and 9229 are
//! the API, the docs site and a debugger, and nothing the kernel knows can
//! tell them apart. The name is the user's to write, so it is stored against
//! project+port and comes back when the same server does.
//!
//! One thing here is not pure: [`gather`], the syscall bridge, which reads the
//! socket table and resolves process metadata. It sits beside the rules it
//! feeds rather than in the view, because what it produces is only meaningful
//! against them — but it is the *only* thing in this file that touches the
//! kernel, and it is called from a background thread.

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

/// What connected a port to the project it is filed under.
///
/// Declared strongest-first, matching the order [`attribute`] and
/// [`claim_by_metadata`] try them in — a port reachable by two routes is filed
/// by the strongest, and the row carries which one so a surprising grouping
/// can be explained rather than argued with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Attribution {
    /// The pid sits inside a terminal this window owns.
    Terminal,
    /// The process's working directory is inside the project.
    Cwd,
    /// The project's path appears in the process's arguments.
    Command,
}

/// What a process was, as far as the kernel would say.
///
/// All three fields are best-effort and independently absent: a process can
/// refuse its working directory (another user's), its arguments (Windows), or
/// both, and still be a perfectly real listener worth showing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PidMeta {
    /// Executable name, or empty when the kernel would not name it.
    pub name: String,
    pub cwd: Option<PathBuf>,
    /// The argument vector, `argv[0]` included.
    pub argv: Vec<String>,
}

/// A listening port, attributed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortRow {
    pub port: u16,
    pub pid: u32,
    /// Executable name of the listening process, or empty when neither the
    /// tree walk nor the metadata read produced one.
    pub process: String,
    pub loopback: bool,
    /// `None` on an external row — nothing claimed it.
    pub attribution: Option<Attribution>,
}

impl PortRow {
    /// Whether this row belongs to a project, and so may be acted on
    /// destructively. External rows are read-only by design: the panel will
    /// not offer to kill a system daemon it merely happened to notice.
    pub fn is_owned(&self) -> bool {
        self.attribution.is_some()
    }
}

/// Every port found under one project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortGroup {
    pub project: PathBuf,
    pub rows: Vec<PortRow>,
}

/// What the panel draws: ports grouped by the project they were started in,
/// plus everything else the machine is listening on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortInventory {
    pub groups: Vec<PortGroup>,
    /// Listeners no project claimed. Kept rather than dropped — this is the
    /// half that answers "what already has 3000".
    pub external: Vec<PortRow>,
}

impl PortInventory {
    /// Project-attributed rows. The status bar's metric, and deliberately not
    /// the external count: a segment reading "84 ports" because the OS talks
    /// to itself is a number nobody can act on.
    pub fn total(&self) -> usize {
        self.groups.iter().map(|g| g.rows.len()).sum()
    }

    pub fn external_count(&self) -> usize {
        self.external.len()
    }

    /// Nothing at all to draw — no owned rows *and* no external ones.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.external.is_empty()
    }
}

/// Group `ports` by the project each one can be attributed to.
///
/// `trees` supplies the strongest evidence and `roots` the other two; `meta`
/// is what the kernel said about each listening pid. Projects with nothing
/// listening are omitted entirely: an empty group is a heading with nothing
/// under it, which reads as a bug.
pub fn attribute(
    trees: &[TreeSnapshot],
    roots: &[PathBuf],
    ports: &[ListeningPort],
    meta: &HashMap<u32, PidMeta>,
) -> PortInventory {
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

    let mut by_project: Vec<(PathBuf, Vec<PortRow>)> = Vec::new();
    let mut external: Vec<PortRow> = Vec::new();
    for port in ports {
        let meta = meta.get(&port.pid);
        let tree_owner = owner.get(&port.pid);
        let process = tree_owner
            .map(|(_, name)| (*name).to_string())
            .filter(|name| !name.is_empty())
            .or_else(|| meta.map(|m| m.name.clone()))
            .unwrap_or_default();

        let claim = tree_owner
            .map(|(project, _)| (project.to_path_buf(), Attribution::Terminal))
            .or_else(|| meta.and_then(|meta| claim_by_metadata(meta, roots)));

        let row = PortRow {
            port: port.port,
            pid: port.pid,
            process,
            loopback: port.loopback,
            attribution: claim.as_ref().map(|(_, how)| *how),
        };
        match claim {
            Some((project, _)) => match by_project.iter_mut().find(|(p, _)| *p == project) {
                Some((_, rows)) => rows.push(row),
                None => by_project.push((project, vec![row])),
            },
            None => external.push(row),
        }
    }

    let mut groups: Vec<PortGroup> = by_project
        .into_iter()
        .map(|(project, mut rows)| {
            rows.sort_by_key(|r| (r.port, r.pid));
            PortGroup { project, rows }
        })
        .collect();
    // Stable heading order: the panel is re-rendered every poll, and a list
    // whose sections reshuffle under the cursor is unusable.
    groups.sort_by(|a, b| a.project.cmp(&b.project));
    external.sort_by_key(|r| (r.port, r.pid));
    PortInventory { groups, external }
}

/// The strongest claim `roots` can make on a process, by working directory
/// first and arguments second.
fn claim_by_metadata(meta: &PidMeta, roots: &[PathBuf]) -> Option<(PathBuf, Attribution)> {
    if let Some(cwd) = &meta.cwd
        && let Some(root) = deepest_containing(roots, cwd)
    {
        return Some((root, Attribution::Cwd));
    }
    // Arguments are weaker evidence on purpose: a path can appear in a command
    // line for reasons other than being where the server runs (a `--config`
    // pointing into a sibling checkout, say). It is checked only when the
    // working directory said nothing, and never overrides it.
    let claimed = meta
        .argv
        .iter()
        .filter_map(|arg| deepest_containing(roots, Path::new(argument_path(arg))))
        .max_by_key(|root| root.as_os_str().len())?;
    Some((claimed, Attribution::Command))
}

/// The path half of one command-line argument.
///
/// `--prefix=/work/api` carries a path that `Path::new` on the whole argument
/// would never match, and splitting on the first `=` is the only shape common
/// enough across CLIs to be worth handling. An argument with no `=` is
/// returned unchanged.
fn argument_path(arg: &str) -> &str {
    match arg.split_once('=') {
        Some((_, value)) if value.starts_with('/') || value.contains(':') => value,
        _ => arg,
    }
}

/// The longest root that `path` is inside of, or is.
///
/// Deepest wins because project roots nest: a worktree lives under its
/// repository, and a server running in the worktree belongs to the worktree.
/// Compared by [`Path::starts_with`], which is component-wise — `/work/apiv2`
/// is not inside `/work/api`, and a substring test would say it was.
fn deepest_containing(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    roots
        .iter()
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.as_os_str().len())
        .cloned()
}

/// What the kernel said about each listening pid, remembered between polls.
///
/// The socket read is cheap and the metadata behind it is not: resolving a
/// working directory and an argument vector is two syscalls per process, and
/// a machine-wide scan sees every listener on the box. On a quiet machine the
/// same servers keep listening, so re-deriving that every few seconds would be
/// paying repeatedly for an answer that has not changed.
///
/// Entries are dropped as soon as their pid stops listening, which bounds the
/// map to what is currently on screen and keeps a recycled pid from inheriting
/// the identity of a process that exited long ago. A pid recycled *while still
/// listening* would still be mislabelled for one poll — which is why
/// [`super::kill`] re-checks the process it is about to stop rather than
/// trusting a rendered row.
#[derive(Default)]
pub struct PidMetaCache {
    by_pid: HashMap<u32, PidMeta>,
}

impl PidMetaCache {
    /// Metadata for every pid in `pids`, reading only the ones not already
    /// known and forgetting every pid not asked about.
    pub fn resolve(&mut self, pids: &[u32]) -> &HashMap<u32, PidMeta> {
        self.by_pid.retain(|pid, _| pids.contains(pid));
        for &pid in pids {
            self.by_pid.entry(pid).or_insert_with(|| read_meta(pid));
        }
        &self.by_pid
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_pid.len()
    }
}

/// One process's identity, straight from the kernel. **Blocking.**
fn read_meta(pid: u32) -> PidMeta {
    PidMeta {
        name: oximux_proc_tree::process(pid)
            .map(|p| p.name)
            .unwrap_or_default(),
        cwd: oximux_proc_cwd::cwd_of_pid(pid),
        argv: oximux_proc_tree::argv_of_pid(pid).unwrap_or_default(),
    }
}

/// Read the socket table and attribute it. **Blocking — background executor
/// only.**
///
/// `terminal_roots` is `(project working directory, terminal shell pid)`, as
/// `ProjectPanes::terminal_roots` reports it; `project_roots` is every project
/// and worktree path this window knows about, whether or not it has a terminal
/// open.
///
/// Each terminal's root is included in its own tree, not just its descendants:
/// a terminal whose command replaced the shell outright — `oximux run npm
/// start` rather than a shell that then ran it — has the listener at the root,
/// and walking only downward would miss exactly the case a user is most likely
/// to have set up on purpose.
pub fn gather(
    terminal_roots: Vec<(PathBuf, u32)>,
    project_roots: Vec<PathBuf>,
    cache: &mut PidMetaCache,
) -> PortInventory {
    let trees: Vec<TreeSnapshot> = terminal_roots
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
    let ports = oximux_proc_ports::listening_ports();
    // Only listening pids get metadata read for them. The socket table is the
    // filter that keeps a machine-wide scan from becoming a process census.
    let mut pids: Vec<u32> = ports.iter().map(|p| p.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    let meta = cache.resolve(&pids);
    attribute(&trees, &project_roots, &ports, meta)
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

    fn roots(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn meta_of(entries: &[(u32, &str, Option<&str>, &[&str])]) -> HashMap<u32, PidMeta> {
        entries
            .iter()
            .map(|(pid, name, cwd, argv)| {
                (
                    *pid,
                    PidMeta {
                        name: (*name).to_string(),
                        cwd: cwd.map(PathBuf::from),
                        argv: argv.iter().map(|a| (*a).to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    fn no_meta() -> HashMap<u32, PidMeta> {
        HashMap::new()
    }

    #[test]
    fn a_port_lands_under_the_project_whose_tree_holds_it() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/web", &[(20, "bash"), (21, "vite")]),
        ];
        let inv = attribute(&trees, &[], &[port(21, 5173), port(11, 3000)], &no_meta());
        assert_eq!(inv.groups.len(), 2);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
        assert_eq!(inv.groups[0].rows[0].port, 3000);
        assert_eq!(inv.groups[0].rows[0].process, "node");
        assert_eq!(
            inv.groups[0].rows[0].attribution,
            Some(Attribution::Terminal)
        );
        assert_eq!(inv.groups[1].project, PathBuf::from("/work/web"));
        assert_eq!(inv.groups[1].rows[0].port, 5173);
    }

    #[test]
    fn a_port_nobody_owns_is_external_rather_than_dropped() {
        let trees = vec![tree("/work/api", &[(10, "bash")])];
        // The whole point of the machine-wide scan: a listener the window did
        // not start is still worth seeing.
        let inv = attribute(&trees, &[], &[port(99, 3000)], &no_meta());
        assert!(inv.groups.is_empty());
        assert_eq!(inv.external.len(), 1);
        assert_eq!(inv.external[0].port, 3000);
        assert_eq!(inv.external[0].attribution, None);
        assert!(!inv.external[0].is_owned());
        assert!(!inv.is_empty(), "an external row is still something to draw");
    }

    #[test]
    fn a_working_directory_inside_a_project_claims_the_port() {
        // Started in another terminal app entirely: no tree contains it.
        let meta = meta_of(&[(50, "node", Some("/work/api/src"), &[])]);
        let inv = attribute(&[], &roots(&["/work/api"]), &[port(50, 3000)], &meta);
        assert_eq!(inv.groups.len(), 1);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
        assert_eq!(inv.groups[0].rows[0].attribution, Some(Attribution::Cwd));
        assert_eq!(inv.groups[0].rows[0].process, "node");
    }

    #[test]
    fn a_sibling_directory_with_a_shared_prefix_is_not_inside_the_project() {
        // `/work/apiv2` starts with the string `/work/api` and is not under it.
        let meta = meta_of(&[(50, "node", Some("/work/apiv2"), &[])]);
        let inv = attribute(&[], &roots(&["/work/api"]), &[port(50, 3000)], &meta);
        assert!(inv.groups.is_empty(), "path containment is by component");
        assert_eq!(inv.external.len(), 1);
    }

    #[test]
    fn the_deepest_matching_root_wins() {
        // A worktree lives under its repository; the server is the worktree's.
        let meta = meta_of(&[(50, "node", Some("/work/api/wt/feature/src"), &[])]);
        let inv = attribute(
            &[],
            &roots(&["/work/api", "/work/api/wt/feature"]),
            &[port(50, 3000)],
            &meta,
        );
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api/wt/feature"));
    }

    #[test]
    fn a_project_path_in_the_arguments_claims_a_port_the_cwd_did_not() {
        let meta = meta_of(&[(
            50,
            "node",
            Some("/"),
            &["node", "/work/api/server.js"],
        )]);
        let inv = attribute(&[], &roots(&["/work/api"]), &[port(50, 3000)], &meta);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
        assert_eq!(inv.groups[0].rows[0].attribution, Some(Attribution::Command));
    }

    #[test]
    fn a_path_behind_a_flag_is_still_a_path() {
        let meta = meta_of(&[(50, "npm", Some("/"), &["npm", "--prefix=/work/api", "start"])]);
        let inv = attribute(&[], &roots(&["/work/api"]), &[port(50, 3000)], &meta);
        assert_eq!(inv.groups[0].rows[0].attribution, Some(Attribution::Command));
    }

    #[test]
    fn the_working_directory_outranks_the_arguments() {
        // Running in /work/web with a config path pointing into /work/api: the
        // server is the web one, and the argument must not move it.
        let meta = meta_of(&[(
            50,
            "node",
            Some("/work/web"),
            &["node", "--config", "/work/api/shared.json"],
        )]);
        let inv = attribute(
            &[],
            &roots(&["/work/api", "/work/web"]),
            &[port(50, 3000)],
            &meta,
        );
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/web"));
        assert_eq!(inv.groups[0].rows[0].attribution, Some(Attribution::Cwd));
    }

    #[test]
    fn a_terminal_tree_outranks_a_working_directory() {
        // The user started it here, in a terminal whose group cwd is the
        // worktree; a stale-looking cwd elsewhere must not relocate the row.
        let trees = vec![tree("/work/api", &[(50, "node")])];
        let meta = meta_of(&[(50, "node", Some("/work/web"), &[])]);
        let inv = attribute(&trees, &roots(&["/work/web"]), &[port(50, 3000)], &meta);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
        assert_eq!(
            inv.groups[0].rows[0].attribution,
            Some(Attribution::Terminal)
        );
    }

    #[test]
    fn a_project_with_nothing_listening_gets_no_heading() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/quiet", &[(20, "bash")]),
        ];
        let inv = attribute(&trees, &[], &[port(11, 3000)], &no_meta());
        assert_eq!(inv.groups.len(), 1, "an empty section reads as a bug");
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/api"));
    }

    #[test]
    fn two_terminals_in_one_project_share_a_heading() {
        let trees = vec![
            tree("/work/api", &[(10, "bash"), (11, "node")]),
            tree("/work/api", &[(20, "bash"), (21, "node")]),
        ];
        let inv = attribute(&trees, &[], &[port(11, 3000), port(21, 3001)], &no_meta());
        assert_eq!(inv.groups.len(), 1);
        assert_eq!(
            inv.groups[0].rows.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 3001]
        );
    }

    #[test]
    fn rows_are_ordered_by_port_not_by_discovery() {
        let trees = vec![tree("/work/api", &[(10, "a"), (11, "b"), (12, "c")])];
        let inv = attribute(
            &trees,
            &[],
            &[port(12, 9229), port(10, 3000), port(11, 5173)],
            &no_meta(),
        );
        assert_eq!(
            inv.groups[0].rows.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 5173, 9229]
        );
    }

    #[test]
    fn external_rows_are_ordered_by_port_too() {
        let inv = attribute(
            &[],
            &[],
            &[port(12, 9229), port(10, 3000), port(11, 5173)],
            &no_meta(),
        );
        assert_eq!(
            inv.external.iter().map(|r| r.port).collect::<Vec<_>>(),
            vec![3000, 5173, 9229]
        );
    }

    #[test]
    fn headings_are_ordered_the_same_way_every_poll() {
        // Same input, opposite tree order: the panel must not reshuffle.
        let a = tree("/work/api", &[(10, "node")]);
        let b = tree("/work/web", &[(20, "vite")]);
        let ports = [port(10, 3000), port(20, 5173)];
        let forward = attribute(&[a.clone(), b.clone()], &[], &ports, &no_meta());
        let backward = attribute(&[b, a], &[], &ports, &no_meta());
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
        let inv = attribute(&trees, &[], &[port(11, 3000)], &no_meta());
        assert_eq!(inv.total(), 1);
        assert_eq!(inv.groups[0].project, PathBuf::from("/work/outer"));
    }

    #[test]
    fn the_metric_counts_owned_rows_and_the_external_count_is_separate() {
        let trees = vec![tree("/work/api", &[(10, "node"), (11, "node")])];
        let inv = attribute(
            &trees,
            &[],
            &[port(10, 3000), port(11, 3001), port(99, 5432)],
            &no_meta(),
        );
        assert_eq!(inv.total(), 2, "the status bar counts what the user started");
        assert_eq!(inv.external_count(), 1);
    }

    #[test]
    fn nothing_listening_anywhere_is_the_only_empty_inventory() {
        assert!(attribute(&[], &[], &[], &no_meta()).is_empty());
    }

    #[test]
    fn a_row_takes_its_name_from_the_metadata_when_no_tree_named_it() {
        let meta = meta_of(&[(50, "postgres", None, &[])]);
        let inv = attribute(&[], &[], &[port(50, 5432)], &meta);
        assert_eq!(inv.external[0].process, "postgres");
    }

    #[test]
    fn a_pid_that_stopped_listening_is_forgotten() {
        // Otherwise the map grows for the life of the window, and a recycled
        // pid inherits the identity of something that exited long ago.
        let mut cache = PidMetaCache::default();
        let me = std::process::id();
        cache.resolve(&[me]);
        assert_eq!(cache.len(), 1);
        cache.resolve(&[]);
        assert_eq!(cache.len(), 0);
    }

    /// Against the real kernel: our own process must resolve to something
    /// nameable with a working directory. Guards the metadata read from
    /// silently returning blanks, which would make every row unattributable.
    #[test]
    fn our_own_process_resolves_to_real_metadata() {
        let mut cache = PidMetaCache::default();
        let me = std::process::id();
        let meta = cache.resolve(&[me]).get(&me).cloned().expect("our own pid");
        assert!(!meta.name.is_empty(), "the kernel names our own process");
        assert!(meta.cwd.is_some(), "our own working directory is readable");
    }
}
