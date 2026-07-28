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

/// Stands in for a window a capture did not name. Phrased as something the user
/// can act on ("an agent has a picture of something of mine") rather than as an
/// error, because it is not one — some capture tools simply take a window id.
const UNNAMED_WINDOW: &str = "a window";

/// Everything being driven, grouped by the agent driving it.
///
/// Empty means nothing is being driven, which is the overwhelmingly common
/// case — callers should treat [`Self::is_idle`] as the fast path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Driving {
    /// One entry per session that is mid-drive and holds at least one live
    /// grant, ordered by session id so a redraw does not reshuffle the list
    /// under the user.
    pub sessions: Vec<DrivingSession>,
}

/// One agent and what it is currently doing to the screen.
///
/// The two lists are kept apart rather than merged into "apps involved" because
/// they are different claims about the user's machine, and the weaker one is far
/// commoner. Saying an agent *controls* a window it only photographed overstates
/// what is happening; saying nothing at all about the photograph is how a
/// capture went unannounced in the first place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivingSession {
    /// The session id with its `oximux-` prefix stripped — the same label the
    /// agent's on-screen cursor carries, so the two can be matched up.
    pub label: String,
    /// Apps this agent holds a grant on and is driving. Deduped and ordered.
    pub controlling: Vec<String>,
    /// Apps this agent has photographed this turn but cannot drive. Deduped and
    /// ordered, and never overlapping `controlling` — a driven app is reported
    /// as driven, which is the stronger and more useful fact.
    pub reading: Vec<String>,
}

impl Driving {
    /// Read the table and name what is being driven right now.
    ///
    /// A grant whose pid no longer resolves is dropped rather than shown as
    /// unknown: the process is gone, so nothing is being driven through it, and
    /// naming it "an unknown program" would be a scarier claim than the truth.
    ///
    /// Reads [`GrantTable::driving`] rather than every grant, because a grant
    /// outlives the turn that used it — it is consent, and consent is answered
    /// once per chat rather than once per turn. Built on every grant instead,
    /// this would report an agent as driving from its first click until the user
    /// closed the tab, and everything downstream believes it: the menu bar
    /// claims apps are being driven while the machine sits idle, and the Escape
    /// tap stays armed, swallowing a key the user needs everywhere else.
    pub fn read(grants: &GrantTable) -> Self {
        let mut sessions: Vec<DrivingSession> = Vec::new();
        for (pid, owner, driven) in grants
            .driving()
            .into_iter()
            .map(|(pid, owner)| (pid, owner, true))
            .chain(grants.captured().into_iter().map(|(pid, owner)| (pid, owner, false)))
        {
            let Some(app) = name_of_pid(pid) else {
                continue;
            };
            let label = owner.strip_prefix("oximux-").unwrap_or(&owner).to_string();
            let session = match sessions.iter_mut().position(|s| s.label == label) {
                Some(at) => &mut sessions[at],
                None => {
                    sessions.push(DrivingSession {
                        label,
                        controlling: Vec::new(),
                        reading: Vec::new(),
                    });
                    sessions.last_mut().expect("just pushed")
                }
            };
            let list = if driven { &mut session.controlling } else { &mut session.reading };
            if !list.contains(&app) {
                list.push(app);
            }
        }
        // A session that photographed something nobody could attribute to a
        // process still has to appear. Naming it vaguely is worth far more than
        // the menu bar staying dark, which is the failure this exists to end.
        for label in grants
            .capturing()
            .into_iter()
            .map(|owner| owner.strip_prefix("oximux-").unwrap_or(&owner).to_string())
        {
            if !sessions.iter().any(|s| s.label == label) {
                sessions.push(DrivingSession {
                    label,
                    controlling: Vec::new(),
                    reading: vec![UNNAMED_WINDOW.to_string()],
                });
            }
        }

        sessions.sort_by(|a, b| a.label.cmp(&b.label));
        for session in &mut sessions {
            session.controlling.sort();
            // Driving is the stronger claim, so an app in both is reported only
            // as driven. Ordering the two loops the other way would be a
            // silent downgrade of exactly the case the user most needs named.
            session.reading.retain(|app| !session.controlling.contains(app));
            session.reading.sort();
        }
        Self { sessions }
    }

    pub fn is_idle(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Distinct apps across every session, driven and read alike. Two agents
    /// driving two builds of the same program is the expected shape here, so
    /// this counts names rather than grants.
    ///
    /// Used for the title, where the question is "how much of my machine is
    /// involved" and the answer does not turn on which kind of involvement.
    pub fn distinct_apps(&self) -> Vec<&str> {
        let mut apps: Vec<&str> = self
            .sessions
            .iter()
            .flat_map(|s| s.controlling.iter().chain(s.reading.iter()).map(String::as_str))
            .collect();
        apps.sort_unstable();
        apps.dedup();
        apps
    }

    fn distinct(&self, pick: fn(&DrivingSession) -> &Vec<String>) -> Vec<&str> {
        let mut apps: Vec<&str> =
            self.sessions.iter().flat_map(|s| pick(s).iter().map(String::as_str)).collect();
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
    ///
    /// Reading is reported in its own clause rather than folded in with driving.
    /// A user who reads "controlling Safari" about a window that was only
    /// photographed learns something false, and the credibility of the one
    /// indicator they have depends on it never doing that.
    pub fn summary(&self) -> String {
        let controlling = self.distinct(|s| &s.controlling);
        let reading = self.distinct(|s| &s.reading);
        if controlling.is_empty() && reading.is_empty() {
            return "No agent is using this Mac's screen".to_string();
        }
        let who = if self.sessions.len() == 1 {
            "An agent is".to_string()
        } else {
            format!("{} agents are", self.sessions.len())
        };
        let mut clauses = Vec::new();
        if !controlling.is_empty() {
            clauses.push(format!("controlling {}", name_or_count(&controlling)));
        }
        if !reading.is_empty() {
            clauses.push(format!("reading {}", name_or_count(&reading)));
        }
        format!("{who} {}", clauses.join(" and "))
    }

    /// Per-agent detail, for a menu that has room for it. One line each, so
    /// several agents working at once stays readable.
    ///
    /// Shown for a single agent only when it is doing both things at once —
    /// otherwise it would restate [`Self::summary`] with an id most users have
    /// no use for. Doing both is exactly when one line cannot carry it.
    pub fn detail_lines(&self) -> Vec<String> {
        let mixed = self.sessions.iter().any(|s| !s.controlling.is_empty() && !s.reading.is_empty());
        if self.sessions.len() < 2 && !mixed {
            return Vec::new();
        }
        self.sessions
            .iter()
            .map(|session| {
                let mut parts = Vec::new();
                if !session.controlling.is_empty() {
                    let apps: Vec<&str> = session.controlling.iter().map(String::as_str).collect();
                    parts.push(format!("controlling {}", join_names(&apps)));
                }
                if !session.reading.is_empty() {
                    let apps: Vec<&str> = session.reading.iter().map(String::as_str).collect();
                    parts.push(format!("reading {}", join_names(&apps)));
                }
                format!("{} — {}", session.label, parts.join(", "))
            })
            .collect()
    }
}

/// Name them while there are few enough to name; past that a count carries more
/// than a truncated list.
fn name_or_count(apps: &[&str]) -> String {
    if apps.len() > 3 {
        return format!("{} apps", apps.len());
    }
    join_names(apps)
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

    /// The state during an actual run: `agent` may drive `pid`, *and* is doing
    /// so. Both halves are required, because a grant on its own is consent and
    /// consent outlives the turn that used it — see [`Driving::read`].
    ///
    /// Worth a helper rather than two lines at each call site: a test that
    /// granted without marking would still pass everywhere the assertion is
    /// "nothing is being driven", while having stopped testing the reason.
    fn driving_grant(table: &GrantTable, pid: u32, agent: &str) {
        let session = SessionId::for_agent(agent);
        table.grant(pid, &session);
        table.begin_driving_turn(&session);
    }

    #[test]
    fn nothing_granted_reads_as_idle() {
        let (_dir, table) = table();
        let driving = Driving::read(&table);
        assert!(driving.is_idle());
        assert!(driving.detail_lines().is_empty());
        assert_eq!(driving.summary(), "No agent is using this Mac's screen");
    }

    #[test]
    fn a_granted_target_is_named_by_its_program() {
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        driving_grant(&table, target.pid(), "chat-1");

        let driving = Driving::read(&table);
        assert!(!driving.is_idle());
        assert_eq!(driving.sessions.len(), 1);
        assert_eq!(driving.sessions[0].label, "chat-1");
        assert_eq!(driving.sessions[0].controlling, vec!["sleep".to_string()]);
        assert!(driving.sessions[0].reading.is_empty(), "driving is not reading");
        assert_eq!(driving.summary(), "An agent is controlling sleep");
    }

    #[test]
    fn a_single_agent_gets_no_redundant_detail_line() {
        // The detail list exists to disambiguate several agents. With one, it
        // would only restate the summary alongside an id nobody asked for.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        driving_grant(&table, target.pid(), "chat-1");
        assert!(!Driving::read(&table).is_idle(), "the agent must be driving for this to mean anything");
        assert!(Driving::read(&table).detail_lines().is_empty());
    }

    #[test]
    fn two_agents_are_reported_separately_and_in_a_stable_order() {
        // The parallelism premise: the user must be able to tell which of their
        // agents is doing what, and the list must not reshuffle on redraw.
        let (_dir, table) = table();
        let (a, b) = (Target::spawn("/bin/sleep", &["120"]), Target::spawn("/bin/cat", &[]));
        driving_grant(&table, b.pid(), "chat-2");
        driving_grant(&table, a.pid(), "chat-1");

        let driving = Driving::read(&table);
        let labels: Vec<&str> = driving
            .sessions
            .iter()
            .map(|s| s.label.as_str())
            .collect();
        assert_eq!(labels, vec!["chat-1", "chat-2"]);
        assert_eq!(
            driving.detail_lines(),
            vec![
                "chat-1 — controlling sleep".to_string(),
                "chat-2 — controlling cat".to_string()
            ]
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
        driving_grant(&table, pid, "chat-1");
        assert!(!Driving::read(&table).is_idle(), "must be driving before the pid dies");
        drop(target);

        assert!(Driving::read(&table).is_idle());
    }

    #[test]
    fn a_grant_with_no_live_turn_is_not_reported_as_driving() {
        // The whole reason the two are stored apart. A grant lasts as long as
        // the chat that holds it, so a chat which drove once and then went idle
        // still holds one — and anything reading grants alone would keep saying
        // it is driving, hold the Escape tap armed, and swallow the key
        // machine-wide until the tab was closed.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        table.grant(target.pid(), &SessionId::for_agent("chat-1"));

        assert!(Driving::read(&table).is_idle());
        assert_eq!(Driving::read(&table).summary(), "No agent is using this Mac's screen");
    }

    #[test]
    fn the_turn_boundary_stops_the_claim_without_dropping_the_grant() {
        // Ending a turn must not cost the user their approval: the next turn
        // drives the same target without a second consent card. Only the claim
        // that something is happening right now goes away.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        let session = SessionId::for_agent("chat-1");
        driving_grant(&table, target.pid(), "chat-1");
        assert!(!Driving::read(&table).is_idle());

        table.end_turn_activity(&session);
        assert!(Driving::read(&table).is_idle(), "the turn ended, so nothing is being driven");
        assert_eq!(
            table.check(target.pid(), &session),
            crate::grants::Verdict::Granted,
            "but the approval survives the turn that used it"
        );

        // And a later turn re-arms without asking again.
        table.begin_driving_turn(&session);
        assert!(!Driving::read(&table).is_idle());
    }

    #[test]
    fn a_photographed_window_is_reported_as_read_not_controlled() {
        // The distinction the whole split exists for. A capture needs no grant,
        // so the agent cannot click this window — saying it "controls" it would
        // be a louder claim than the truth, and the indicator's usefulness rests
        // on it never overstating.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        table.note_capture(Some(target.pid()), &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        assert!(!driving.is_idle(), "a capture is not nothing");
        assert!(driving.sessions[0].controlling.is_empty());
        assert_eq!(driving.sessions[0].reading, vec!["sleep".to_string()]);
        assert_eq!(driving.summary(), "An agent is reading sleep");
    }

    #[test]
    fn a_capture_that_named_no_process_still_shows() {
        // `zoom` takes a window id and need not carry a pid, and a read with no
        // pid is allowed. Reporting only the captures that happen to name a
        // process would leave the same dark-menu-bar hole, just narrower.
        let (_dir, table) = table();
        table.note_capture(None, &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        assert!(!driving.is_idle());
        assert_eq!(driving.summary(), "An agent is reading a window");
    }

    #[test]
    fn driving_and_reading_are_reported_in_one_breath() {
        // The common shape of a real run: the agent photographs a window to find
        // its buttons, then clicks a different app it was granted.
        let (_dir, table) = table();
        let (driven, read) = (Target::spawn("/bin/sleep", &["120"]), Target::spawn("/bin/cat", &[]));
        driving_grant(&table, driven.pid(), "chat-1");
        table.note_capture(Some(read.pid()), &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        assert_eq!(driving.summary(), "An agent is controlling sleep and reading cat");
        assert_eq!(
            driving.detail_lines(),
            vec!["chat-1 — controlling sleep, reading cat".to_string()],
            "one agent doing both is exactly when a single summary line cannot carry it"
        );
    }

    #[test]
    fn an_app_both_driven_and_photographed_is_reported_as_driven() {
        // Reads are how an agent finds what to click, so the same window is
        // routinely both. Listing it twice would double-count it in the title
        // and read as two separate things happening.
        let (_dir, table) = table();
        let target = Target::spawn("/bin/sleep", &["120"]);
        driving_grant(&table, target.pid(), "chat-1");
        table.note_capture(Some(target.pid()), &SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        assert_eq!(driving.sessions[0].controlling, vec!["sleep".to_string()]);
        assert!(driving.sessions[0].reading.is_empty(), "the stronger claim wins");
        assert_eq!(driving.distinct_apps(), vec!["sleep"], "and it counts once");
    }

    #[test]
    fn one_agent_going_idle_leaves_the_other_driving() {
        // Two agents run at different speeds, so their turns end at different
        // times. The first to finish must not take the indicator — or the kill
        // switch — down with it while the second is still clicking.
        let (_dir, table) = table();
        let (a, b) = (Target::spawn("/bin/sleep", &["120"]), Target::spawn("/bin/cat", &[]));
        driving_grant(&table, a.pid(), "chat-1");
        driving_grant(&table, b.pid(), "chat-2");

        table.end_turn_activity(&SessionId::for_agent("chat-1"));

        let driving = Driving::read(&table);
        let labels: Vec<&str> = driving.sessions.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["chat-2"]);
    }

    #[test]
    fn many_apps_are_counted_rather_than_listed() {
        // A truncated list is detail nobody can act on; the actionable fact at
        // that point is simply that a lot is happening.
        let driving = Driving {
            sessions: vec![DrivingSession {
                label: "chat-1".into(),
                controlling: vec!["a".into(), "b".into(), "c".into(), "d".into()],
                reading: Vec::new(),
            }],
        };
        assert_eq!(driving.summary(), "An agent is controlling 4 apps");
    }

    #[test]
    fn up_to_three_apps_are_named() {
        let driving = Driving {
            sessions: vec![DrivingSession {
                label: "chat-1".into(),
                controlling: vec!["Safari".into(), "Calculator".into(), "Notes".into()],
                reading: Vec::new(),
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
                    controlling: vec!["my-app".into()],
                    reading: Vec::new(),
                },
                DrivingSession {
                    label: "chat-2".into(),
                    controlling: vec!["my-app".into()],
                    reading: Vec::new(),
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
