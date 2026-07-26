//! Which agent may drive which process.
//!
//! A grant binds a pid to one agent session. It is the only thing standing
//! between two agents driving each other's builds — or an agent driving the
//! user's editor — so the invariant is load-bearing rather than a nicety: drop
//! the pid pinning and cross-driving silently returns with nothing to catch it.
//!
//! Grants record the executable path as well as the pid, and every use
//! re-resolves the pid and compares. Be honest about what that buys: it narrows
//! the window in which a recycled pid could be driven under someone else's
//! grant, it does not close it. The driver re-resolves the pid on its own side
//! of the socket after we answer, so check-then-dispatch cannot be made atomic
//! across that hop from here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::proc::executable_of_pid;
use crate::session::SessionId;

/// What the table says about one agent driving one pid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// This agent holds a grant and the pid still runs the same executable.
    Granted,
    /// Nobody holds this pid. The caller asks the user.
    Ungranted,
    /// Held by a different agent — the cross-drive guard. Distinct from
    /// `Ungranted` because it must never become a consent prompt: the answer is
    /// no regardless of what the user would say.
    HeldByAnother { owner: SessionId },
    /// The pid resolves to nothing, so the call cannot be attributed to any
    /// program. Refuse; never treat an unreadable pid as absent.
    Unresolvable,
    /// The pid now runs a different executable than when it was granted —
    /// the number was recycled.
    Recycled { granted: PathBuf, found: PathBuf },
}

#[derive(Debug, Clone)]
struct Grant {
    owner: SessionId,
    /// Resolved at grant time; compared on every use.
    executable: PathBuf,
}

/// Pid → owning agent, for every agent in this process.
///
/// One table rather than one per agent: cross-drive detection needs to see
/// every agent's grants at once, which a per-agent table structurally cannot.
#[derive(Debug, Default)]
pub struct GrantTable {
    grants: Mutex<HashMap<u32, Grant>>,
}

impl GrantTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// May `agent` drive `pid` right now?
    ///
    /// Re-resolves the executable on every call rather than trusting the path
    /// recorded at grant time — that re-resolution is the whole point.
    pub fn check(&self, pid: u32, agent: &SessionId) -> Verdict {
        let Some(found) = executable_of_pid(pid) else {
            return Verdict::Unresolvable;
        };
        let grants = self.grants.lock().expect("grant table poisoned");
        match grants.get(&pid) {
            None => Verdict::Ungranted,
            Some(grant) if &grant.owner != agent => Verdict::HeldByAnother {
                owner: grant.owner.clone(),
            },
            Some(grant) if grant.executable != found => Verdict::Recycled {
                granted: grant.executable.clone(),
                found,
            },
            Some(_) => Verdict::Granted,
        }
    }

    /// Record a grant, returning what actually happened.
    ///
    /// Check and insert happen under one lock. Two agents that both see a
    /// never-granted pid and race to claim it must not both win: the loser gets
    /// `HeldByAnother`, so a concurrent double-approval resolves to one owner
    /// rather than two.
    pub fn grant(&self, pid: u32, agent: &SessionId) -> Verdict {
        let Some(executable) = executable_of_pid(pid) else {
            return Verdict::Unresolvable;
        };
        let mut grants = self.grants.lock().expect("grant table poisoned");
        match grants.get(&pid) {
            Some(grant) if &grant.owner != agent => Verdict::HeldByAnother {
                owner: grant.owner.clone(),
            },
            // Re-granting to the same owner refreshes the recorded path, which
            // is what makes a legitimate relaunch-into-the-same-pid work.
            _ => {
                grants.insert(
                    pid,
                    Grant {
                        owner: agent.clone(),
                        executable,
                    },
                );
                Verdict::Granted
            }
        }
    }

    /// Drop every grant an agent holds. Called on agent exit and on quit — a
    /// grant outliving its agent would be inherited by whoever next gets that
    /// session id.
    pub fn release_all(&self, agent: &SessionId) {
        self.grants
            .lock()
            .expect("grant table poisoned")
            .retain(|_, grant| &grant.owner != agent);
    }

    /// Pids currently granted to an agent. For tests and for the transcript.
    pub fn granted_to(&self, agent: &SessionId) -> Vec<u32> {
        let grants = self.grants.lock().expect("grant table poisoned");
        let mut pids: Vec<u32> = grants
            .iter()
            .filter(|(_, grant)| &grant.owner == agent)
            .map(|(pid, _)| *pid)
            .collect();
        pids.sort_unstable();
        pids
    }
}

/// What an agent built for itself, and may therefore drive without asking.
///
/// The workflow this exists for is "agent builds an app in its worktree, runs
/// it, drives it" — stopping to ask about a binary the agent just produced is
/// noise. Everything else asks.
#[derive(Debug, Clone)]
pub struct Provenance {
    /// The agent's worktree, canonicalized once at construction.
    root: PathBuf,
    /// When this agent's session began. A binary older than this was not built
    /// by it.
    since: SystemTime,
}

impl Provenance {
    /// `None` when the worktree cannot be canonicalized — an unresolvable root
    /// can only produce unsound containment answers, so there is no provenance
    /// rather than a guessed one.
    pub fn new(worktree: &Path, session_start: SystemTime) -> Option<Self> {
        Some(Self {
            root: worktree.canonicalize().ok()?,
            since: session_start,
        })
    }

    /// Did this agent build `executable` during this session?
    ///
    /// Containment alone is not enough and never was: `cp -R /Applications/
    /// Safari.app <worktree>/` puts a foreign binary genuinely, canonically
    /// inside the worktree, and the agent has a shell. The mtime is what ties
    /// the binary to *this run* — a copy preserves neither a fresh mtime nor,
    /// for `cp -p`, one later than the session start.
    ///
    /// Deliberately not routed through the repo's `path_guard::contained_path`:
    /// that one hardens untrusted, possibly relative, possibly nonexistent path
    /// *strings*, with a lexical fallback and an absolute-path carve-out. Here
    /// both sides are absolute paths the kernel already resolved. Same shape,
    /// different threat model, and reusing it would inherit carve-outs written
    /// for the other one.
    pub fn built_this_session(&self, executable: &Path) -> bool {
        let Ok(resolved) = executable.canonicalize() else {
            return false;
        };
        // Component-wise, so a sibling worktree named `<root>-2` is not a
        // prefix match the way a string comparison would make it.
        if !resolved.starts_with(&self.root) {
            return false;
        }
        let Ok(modified) = std::fs::metadata(&resolved).and_then(|m| m.modified()) else {
            return false;
        };
        modified >= self.since
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn agent(name: &str) -> SessionId {
        SessionId::for_agent(name)
    }

    /// Our own pid, which is guaranteed to resolve to a real executable.
    fn live_pid() -> u32 {
        std::process::id()
    }

    #[test]
    fn a_pid_nobody_claimed_is_ungranted() {
        let table = GrantTable::new();
        assert_eq!(table.check(live_pid(), &agent("a")), Verdict::Ungranted);
    }

    #[test]
    fn granting_then_checking_allows_the_owner() {
        let table = GrantTable::new();
        let a = agent("a");
        assert_eq!(table.grant(live_pid(), &a), Verdict::Granted);
        assert_eq!(table.check(live_pid(), &a), Verdict::Granted);
    }

    #[test]
    fn one_agent_cannot_drive_another_agents_process() {
        // The invariant the whole table exists for.
        let table = GrantTable::new();
        let (a, b) = (agent("a"), agent("b"));
        table.grant(live_pid(), &a);
        assert_eq!(
            table.check(live_pid(), &b),
            Verdict::HeldByAnother { owner: a }
        );
    }

    #[test]
    fn a_cross_drive_attempt_cannot_be_upgraded_into_a_grant() {
        // Distinct from ungranted precisely so it never reaches a consent
        // prompt: claiming a pid another agent holds must fail even on the
        // grant path, not just the check path.
        let table = GrantTable::new();
        let (a, b) = (agent("a"), agent("b"));
        table.grant(live_pid(), &a);
        assert_eq!(
            table.grant(live_pid(), &b),
            Verdict::HeldByAnother { owner: a }
        );
    }

    #[test]
    fn an_unresolvable_pid_is_refused_rather_than_treated_as_free() {
        let table = GrantTable::new();
        assert_eq!(table.check(u32::MAX, &agent("a")), Verdict::Unresolvable);
        assert_eq!(table.grant(u32::MAX, &agent("a")), Verdict::Unresolvable);
    }

    #[test]
    fn a_recycled_pid_is_refused() {
        // Simulate reuse by recording a grant against a path that is not what
        // the pid actually runs — the same state a caller would observe after
        // the granted process exited and its number was handed to another.
        let table = GrantTable::new();
        let a = agent("a");
        table.grants.lock().unwrap().insert(
            live_pid(),
            Grant {
                owner: a.clone(),
                executable: PathBuf::from("/usr/bin/true"),
            },
        );
        assert!(matches!(
            table.check(live_pid(), &a),
            Verdict::Recycled { .. }
        ));
    }

    #[test]
    fn releasing_an_agent_drops_only_its_own_grants() {
        let table = GrantTable::new();
        let (a, b) = (agent("a"), agent("b"));
        table.grant(live_pid(), &a);
        table.grants.lock().unwrap().insert(
            999_001,
            Grant {
                owner: b.clone(),
                executable: PathBuf::from("/usr/bin/true"),
            },
        );

        table.release_all(&a);
        assert!(table.granted_to(&a).is_empty());
        assert_eq!(table.granted_to(&b), vec![999_001]);
    }

    #[test]
    fn a_released_pid_can_be_claimed_by_the_next_agent() {
        // Teardown has to actually free the pid, or a long-lived OxiMux would
        // accumulate grants that refuse everyone.
        let table = GrantTable::new();
        let (a, b) = (agent("a"), agent("b"));
        table.grant(live_pid(), &a);
        table.release_all(&a);
        assert_eq!(table.grant(live_pid(), &b), Verdict::Granted);
    }

    #[test]
    fn concurrent_claims_on_one_pid_resolve_to_a_single_owner() {
        // Two agents raising consent for the same never-seen pid can both read
        // "ungranted"; only one may end up holding it.
        let table = std::sync::Arc::new(GrantTable::new());
        let pid = live_pid();
        let agents: Vec<SessionId> = (0..8).map(|i| agent(&format!("agent-{i}"))).collect();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(agents.len()));
        let winners: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = agents
                .iter()
                .map(|id| {
                    let (table, barrier) = (table.clone(), barrier.clone());
                    scope.spawn(move || {
                        barrier.wait();
                        table.grant(pid, id) == Verdict::Granted
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(
            winners.iter().filter(|won| **won).count(),
            1,
            "exactly one agent may win the race"
        );
        let owners: Vec<&SessionId> = agents.iter().filter(|id| !table.granted_to(id).is_empty()).collect();
        assert_eq!(owners.len(), 1, "the table must record exactly one owner");
    }

    fn provenance_fixture() -> (tempfile::TempDir, SystemTime) {
        let dir = tempfile::tempdir().expect("tempdir");
        // A second in the past, so a file created now is unambiguously "after"
        // even at filesystem timestamp granularity.
        let started = SystemTime::now() - Duration::from_secs(1);
        (dir, started)
    }

    #[test]
    fn a_binary_built_in_the_worktree_this_session_counts_as_ours() {
        let (dir, started) = provenance_fixture();
        let built = dir.path().join("target/debug/app");
        std::fs::create_dir_all(built.parent().unwrap()).expect("mkdir");
        std::fs::write(&built, b"fresh").expect("write");

        let prov = Provenance::new(dir.path(), started).expect("provenance");
        assert!(prov.built_this_session(&built));
    }

    #[test]
    fn a_binary_copied_into_the_worktree_before_the_session_does_not() {
        // The attack path: `cp -R /Applications/Something.app <worktree>/` is
        // genuinely contained, so containment alone would allow it. Only the
        // build-time evidence separates the two.
        let (dir, _) = provenance_fixture();
        let planted = dir.path().join("Planted.app");
        std::fs::write(&planted, b"foreign").expect("write");

        // Session starts *after* the binary landed.
        let prov = Provenance::new(dir.path(), SystemTime::now() + Duration::from_secs(60))
            .expect("provenance");
        assert!(!prov.built_this_session(&planted));
    }

    #[test]
    fn a_binary_outside_the_worktree_is_not_ours_however_fresh() {
        let (dir, started) = provenance_fixture();
        let outside = tempfile::tempdir().expect("tempdir");
        let path = outside.path().join("app");
        std::fs::write(&path, b"fresh").expect("write");

        let prov = Provenance::new(dir.path(), started).expect("provenance");
        assert!(!prov.built_this_session(&path));
    }

    #[test]
    fn a_symlink_pointing_out_of_the_worktree_is_refused() {
        let (dir, started) = provenance_fixture();
        let outside = tempfile::tempdir().expect("tempdir");
        let real = outside.path().join("app");
        std::fs::write(&real, b"fresh").expect("write");
        let link = dir.path().join("app");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let prov = Provenance::new(dir.path(), started).expect("provenance");
        assert!(
            !prov.built_this_session(&link),
            "canonicalization must see through the link"
        );
    }

    #[test]
    fn a_sibling_worktree_sharing_a_name_prefix_is_not_contained() {
        // `starts_with` on paths is component-wise; a string prefix check would
        // let `<root>-2` pass as inside `<root>`.
        let parent = tempfile::tempdir().expect("tempdir");
        let root = parent.path().join("work");
        let sibling = parent.path().join("work-2");
        std::fs::create_dir_all(&root).expect("mkdir");
        std::fs::create_dir_all(&sibling).expect("mkdir");
        let path = sibling.join("app");
        std::fs::write(&path, b"fresh").expect("write");

        let prov = Provenance::new(&root, SystemTime::now() - Duration::from_secs(1))
            .expect("provenance");
        assert!(!prov.built_this_session(&path));
    }

    #[test]
    fn a_missing_executable_has_no_provenance() {
        let (dir, started) = provenance_fixture();
        let prov = Provenance::new(dir.path(), started).expect("provenance");
        assert!(!prov.built_this_session(&dir.path().join("never-built")));
    }

    #[test]
    fn an_unresolvable_worktree_yields_no_provenance_at_all() {
        assert!(Provenance::new(Path::new("/nonexistent/worktree"), SystemTime::now()).is_none());
    }
}
