//! What is being driven right now, in words a person can read at a glance.
//!
//! The indicator's whole job is to answer "is something clicking on my screen,
//! and what?" without the user having to go and look. That answer comes from
//! the grant table rather than from any in-flight call, and the distinction is
//! deliberate:
//!
//! # A held grant is the honest signal, not a call in flight
//!
//! A click takes milliseconds. An indicator that lit only while one was
//! dispatching would flicker at the exact moments it mattered and read as idle
//! the rest of the time — useless as a safety signal, and worse than nothing
//! because the user would learn to trust a dark indicator.
//!
//! A *held grant* means something else: this agent may drive that app at any
//! instant without asking again. That is the state worth showing, it lasts as
//! long as the risk does, and it clears exactly when the risk does — on chat
//! close, on release, on quit.
//!
//! # Read from the table, not from the approval path
//!
//! Grants are also created outside this process: the `PreToolUse` hook resolves
//! provenance itself and can grant a binary the agent just built without OxiMux
//! ever seeing an approval. Anything that tracked grants by observing the
//! consent card would therefore under-report — showing nothing while an agent
//! drove its own fresh build, which is the most common case this feature has.

use crate::grants::GrantTable;
use crate::target::name_of_pid;

/// Everything being driven, grouped by the agent driving it.
///
/// Empty means nothing is being driven, which is the overwhelmingly common
/// case — callers should treat [`Self::is_idle`] as the fast path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Driving {
    /// One entry per session holding at least one live grant, ordered by
    /// session id so a redraw does not reshuffle the list under the user.
    pub sessions: Vec<DrivingSession>,
}

/// One agent and the apps it may currently drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivingSession {
    /// The session id with its `oximux-` prefix stripped — the same label the
    /// agent's on-screen cursor carries, so the two can be matched up.
    pub label: String,
    /// App names, deduped and ordered. Never empty.
    pub apps: Vec<String>,
}

impl Driving {
    /// Read the table and name everything in it.
    ///
    /// A grant whose pid no longer resolves is dropped rather than shown as
    /// unknown: the process is gone, so nothing is being driven through it, and
    /// naming it "an unknown program" would be a scarier claim than the truth.
    pub fn read(grants: &GrantTable) -> Self {
        let mut sessions: Vec<DrivingSession> = Vec::new();
        for (pid, owner) in grants.all() {
            let Some(app) = name_of_pid(pid) else {
                continue;
            };
            let label = owner.strip_prefix("oximux-").unwrap_or(&owner).to_string();
            match sessions.iter_mut().find(|s| s.label == label) {
                Some(session) => {
                    if !session.apps.contains(&app) {
                        session.apps.push(app);
                    }
                }
                None => sessions.push(DrivingSession {
                    label,
                    apps: vec![app],
                }),
            }
        }
        sessions.sort_by(|a, b| a.label.cmp(&b.label));
        for session in &mut sessions {
            session.apps.sort();
        }
        Self { sessions }
    }

    pub fn is_idle(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Distinct apps across every session. Two agents driving two builds of the
    /// same program is the expected shape here, so this counts names rather
    /// than grants.
    fn distinct_apps(&self) -> Vec<&str> {
        let mut apps: Vec<&str> = self
            .sessions
            .iter()
            .flat_map(|s| s.apps.iter().map(String::as_str))
            .collect();
        apps.sort_unstable();
        apps.dedup();
        apps
    }

    /// The one line that has to carry the whole message when it is all the user
    /// sees — a tooltip, or a menu's first row.
    ///
    /// Names the apps while there are few enough to name. Past that, a count is
    /// more useful than a truncated list: the point is "a lot is happening", and
    /// "Safari, Calculator and 4 more" reads as detail nobody can act on.
    pub fn summary(&self) -> String {
        let apps = self.distinct_apps();
        if apps.is_empty() {
            return "No agent is controlling this Mac".to_string();
        }
        let who = if self.sessions.len() == 1 {
            "An agent is".to_string()
        } else {
            format!("{} agents are", self.sessions.len())
        };
        if apps.len() > 3 {
            return format!("{who} controlling {} apps", apps.len());
        }
        format!("{who} controlling {}", join_names(&apps))
    }

    /// Per-agent detail, for a menu that has room for it. One line each, so
    /// several agents driving at once stays readable.
    ///
    /// Skipped entirely for a single agent: it would just restate
    /// [`Self::summary`] with an id most users have no use for.
    pub fn detail_lines(&self) -> Vec<String> {
        if self.sessions.len() < 2 {
            return Vec::new();
        }
        self.sessions
            .iter()
            .map(|session| {
                let apps: Vec<&str> = session.apps.iter().map(String::as_str).collect();
                format!("{} — {}", session.label, join_names(&apps))
            })
            .collect()
    }
}

/// "a", "a and b", "a, b and c".
fn join_names(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionId;

    /// A live process standing in for an app being driven. Not our own pid:
    /// these tests care about naming and grouping, and a spawned child gives a
    /// second, distinct name to group against.
    struct Target(std::process::Child);

    impl Target {
        /// Spawn a process that outlives the test and prints nothing.
        ///
        /// Args are per-program rather than one hardcoded `120`: that argument
        /// belongs to `sleep`, and handing it to `cat` made it look for a file
        /// named `120`, fail, and exit — leaving a target that resolved only
        /// while it was still a zombie, so the test that read it passed or
        /// failed on timing.
        ///
        /// stdin is piped for the same reason from the other direction: `cat`
        /// with no argument reads stdin, and an inherited one is at EOF
        /// immediately. Holding the write end open is what keeps it blocked.
        fn spawn(program: &str, args: &[&str]) -> Self {
            Self(
                std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .expect("spawn a target process"),
            )
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Target {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn table() -> (tempfile::TempDir, GrantTable) {
        let dir = tempfile::tempdir().expect("tempdir");
        let table = GrantTable::in_data_dir(dir.path());
        (dir, table)
    }

    #[test]
    fn nothing_granted_reads_as_idle() {
        let (_dir, table) = table();
        let driving = Driving::read(&table);
        assert!(driving.is_idle());
        assert!(driving.detail_lines().is_empty());
        assert_eq!(driving.summary(), "No agent is controlling this Mac");
    }

    #[test]
    fn a_granted_target_is_named_by_its_program() {
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        table.grant(target.pid(), &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        assert!(!driving.is_idle());
        assert_eq!(driving.sessions.len(), 1);
        assert_eq!(driving.sessions[0].label, "chat-1");
        assert_eq!(driving.sessions[0].apps, vec!["sleep".to_string()]);
        assert_eq!(driving.summary(), "An agent is controlling sleep");
    }

    #[test]
    fn a_single_agent_gets_no_redundant_detail_line() {
        // The detail list exists to disambiguate several agents. With one, it
        // would only restate the summary alongside an id nobody asked for.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        table.grant(target.pid(), &SessionId::for_agent("chat-1"));
        assert!(Driving::read(&table).detail_lines().is_empty());
    }

    #[test]
    fn two_agents_are_reported_separately_and_in_a_stable_order() {
        // The parallelism premise: the user must be able to tell which of their
        // agents is doing what, and the list must not reshuffle on redraw.
        let (_dir, table) = table();
        let (a, b) = (Target::spawn("/bin/sleep", &["120"]), Target::spawn("/bin/cat", &[]));
        table.grant(b.pid(), &SessionId::for_agent("chat-2"));
        table.grant(a.pid(), &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        let labels: Vec<&str> = driving
            .sessions
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, vec!["chat-1", "chat-2"]);
        assert_eq!(
            driving.detail_lines(),
            vec!["chat-1 — sleep".to_string(), "chat-2 — cat".to_string()]
        );
        assert!(driving.summary().starts_with("2 agents are controlling"));
    }

    #[test]
    fn a_dead_pid_is_dropped_rather_than_named_as_unknown() {
        // Nothing is being driven through a process that no longer exists, so
        // reporting it would overstate what is happening.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        let pid = target.pid();
        table.grant(pid, &SessionId::for_agent("chat-1"));
        drop(target);

        assert!(Driving::read(&table).is_idle());
    }

    #[test]
    fn many_apps_are_counted_rather_than_listed() {
        // A truncated list is detail nobody can act on; the actionable fact at
        // that point is simply that a lot is happening.
        let driving = Driving {
            sessions: vec![DrivingSession {
                label: "chat-1".into(),
                apps: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            }],
        };
        assert_eq!(driving.summary(), "An agent is controlling 4 apps");
    }

    #[test]
    fn up_to_three_apps_are_named() {
        let driving = Driving {
            sessions: vec![DrivingSession {
                label: "chat-1".into(),
                apps: vec!["Safari".into(), "Calculator".into(), "Notes".into()],
            }],
        };
        // Sorted, not in grant order: the summary is redrawn on a timer, and a
        // list that reordered itself between redraws would read as movement.
        assert_eq!(
            driving.summary(),
            "An agent is controlling Calculator, Notes and Safari"
        );
    }

    #[test]
    fn the_same_app_driven_twice_is_counted_once() {
        // Two agents each driving their own build of the same program is the
        // expected shape, and "2 apps" would be the wrong thing to say.
        let driving = Driving {
            sessions: vec![
                DrivingSession {
                    label: "chat-1".into(),
                    apps: vec!["my-app".into()],
                },
                DrivingSession {
                    label: "chat-2".into(),
                    apps: vec!["my-app".into()],
                },
            ],
        };
        assert_eq!(driving.summary(), "2 agents are controlling my-app");
    }

    #[test]
    fn names_join_readably() {
        assert_eq!(join_names(&["a"]), "a");
        assert_eq!(join_names(&["a", "b"]), "a and b");
        assert_eq!(join_names(&["a", "b", "c"]), "a, b and c");
    }
}
