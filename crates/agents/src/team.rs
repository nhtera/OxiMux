//! Team runs: one fan-out of roles, each worked by its own agent session,
//! recorded host-side so the run outlives the process that started it.
//!
//! **Why this is host state and not a script's bookkeeping.** A client-side
//! orchestrator holds its run in memory: kill it, or restart the host, and the
//! run is gone while the sessions it started carry on unattached. Keeping the
//! run in the host's own database makes the opposite true — a host that comes
//! back reads its open roles, re-associates the ones whose sessions still
//! exist, and settles the ones whose sessions do not. That is the whole reason
//! the table exists.
//!
//! The store has no opinion about *how* roles are started; that is the host's
//! launcher. It records what was started, what each role reported, and when.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// How many roles one run may carry.
///
/// Eight because that is where a fan-out stops being a team and starts being a
/// load test: every role is a live agent process against the same project, and
/// the point of the cap is that a typo in a scripted loop cannot start fifty.
pub const MAX_ROLES: usize = 8;

/// Where one role stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamRoleStatus {
    /// Its session is open and it has not reported.
    Running,
    /// It reported success.
    Done,
    /// It reported failure, or never started.
    Failed,
}

impl TeamRoleStatus {
    fn as_text(self) -> &'static str {
        match self {
            TeamRoleStatus::Running => "running",
            TeamRoleStatus::Done => "done",
            TeamRoleStatus::Failed => "failed",
        }
    }

    /// An unrecognized stored value reads as [`TeamRoleStatus::Failed`], never
    /// as `Running`: a row this build cannot interpret must not hold a run open
    /// forever waiting for a report that can never be matched.
    fn from_text(text: &str) -> Self {
        match text {
            "running" => TeamRoleStatus::Running,
            "done" => TeamRoleStatus::Done,
            _ => TeamRoleStatus::Failed,
        }
    }
}

/// One role within a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRole {
    pub name: String,
    /// The session working it, when one opened.
    pub session_id: Option<String>,
    pub status: TeamRoleStatus,
    pub summary: Option<String>,
    pub updated_at: DateTime<Local>,
}

/// A run and its roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRun {
    pub id: String,
    pub name: String,
    pub cwd: String,
    pub created_at: DateTime<Local>,
    pub roles: Vec<TeamRole>,
}

impl TeamRun {
    /// Whether every role has reported.
    ///
    /// Derived rather than stored, so two clients can never disagree about
    /// whether a run is finished and no write path can forget to update it.
    /// A run with no roles is closed — vacuously, and it is not a state any
    /// create path can produce.
    pub fn closed(&self) -> bool {
        self.roles.iter().all(|r| r.status != TeamRoleStatus::Running)
    }
}

/// What a role looked like the moment it was created.
#[derive(Debug, Clone)]
pub struct NewTeamRole {
    pub name: String,
    /// The session started for it, or `None` when the start failed.
    pub session_id: Option<String>,
    /// `Failed` when the session could not be started — recorded immediately so
    /// a run whose roles all failed to launch is closed rather than open with
    /// nothing running.
    pub status: TeamRoleStatus,
    pub summary: Option<String>,
}

/// Team runs, backed by the app's SQLite database. Cloning shares the
/// connection.
#[derive(Clone)]
pub struct TeamStore {
    conn: Arc<Mutex<Connection>>,
}

impl TeamStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record a run and its roles in one transaction, so a crash mid-write
    /// cannot leave a run with half its roles — which would read as a team that
    /// finished the ones it never had.
    pub fn create(
        &self,
        name: &str,
        cwd: &str,
        roles: &[NewTeamRole],
        now: DateTime<Local>,
    ) -> Result<TeamRun> {
        anyhow::ensure!(!roles.is_empty(), "a team run needs at least one role");
        anyhow::ensure!(
            roles.len() <= MAX_ROLES,
            "a team run may have at most {MAX_ROLES} roles"
        );
        let id = format!("run-{}", super::schedule::store::random_id());
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction().context("begin team run")?;
        tx.execute(
            "INSERT INTO team_runs (id, name, cwd, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, name, cwd, now.to_rfc3339()],
        )
        .context("insert team run")?;
        for role in roles {
            tx.execute(
                "INSERT INTO team_run_roles (run_id, name, session_id, status, summary, \
                 updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    role.name,
                    role.session_id,
                    role.status.as_text(),
                    role.summary,
                    now.to_rfc3339(),
                ],
            )
            .context("insert team role")?;
        }
        tx.commit().context("commit team run")?;
        Ok(TeamRun {
            id,
            name: name.to_string(),
            cwd: cwd.to_string(),
            created_at: now,
            roles: roles
                .iter()
                .map(|r| TeamRole {
                    name: r.name.clone(),
                    session_id: r.session_id.clone(),
                    status: r.status,
                    summary: r.summary.clone(),
                    updated_at: now,
                })
                .collect(),
        })
    }

    /// One run with its roles, or `None`.
    pub fn get(&self, run_id: &str) -> Result<Option<TeamRun>> {
        let conn = self.conn.lock().unwrap();
        let run = conn
            .query_row(
                "SELECT id, name, cwd, created_at FROM team_runs WHERE id = ?1",
                params![run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()
            .context("read team run")?;
        let Some((id, name, cwd, created_at)) = run else { return Ok(None) };
        let roles = read_roles(&conn, &id)?;
        Ok(Some(TeamRun {
            id,
            name,
            cwd,
            created_at: parse_time(&created_at),
            roles,
        }))
    }

    /// Every run, newest first.
    pub fn list(&self, limit: u32) -> Result<Vec<TeamRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, cwd, created_at FROM team_runs \
                 ORDER BY created_at DESC LIMIT ?1",
            )
            .context("prepare team run list")?;
        let heads = stmt
            .query_map(params![limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .context("query team runs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read team runs")?;
        heads
            .into_iter()
            .map(|(id, name, cwd, created_at)| {
                let roles = read_roles(&conn, &id)?;
                Ok(TeamRun { id, name, cwd, created_at: parse_time(&created_at), roles })
            })
            .collect()
    }

    /// Settle one role. Returns `false` when the run has no such role.
    ///
    /// A report **overwrites** an earlier one rather than being refused: a role
    /// that reported failure and then recovered should be able to say so, and
    /// nothing downstream depends on a report being final. What it cannot do is
    /// move a role back to `Running` — see [`super::team::TeamReport`].
    pub fn report(
        &self,
        run_id: &str,
        role: &str,
        ok: bool,
        summary: Option<&str>,
        now: DateTime<Local>,
    ) -> Result<bool> {
        let status = if ok { TeamRoleStatus::Done } else { TeamRoleStatus::Failed };
        let changed = self
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE team_run_roles SET status = ?3, summary = ?4, updated_at = ?5 \
                 WHERE run_id = ?1 AND name = ?2",
                params![run_id, role, status.as_text(), summary, now.to_rfc3339()],
            )
            .context("settle team role")?;
        Ok(changed > 0)
    }

    /// The session working one role, if it has one — the scope a report is
    /// authorized against.
    pub fn session_for_role(&self, run_id: &str, role: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT session_id FROM team_run_roles WHERE run_id = ?1 AND name = ?2",
                params![run_id, role],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .context("read role session")?
            .flatten())
    }

    /// Every role still open, with the session it was working.
    ///
    /// The boot pass reads this to decide which roles can carry on (their
    /// session still exists) and which must be settled (it does not).
    pub fn open_roles(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT run_id, name, session_id FROM team_run_roles WHERE status = 'running'",
            )
            .context("prepare open roles")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .context("query open roles")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("read open roles")?;
        Ok(rows)
    }

    /// Boot pass: settle every open role whose session this host no longer has.
    ///
    /// Roles whose sessions survive are **left running** — that is the
    /// restart-survival property, and marking them would throw away work still
    /// in flight. Returns the roles it settled, so the host can log what a
    /// restart cost.
    pub fn recover_open_roles(
        &self,
        now: DateTime<Local>,
        session_exists: impl Fn(&str) -> bool,
    ) -> Result<Vec<(String, String)>> {
        let mut settled = Vec::new();
        for (run_id, role, session_id) in self.open_roles()? {
            let gone = match &session_id {
                Some(id) => !session_exists(id),
                // A role that never got a session is not waiting on anything.
                None => true,
            };
            if !gone {
                continue;
            }
            self.report(
                &run_id,
                &role,
                false,
                Some("the host restarted and this role's session is gone"),
                now,
            )?;
            settled.push((run_id, role));
        }
        Ok(settled)
    }
}

/// One run's roles, ordered as they were created.
fn read_roles(conn: &Connection, run_id: &str) -> Result<Vec<TeamRole>> {
    let mut stmt = conn
        .prepare(
            "SELECT name, session_id, status, summary, updated_at FROM team_run_roles \
             WHERE run_id = ?1 ORDER BY rowid",
        )
        .context("prepare roles")?;
    let rows = stmt
        .query_map(params![run_id], |row| Ok(row_to_role(row)))
        .context("query roles")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("read roles")?;
    Ok(rows)
}

fn row_to_role(row: &Row<'_>) -> TeamRole {
    TeamRole {
        name: row.get::<_, String>(0).unwrap_or_default(),
        session_id: row.get::<_, Option<String>>(1).unwrap_or(None),
        status: TeamRoleStatus::from_text(
            &row.get::<_, String>(2).unwrap_or_else(|_| "failed".into()),
        ),
        summary: row.get::<_, Option<String>>(3).unwrap_or(None),
        updated_at: parse_time(&row.get::<_, String>(4).unwrap_or_default()),
    }
}

/// An unparseable stored instant reads as *now* rather than failing the row:
/// a timestamp is display metadata here, and losing a whole run over it would
/// be a worse answer than showing one with a wrong clock.
fn parse_time(text: &str) -> DateTime<Local> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.with_timezone(&Local))
        .unwrap_or_else(|_| Local::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn store() -> TeamStore {
        let db = oximux_storage::db::open_memory().expect("open in-memory db");
        TeamStore::new(db.conn())
    }

    fn at(s: &str) -> DateTime<Local> {
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").expect("parse");
        Local.from_local_datetime(&naive).single().expect("unambiguous")
    }

    fn role(name: &str, session: Option<&str>) -> NewTeamRole {
        NewTeamRole {
            name: name.to_string(),
            session_id: session.map(str::to_string),
            status: TeamRoleStatus::Running,
            summary: None,
        }
    }

    #[test]
    fn a_run_records_its_roles_and_reads_back_whole() {
        let store = store();
        let made = store
            .create(
                "ship it",
                "/work",
                &[role("backend", Some("sess-1")), role("frontend", Some("sess-2"))],
                at("2026-08-04 09:00:00"),
            )
            .expect("create");

        let read = store.get(&made.id).expect("get").expect("exists");
        assert_eq!(read.name, "ship it");
        assert_eq!(read.roles.len(), 2);
        assert_eq!(read.roles[0].name, "backend", "roles keep their creation order");
        assert!(!read.closed(), "nothing has reported");
    }

    /// The convergence property `team status` depends on: a run is closed
    /// exactly when its last role reports, whatever each of them reported.
    #[test]
    fn a_run_closes_when_its_last_role_reports() {
        let store = store();
        let made = store
            .create(
                "ship it",
                "/work",
                &[role("backend", Some("sess-1")), role("frontend", Some("sess-2"))],
                at("2026-08-04 09:00:00"),
            )
            .expect("create");

        assert!(store.report(&made.id, "backend", true, Some("done"), at("2026-08-04 09:05:00")).unwrap());
        assert!(!store.get(&made.id).unwrap().unwrap().closed(), "one still running");

        assert!(store.report(&made.id, "frontend", false, Some("blocked"), at("2026-08-04 09:06:00")).unwrap());
        let read = store.get(&made.id).unwrap().unwrap();
        assert!(read.closed(), "every role reported");
        assert_eq!(read.roles[1].status, TeamRoleStatus::Failed);
        assert_eq!(read.roles[1].summary.as_deref(), Some("blocked"));
    }

    #[test]
    fn reporting_an_unknown_role_changes_nothing() {
        let store = store();
        let made = store
            .create("ship it", "/work", &[role("backend", Some("sess-1"))], at("2026-08-04 09:00:00"))
            .expect("create");
        assert!(!store.report(&made.id, "nobody", true, None, at("2026-08-04 09:05:00")).unwrap());
        assert!(!store.get(&made.id).unwrap().unwrap().closed());
    }

    /// The restart-survival property. A role whose session is still there keeps
    /// running; one whose session is gone is settled with a reason, so the run
    /// converges instead of hanging on a session nothing can report for.
    #[test]
    fn boot_keeps_live_roles_and_settles_orphaned_ones() {
        let store = store();
        let made = store
            .create(
                "ship it",
                "/work",
                &[
                    role("backend", Some("sess-live")),
                    role("frontend", Some("sess-gone")),
                    role("docs", None),
                ],
                at("2026-08-04 09:00:00"),
            )
            .expect("create");

        let settled = store
            .recover_open_roles(at("2026-08-04 10:00:00"), |id| id == "sess-live")
            .expect("recover");
        assert_eq!(settled.len(), 2, "the missing session and the never-started role");

        let read = store.get(&made.id).unwrap().unwrap();
        assert_eq!(read.roles[0].status, TeamRoleStatus::Running, "a live role carries on");
        assert_eq!(read.roles[1].status, TeamRoleStatus::Failed);
        assert!(
            read.roles[1].summary.as_deref().unwrap_or_default().contains("restarted"),
            "says what happened: {:?}",
            read.roles[1].summary
        );
        assert!(!read.closed(), "the surviving role still holds the run open");
    }

    #[test]
    fn a_run_with_no_roles_is_refused() {
        assert!(store().create("empty", "/work", &[], at("2026-08-04 09:00:00")).is_err());
    }

    #[test]
    fn a_run_over_the_role_cap_is_refused() {
        let roles: Vec<NewTeamRole> =
            (0..=MAX_ROLES).map(|i| role(&format!("r{i}"), None)).collect();
        assert!(store().create("big", "/work", &roles, at("2026-08-04 09:00:00")).is_err());
    }

    /// The scope a report is authorized against.
    #[test]
    fn a_roles_session_is_readable_for_the_authorization_check() {
        let store = store();
        let made = store
            .create("ship it", "/work", &[role("backend", Some("sess-1"))], at("2026-08-04 09:00:00"))
            .expect("create");
        assert_eq!(store.session_for_role(&made.id, "backend").unwrap().as_deref(), Some("sess-1"));
        assert_eq!(store.session_for_role(&made.id, "nobody").unwrap(), None);
    }

    #[test]
    fn runs_list_newest_first() {
        let store = store();
        store.create("first", "/work", &[role("a", None)], at("2026-08-04 09:00:00")).unwrap();
        store.create("second", "/work", &[role("a", None)], at("2026-08-04 10:00:00")).unwrap();
        let listed = store.list(10).expect("list");
        assert_eq!(listed[0].name, "second");
        assert_eq!(listed[1].name, "first");
    }
}
