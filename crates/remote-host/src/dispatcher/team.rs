//! The team-run RPC handlers — open a multi-role fan-out, settle a role, read
//! a run's board.
//!
//! Opening a run starts several sessions at once, so it carries
//! [`may_manage_teams`](crate::auth::AuthStore::may_manage_teams) — the same
//! two gates as `CreateSession`. Everything after that is deliberately reachable
//! from inside a role: a confined agent settles its own row and reads the board
//! of the run it belongs to, because a team where the members cannot see each
//! other is not a team.

use oximux_agents::team::{NewTeamRole, TeamRole, TeamRoleStatus, TeamRun};
use oximux_remote_proto::messages::{
    TeamReportReq, TeamRoleStatusWire, TeamRoleWire, TeamRunCreateReq, TeamRunWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

/// How many runs `TeamList` returns. A board, not an archive — a caller wanting
/// history reads a specific run by id.
const LIST_LIMIT: u32 = 50;

impl Dispatcher {
    /// Open a run: start one session per role, then record the whole thing.
    ///
    /// A role whose session fails to start is recorded as `Failed` rather than
    /// aborting the run. Half a team is a real outcome the caller can act on;
    /// unwinding the sessions that *did* start would throw away work to report
    /// a tidier error.
    pub(super) async fn team_run_create(&self, peer: &Peer, req: TeamRunCreateReq) -> Response {
        if !self.auth.may_manage_teams(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let (Some(teams), Some(launcher)) = (self.teams.as_ref(), self.launcher.as_ref()) else {
            return Response::Error(RpcError::Unsupported);
        };
        if req.roles.is_empty() {
            return Response::Error(RpcError::BadRequest("a run needs at least one role".into()));
        }
        if req.roles.len() > oximux_agents::team::MAX_ROLES {
            return Response::Error(RpcError::BadRequest(format!(
                "a run may have at most {} roles",
                oximux_agents::team::MAX_ROLES
            )));
        }
        // Duplicate role names would collide on the table's (run, name) key, so
        // one role would silently overwrite another's session.
        let mut seen = std::collections::HashSet::new();
        if !req.roles.iter().all(|r| seen.insert(r.name.as_str())) {
            return Response::Error(RpcError::BadRequest("role names must be distinct".into()));
        }

        let mut roles = Vec::with_capacity(req.roles.len());
        for spec in &req.roles {
            // Each role gets its own worktree when asked, so two roles editing
            // the same files do not fight. A worktree that cannot be created
            // fails only its own role.
            let cwd = if req.worktree_each {
                match self.worktree_for_role(&req.cwd, &req.name, &spec.name).await {
                    Ok(path) => path,
                    Err(detail) => {
                        roles.push(NewTeamRole {
                            name: spec.name.clone(),
                            session_id: None,
                            status: TeamRoleStatus::Failed,
                            summary: Some(detail),
                        });
                        continue;
                    }
                }
            } else {
                req.cwd.clone()
            };
            match launcher.create(&cwd, req.agent_id.as_deref(), None).await {
                Ok(session_id) => {
                    // The opening instruction goes in like any other prompt; a
                    // session that opened but cannot take it still names itself,
                    // so the board points at something inspectable.
                    let status = match self.registry.get(&session_id) {
                        Some(handle) => match handle.send_prompt(&spec.prompt, &[]) {
                            Ok(()) => TeamRoleStatus::Running,
                            Err(_) => TeamRoleStatus::Failed,
                        },
                        None => TeamRoleStatus::Failed,
                    };
                    let summary = (status == TeamRoleStatus::Failed)
                        .then(|| "the session opened but its prompt could not be delivered".into());
                    roles.push(NewTeamRole {
                        name: spec.name.clone(),
                        session_id: Some(session_id),
                        status,
                        summary,
                    });
                }
                Err(err) => roles.push(NewTeamRole {
                    name: spec.name.clone(),
                    session_id: None,
                    status: TeamRoleStatus::Failed,
                    summary: Some(launch_detail(&err).to_string()),
                }),
            }
        }

        match teams.create(&req.name, &req.cwd, &roles, self.now_local()) {
            Ok(run) => Response::TeamRun(run_to_wire(&run)),
            Err(e) => {
                // The store error can name the database path; log it, return
                // the category.
                tracing::warn!(error = %e, "recording a team run failed");
                Response::Error(RpcError::Internal("could not record the run".into()))
            }
        }
    }

    /// A role reporting its own outcome.
    pub(super) fn team_report(&self, peer: &Peer, req: TeamReportReq) -> Response {
        let Some(teams) = self.teams.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        // Authorized against the session working THAT role, read from the
        // store: a confined agent may settle the row it is actually working and
        // no other, and cannot settle a teammate's by naming it.
        let role_session = match teams.session_for_role(&req.run_id, &req.role) {
            Ok(session) => session,
            Err(e) => {
                tracing::warn!(error = %e, "reading a team role failed");
                return Response::Error(RpcError::Internal("could not read the run".into()));
            }
        };
        let allowed = match &role_session {
            Some(session_id) => self.auth.may_write(peer, session_id),
            // A role that never got a session can only be settled by someone
            // who could have created the run in the first place.
            None => self.auth.may_manage_teams(peer),
        };
        if !allowed {
            return Response::Error(RpcError::Unauthorized);
        }
        match teams.report(
            &req.run_id,
            &req.role,
            req.ok,
            req.summary.as_deref(),
            self.now_local(),
        ) {
            Ok(true) => self.team_status_unchecked(&req.run_id),
            Ok(false) => Response::Error(RpcError::BadRequest("no such role in that run".into())),
            Err(e) => {
                tracing::warn!(error = %e, "settling a team role failed");
                Response::Error(RpcError::Internal("could not record the report".into()))
            }
        }
    }

    /// One run's board.
    pub(super) fn team_status(&self, peer: &Peer, run_id: &str) -> Response {
        let Some(teams) = self.teams.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        let run = match teams.get(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return Response::Error(RpcError::BadRequest("no such run".into())),
            Err(e) => {
                tracing::warn!(error = %e, "reading a team run failed");
                return Response::Error(RpcError::Internal("could not read the run".into()));
            }
        };
        let sessions: Vec<String> = run.roles.iter().filter_map(|r| r.session_id.clone()).collect();
        if !self.auth.may_read_team_run(peer, &sessions) {
            return Response::Error(RpcError::Unauthorized);
        }
        Response::TeamRun(run_to_wire(&run))
    }

    /// Every run this host holds.
    ///
    /// Full scope only: the list spans every session on the host, so there is
    /// no narrowing that would leave a confined caller a coherent view. Such a
    /// caller reads its own run by id instead, which
    /// [`team_status`](Self::team_status) does allow.
    pub(super) fn team_list(&self, peer: &Peer) -> Response {
        if !self.auth.may_read_schedules(peer) {
            return Response::Error(RpcError::Unauthorized);
        }
        let Some(teams) = self.teams.as_ref() else {
            return Response::Error(RpcError::Unsupported);
        };
        match teams.list(LIST_LIMIT) {
            Ok(runs) => Response::TeamRuns(runs.iter().map(run_to_wire).collect()),
            Err(e) => {
                tracing::warn!(error = %e, "listing team runs failed");
                Response::Error(RpcError::Internal("could not read the runs".into()))
            }
        }
    }

    /// The board, after this caller's authorization has already been settled by
    /// the write it just made.
    fn team_status_unchecked(&self, run_id: &str) -> Response {
        match self.teams.as_ref().map(|t| t.get(run_id)) {
            Some(Ok(Some(run))) => Response::TeamRun(run_to_wire(&run)),
            _ => Response::Ack,
        }
    }

    /// A per-role worktree under the run's project. Returns the path, or why
    /// the role could not have one.
    async fn worktree_for_role(
        &self,
        project: &str,
        run_name: &str,
        role: &str,
    ) -> Result<String, String> {
        let Some(worktrees) = self.worktrees.as_ref() else {
            return Err("this host cannot create worktrees".into());
        };
        // The host derives the path from the slug; the client never supplies
        // one, exactly as `CreateWorktree` requires.
        let slug = format!("{}-{}", slugify(run_name), slugify(role));
        worktrees
            .create(project, &slug)
            .await
            .map(|row| row.path)
            .map_err(|_| "the role's worktree could not be created".to_string())
    }
}

/// A launch failure as the board should read it.
fn launch_detail(err: &crate::launcher::LaunchError) -> &'static str {
    match err {
        crate::launcher::LaunchError::BadWorkingDirectory => {
            "that working directory is not usable"
        }
        crate::launcher::LaunchError::Unavailable => {
            "the host could not start a session right now"
        }
        crate::launcher::LaunchError::Failed => "the agent could not be started",
        crate::launcher::LaunchError::ModelUnsupported => {
            "this agent cannot be given a model when it starts"
        }
    }
}

/// Lowercase, hyphen-joined, alphanumerics only — the same shape a worktree
/// slug takes elsewhere, so a role's directory name is predictable and safe to
/// put in a path.
fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() { "role".into() } else { out }
}

fn run_to_wire(run: &TeamRun) -> TeamRunWire {
    TeamRunWire {
        id: run.id.clone(),
        name: run.name.clone(),
        cwd: run.cwd.clone(),
        created_at: run.created_at.to_rfc3339(),
        closed: run.closed(),
        roles: run.roles.iter().map(role_to_wire).collect(),
    }
}

fn role_to_wire(role: &TeamRole) -> TeamRoleWire {
    TeamRoleWire {
        name: role.name.clone(),
        session_id: role.session_id.clone(),
        status: match role.status {
            TeamRoleStatus::Running => TeamRoleStatusWire::Running,
            TeamRoleStatus::Done => TeamRoleStatusWire::Done,
            TeamRoleStatus::Failed => TeamRoleStatusWire::Failed,
        },
        summary: role.summary.clone(),
        updated_at: role.updated_at.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_path_safe_and_never_empty() {
        assert_eq!(slugify("Ship It!"), "ship-it");
        assert_eq!(slugify("back/end"), "back-end");
        assert_eq!(slugify("../.."), "role", "a path-traversal name yields a literal");
        assert_eq!(slugify("  "), "role");
    }
}
