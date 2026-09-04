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
    TeamReportReq, TeamRoleStatusWire, TeamRoleV2Wire, TeamRoleWire, TeamRunCreateReq,
    TeamRunCreateV2Req, TeamRunV2Wire, TeamRunWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

/// One role as the launch loop needs it, whichever verb asked for the run.
///
/// The two create verbs differ only in what a role may name, so they normalize
/// into this and share every line after that — the alternative is two copies of
/// a loop that starts sessions and creates worktrees, which is the last code in
/// this file that should exist twice.
struct RoleSpec {
    name: String,
    prompt: String,
    agent_id: Option<String>,
    model: Option<String>,
}

/// A run request, normalized.
struct RunSpec {
    name: String,
    cwd: String,
    /// The agent for roles that name none of their own.
    agent_id: Option<String>,
    worktree_each: bool,
    roles: Vec<RoleSpec>,
}

impl From<TeamRunCreateReq> for RunSpec {
    /// The v18 shape: one agent for the whole run, no per-role model.
    fn from(req: TeamRunCreateReq) -> Self {
        RunSpec {
            name: req.name,
            cwd: req.cwd,
            agent_id: req.agent_id,
            worktree_each: req.worktree_each,
            roles: req
                .roles
                .into_iter()
                .map(|r| RoleSpec {
                    name: r.name,
                    prompt: r.prompt,
                    agent_id: None,
                    model: None,
                })
                .collect(),
        }
    }
}

impl From<TeamRunCreateV2Req> for RunSpec {
    fn from(req: TeamRunCreateV2Req) -> Self {
        RunSpec {
            name: req.name,
            cwd: req.cwd,
            agent_id: req.agent_id,
            worktree_each: req.worktree_each,
            roles: req
                .roles
                .into_iter()
                .map(|r| RoleSpec {
                    name: r.name,
                    prompt: r.prompt,
                    agent_id: r.agent_id,
                    model: r.model,
                })
                .collect(),
        }
    }
}

/// How many runs `TeamList` returns. A board, not an archive — a caller wanting
/// history reads a specific run by id.
const LIST_LIMIT: u32 = 50;

impl Dispatcher {
    /// Open a run in the v18 shape: one agent covers every role.
    pub(super) async fn team_run_create(&self, peer: &Peer, req: TeamRunCreateReq) -> Response {
        match self.open_run(peer, req.into()).await {
            Ok(run) => Response::TeamRun(run_to_wire(&run)),
            Err(e) => Response::Error(e),
        }
    }

    /// Open a run whose roles each choose their own agent and model.
    pub(super) async fn team_run_create_v2(
        &self,
        peer: &Peer,
        req: TeamRunCreateV2Req,
    ) -> Response {
        match self.open_run(peer, req.into()).await {
            Ok(run) => Response::TeamRunV2(run_to_wire_v2(&run)),
            Err(e) => Response::Error(e),
        }
    }

    /// Start one session per role, then record the whole thing.
    ///
    /// A role whose session fails to start is recorded as `Failed` rather than
    /// aborting the run. Half a team is a real outcome the caller can act on;
    /// unwinding the sessions that *did* start would throw away work to report
    /// a tidier error.
    ///
    /// Every failure before the first launch is a protocol-level refusal
    /// identical for both verbs; only the success half differs in shape, which
    /// is why the reason comes back rather than a whole `Response`.
    async fn open_run(&self, peer: &Peer, req: RunSpec) -> Result<TeamRun, RpcError> {
        if !self.auth.may_manage_teams(peer) {
            return Err(RpcError::Unauthorized);
        }
        let (Some(teams), Some(launcher)) = (self.teams.as_ref(), self.launcher.as_ref()) else {
            return Err(RpcError::Unsupported);
        };
        if req.roles.is_empty() {
            return Err(RpcError::BadRequest("a run needs at least one role".into()));
        }
        if req.roles.len() > oximux_agents::team::MAX_ROLES {
            return Err(RpcError::BadRequest(format!(
                "a run may have at most {} roles",
                oximux_agents::team::MAX_ROLES
            )));
        }
        // Duplicate role names would collide on the table's (run, name) key, so
        // one role would silently overwrite another's session.
        let mut seen = std::collections::HashSet::new();
        if !req.roles.iter().all(|r| seen.insert(r.name.as_str())) {
            return Err(RpcError::BadRequest("role names must be distinct".into()));
        }

        let mut roles = Vec::with_capacity(req.roles.len());
        for spec in &req.roles {
            // The role's own agent, else the run's, else the host's default —
            // recorded as resolved so the board answers "which agent worked
            // this" rather than "was an override typed".
            let agent_id = spec.agent_id.clone().or_else(|| req.agent_id.clone());
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
                            agent_id,
                            model: spec.model.clone(),
                        });
                        continue;
                    }
                }
            } else {
                req.cwd.clone()
            };
            match launcher.create(&cwd, agent_id.as_deref(), spec.model.as_deref()).await {
                Ok(session_id) => {
                    let (status, summary) = self.start_role(&session_id, spec);
                    roles.push(NewTeamRole {
                        name: spec.name.clone(),
                        session_id: Some(session_id),
                        status,
                        summary,
                        agent_id,
                        model: spec.model.clone(),
                    });
                }
                Err(err) => roles.push(NewTeamRole {
                    name: spec.name.clone(),
                    session_id: None,
                    status: TeamRoleStatus::Failed,
                    summary: Some(launch_detail(&err).to_string()),
                    agent_id,
                    model: spec.model.clone(),
                }),
            }
        }

        teams.create(&req.name, &req.cwd, &roles, self.now_local()).map_err(|e| {
            // The store error can name the database path; log it, return
            // the category.
            tracing::warn!(error = %e, "recording a team run failed");
            RpcError::Internal("could not record the run".into())
        })
    }

    /// Give a freshly launched session its opening instruction.
    ///
    /// The role's model is **not** applied here — it was fixed at spawn, by the
    /// launcher. Switching afterwards was the obvious shape and it is the wrong
    /// one: Claude and Codex take `--model` on the command line and refuse to
    /// change it at runtime, so a post-launch switch reaches the trait-default
    /// `set_model` and bails on a headless host, which is where a team run is
    /// most likely to be driven. See [`SessionLauncher::create`].
    ///
    /// A session that opened but cannot take its prompt still names itself, so
    /// the board points at something inspectable.
    fn start_role(&self, session_id: &str, spec: &RoleSpec) -> (TeamRoleStatus, Option<String>) {
        let delivered = self
            .registry
            .get(session_id)
            .is_some_and(|handle| handle.send_prompt(&spec.prompt, &[]).is_ok());
        if delivered {
            (TeamRoleStatus::Running, None)
        } else {
            (
                TeamRoleStatus::Failed,
                Some("the session opened but its prompt could not be delivered".into()),
            )
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
        match self.readable_run(peer, run_id) {
            Ok(run) => Response::TeamRun(run_to_wire(&run)),
            Err(e) => Response::Error(e),
        }
    }

    /// One run's board, including the agent each role was worked by.
    ///
    /// Same gate, same run, one shape wider. A run opened through the v18 verb
    /// answers here with its roles naming no agent — which is what they
    /// recorded, not a gap in this reply.
    pub(super) fn team_status_v2(&self, peer: &Peer, run_id: &str) -> Response {
        match self.readable_run(peer, run_id) {
            Ok(run) => Response::TeamRunV2(run_to_wire_v2(&run)),
            Err(e) => Response::Error(e),
        }
    }

    /// One run, once this caller has been shown to be allowed to read it.
    fn readable_run(&self, peer: &Peer, run_id: &str) -> Result<TeamRun, RpcError> {
        let Some(teams) = self.teams.as_ref() else {
            return Err(RpcError::Unsupported);
        };
        let run = match teams.get(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return Err(RpcError::BadRequest("no such run".into())),
            Err(e) => {
                tracing::warn!(error = %e, "reading a team run failed");
                return Err(RpcError::Internal("could not read the run".into()));
            }
        };
        let sessions: Vec<String> = run.roles.iter().filter_map(|r| r.session_id.clone()).collect();
        if !self.auth.may_read_team_run(peer, &sessions) {
            return Err(RpcError::Unauthorized);
        }
        Ok(run)
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
            "this agent cannot be given a model when it starts; drop --role-model \
             for this role, or give it an agent that takes one"
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
        status: status_to_wire(role.status),
        summary: role.summary.clone(),
        updated_at: role.updated_at.to_rfc3339(),
    }
}

fn run_to_wire_v2(run: &TeamRun) -> TeamRunV2Wire {
    TeamRunV2Wire {
        id: run.id.clone(),
        name: run.name.clone(),
        cwd: run.cwd.clone(),
        created_at: run.created_at.to_rfc3339(),
        closed: run.closed(),
        roles: run.roles.iter().map(role_to_wire_v2).collect(),
    }
}

fn role_to_wire_v2(role: &TeamRole) -> TeamRoleV2Wire {
    TeamRoleV2Wire {
        name: role.name.clone(),
        session_id: role.session_id.clone(),
        status: status_to_wire(role.status),
        summary: role.summary.clone(),
        updated_at: role.updated_at.to_rfc3339(),
        agent_id: role.agent_id.clone(),
        model: role.model.clone(),
    }
}

fn status_to_wire(status: TeamRoleStatus) -> TeamRoleStatusWire {
    match status {
        TeamRoleStatus::Running => TeamRoleStatusWire::Running,
        TeamRoleStatus::Done => TeamRoleStatusWire::Done,
        TeamRoleStatus::Failed => TeamRoleStatusWire::Failed,
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
