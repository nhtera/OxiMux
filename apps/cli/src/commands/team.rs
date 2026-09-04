//! `oximux team` — open a multi-role run, report a role's outcome, read the
//! board.
//!
//! The run lives on the host, which is what makes `team status` meaningful
//! after the process that started the run is gone. Roles report from inside
//! their own sessions (`oximux team report …`), so the board converges without
//! anything polling the agents.

use std::collections::HashMap;

use oximux_remote_proto::messages::{
    TeamReportReq, TeamRoleSpecV2Wire, TeamRoleSpecWire, TeamRoleStatusWire, TeamRoleV2Wire,
    TeamRoleWire, TeamRunCreateReq, TeamRunCreateV2Req, TeamRunV2Wire, TeamRunWire,
};
use oximux_remote_proto::proto::{Request, Response, TEAM_PER_ROLE_MIN_VERSION};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// `NAME=VALUE`, the grammar every repeatable `--role*` flag shares. Split on
/// the FIRST `=` so a value may contain them.
fn parse_pair(
    spec: &str,
    flag: &str,
    value_label: &str,
    example: &str,
) -> Result<(String, String), Failure> {
    let (name, value) = spec.split_once('=').ok_or_else(|| {
        Failure::new(
            "usage",
            exit::USAGE,
            format!("{flag} wants NAME={value_label}; got `{spec}`"),
        )
        .with_steps([format!("e.g. {example}")])
    })?;
    if name.trim().is_empty() || value.trim().is_empty() {
        return Err(Failure::new(
            "usage",
            exit::USAGE,
            format!("{flag} wants a non-empty NAME and {value_label}"),
        ));
    }
    Ok((name.trim().to_string(), value.to_string()))
}

/// `name=prompt`, the repeatable `--role` form.
pub fn parse_role(spec: &str) -> Result<(String, String), Failure> {
    parse_pair(spec, "--role", "PROMPT", "--role backend=\"port the API to v2\"")
}

/// The `--role-agent` / `--role-model` pairs, keyed by role name.
///
/// The value is trimmed here, unlike `--role`'s: an adapter id or a model id is
/// a token, and `--role-agent "plan= claude"` would otherwise launch nothing
/// with an error naming an agent that looks correct in the message. A prompt is
/// prose and keeps whatever spacing it was given.
///
/// A name no `--role` declared is a usage error listing the roles that exist,
/// rather than a flag that quietly applies to nothing: a typo here otherwise
/// costs a whole run against the wrong agent, and the run has already started
/// by the time anyone could notice. Naming one role twice is refused for the
/// same reason — last-wins would silently drop the earlier pick.
fn parse_role_pairs(
    specs: &[String],
    flag: &str,
    value_label: &str,
    example: &str,
    roles: &[(String, String)],
) -> Result<HashMap<String, String>, Failure> {
    let mut out: HashMap<String, String> = HashMap::new();
    for spec in specs {
        let (name, value) = parse_pair(spec, flag, value_label, example)?;
        let value = value.trim().to_string();
        if !roles.iter().any(|(role, _)| *role == name) {
            let known =
                roles.iter().map(|(role, _)| role.as_str()).collect::<Vec<_>>().join(", ");
            return Err(Failure::new(
                "usage",
                exit::USAGE,
                format!("{flag} names `{name}`, which is not a role in this run"),
            )
            .with_steps([format!("roles declared by --role: {known}")]));
        }
        if out.insert(name.clone(), value).is_some() {
            return Err(Failure::new(
                "usage",
                exit::USAGE,
                format!("{flag} names `{name}` more than once"),
            ));
        }
    }
    Ok(out)
}

fn status_text(status: TeamRoleStatusWire) -> &'static str {
    match status {
        TeamRoleStatusWire::Running => "running",
        TeamRoleStatusWire::Done => "done",
        TeamRoleStatusWire::Failed => "failed",
    }
}

/// The v18 role, with the v22 keys present and null.
///
/// A JSON shape that depends on which host answered is a trap: a script reading
/// `.roles[].agent_id` would see `null` from one host and a *missing key* from
/// another, with no way to tell which happened or why. Emitting the keys always
/// makes "no agent recorded" one answer with one spelling — which is also the
/// truth for every role an older host holds.
fn role_json(r: &TeamRoleWire) -> Value {
    json!({
        "name": r.name,
        "session_id": r.session_id,
        "status": status_text(r.status),
        "summary": r.summary,
        "updated_at": r.updated_at,
        "agent_id": Value::Null,
        "model": Value::Null,
    })
}

fn run_json(run: &TeamRunWire) -> Value {
    json!({
        "id": run.id,
        "name": run.name,
        "cwd": run.cwd,
        "created_at": run.created_at,
        "closed": run.closed,
        "roles": run.roles.iter().map(role_json).collect::<Vec<_>>(),
    })
}

fn run_board(run: &TeamRunWire) -> String {
    let mut out = format!(
        "{}  {}  {}",
        run.id,
        run.name,
        if run.closed { "closed" } else { "open" }
    );
    for role in &run.roles {
        out.push_str(&format!("\n  {:<12} {}", role.name, status_text(role.status)));
        if let Some(session) = &role.session_id {
            out.push_str(&format!("  session {session}"));
        }
        if let Some(summary) = &role.summary {
            out.push_str(&format!("  — {summary}"));
        }
    }
    out
}

/// The v2 board's extra column, as a suffix on the role's line.
///
/// Only what the host recorded is shown: a role that named no agent prints
/// none, rather than inventing "default" for a name the host never reports.
fn launched_with(agent_id: Option<&str>, model: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(agent) = agent_id {
        out.push_str(&format!("  agent {agent}"));
    }
    if let Some(model) = model {
        out.push_str(&format!("  model {model}"));
    }
    out
}

fn role_json_v2(r: &TeamRoleV2Wire) -> Value {
    json!({
        "name": r.name,
        "session_id": r.session_id,
        "status": status_text(r.status),
        "summary": r.summary,
        "updated_at": r.updated_at,
        "agent_id": r.agent_id,
        "model": r.model,
    })
}

fn run_json_v2(run: &TeamRunV2Wire) -> Value {
    json!({
        "id": run.id,
        "name": run.name,
        "cwd": run.cwd,
        "created_at": run.created_at,
        "closed": run.closed,
        "roles": run.roles.iter().map(role_json_v2).collect::<Vec<_>>(),
    })
}

fn run_board_v2(run: &TeamRunV2Wire) -> String {
    let mut out = format!(
        "{}  {}  {}",
        run.id,
        run.name,
        if run.closed { "closed" } else { "open" }
    );
    for role in &run.roles {
        out.push_str(&format!("\n  {:<12} {}", role.name, status_text(role.status)));
        if let Some(session) = &role.session_id {
            out.push_str(&format!("  session {session}"));
        }
        out.push_str(&launched_with(role.agent_id.as_deref(), role.model.as_deref()));
        if let Some(summary) = &role.summary {
            out.push_str(&format!("  — {summary}"));
        }
    }
    out
}

pub struct RunArgs {
    pub name: String,
    pub roles: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub agent: Option<String>,
    pub role_agents: Vec<String>,
    pub role_models: Vec<String>,
    pub worktree_each: bool,
}

/// Whether this host serves the per-role verbs, or must be spoken to in the
/// v18 shape.
///
/// A named predicate rather than an inline comparison so the fallback is one
/// decision with one test, instead of a `<` repeated at each call site where a
/// later edit could update one and miss the other — which would send a v22
/// ordinal to a host that answers it by dropping the connection.
fn serves_per_role(host_version: u32) -> bool {
    host_version >= TEAM_PER_ROLE_MIN_VERSION
}

/// Turn the flags into the roles the host launches.
///
/// Split out so the whole grammar — the pairs, the unknown-role check, the
/// per-role fallback to `--agent` — is testable without a host.
pub fn role_specs(args: &RunArgs) -> Result<Vec<TeamRoleSpecV2Wire>, Failure> {
    let roles = args.roles.iter().map(|s| parse_role(s)).collect::<Result<Vec<_>, _>>()?;
    let agents = parse_role_pairs(
        &args.role_agents,
        "--role-agent",
        "AGENT_ID",
        "--role-agent plan=claude",
        &roles,
    )?;
    let models = parse_role_pairs(
        &args.role_models,
        "--role-model",
        "MODEL",
        "--role-model plan=opus",
        &roles,
    )?;
    Ok(roles
        .into_iter()
        .map(|(name, prompt)| TeamRoleSpecV2Wire {
            // The run-level `--agent` is left to the host rather than folded in
            // here: it already resolves the fallback chain (role, run, its own
            // default) and folding one step of it client-side would put the
            // rule in two places.
            agent_id: agents.get(&name).cloned(),
            model: models.get(&name).cloned(),
            name,
            prompt,
        })
        .collect())
}

/// Which create verb to send this host, and in which shape.
///
/// A named function rather than a branch inside `run` so the choice is
/// **testable without a host**: both e2e suites boot a host of the current
/// build, which makes version skew structurally invisible to them, and the only
/// other coverage is a released binary this suite skips when it is absent.
/// A seam that returns the `Request` can be pinned to an ordinal directly.
fn create_request(host_version: u32, args: CreateWire) -> Request {
    let CreateWire { name, cwd, agent_id, worktree_each, roles } = args;
    // A host that predates the per-role verbs gets the v18 request. Not a
    // downgrade: `required_version` has already refused any run that names a
    // per-role agent or model, so what remains is exactly a v18 run.
    if !serves_per_role(host_version) {
        return Request::TeamRunCreate(TeamRunCreateReq {
            name,
            cwd,
            agent_id,
            worktree_each,
            roles: roles
                .into_iter()
                .map(|r| TeamRoleSpecWire { name: r.name, prompt: r.prompt })
                .collect(),
        });
    }
    Request::TeamRunCreateV2(TeamRunCreateV2Req { name, cwd, agent_id, worktree_each, roles })
}

/// Which board verb to ask this host for. See [`create_request`].
fn status_request(host_version: u32, run_id: &str) -> Request {
    if serves_per_role(host_version) {
        Request::TeamStatusV2 { run_id: run_id.into() }
    } else {
        Request::TeamStatus { run_id: run_id.into() }
    }
}

/// The parts of a run that cross the wire, in whichever shape it takes.
struct CreateWire {
    name: String,
    cwd: String,
    agent_id: Option<String>,
    worktree_each: bool,
    roles: Vec<TeamRoleSpecV2Wire>,
}

pub async fn run(client: &Client, args: RunArgs) -> Result<(Value, String), Failure> {
    let roles = role_specs(&args)?;
    let cwd = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    let request = create_request(
        client.host_version,
        CreateWire {
            name: args.name,
            cwd: cwd.to_string_lossy().into_owned(),
            agent_id: args.agent,
            worktree_each: args.worktree_each,
            roles,
        },
    );
    // Roles that failed to start are part of a normal reply, not an error: the
    // run exists, and the board says which roles are on it.
    let (json, board, id) = match client.call(request).await? {
        Response::TeamRunV2(run) => (run_json_v2(&run), run_board_v2(&run), run.id),
        Response::TeamRun(run) => (run_json(&run), run_board(&run), run.id),
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("TeamRunCreate", &other)),
    };
    Ok((json, format!("{board}\nwatch it with `oximux team status --run {id}`")))
}

pub async fn report(
    client: &Client,
    run_id: &str,
    role: &str,
    ok: bool,
    summary: Option<String>,
) -> Result<(Value, String), Failure> {
    let reply = client
        .call(Request::TeamReport(TeamReportReq {
            run_id: run_id.into(),
            role: role.into(),
            ok,
            summary,
        }))
        .await?;
    // The board comes back in the v18 shape — `TeamReport` has one reply type
    // and it is the frozen one. The JSON still carries `agent_id`/`model` as
    // null, like every other v18 board, so the key set never depends on which
    // verb answered; it is only the *human* board that omits the column. A
    // caller that needs the agents asks `team status`.
    match reply {
        Response::TeamRun(run) => Ok((run_json(&run), run_board(&run))),
        Response::Ack => Ok((json!({ "run_id": run_id, "role": role }), "reported".into())),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TeamReport", &other)),
    }
}

pub async fn status(client: &Client, run_id: &str) -> Result<(Value, String), Failure> {
    // Reading a board is never refused for being old — an older host simply has
    // no agent to report, which is also true of every run it holds.
    match client.call(status_request(client.host_version, run_id)).await? {
        Response::TeamRunV2(run) => Ok((run_json_v2(&run), run_board_v2(&run))),
        Response::TeamRun(run) => Ok((run_json(&run), run_board(&run))),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TeamStatus", &other)),
    }
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let runs = match client.call(Request::TeamList).await? {
        Response::TeamRuns(runs) => runs,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("TeamList", &other)),
    };
    let human = if runs.is_empty() {
        "no team runs".to_string()
    } else {
        runs.iter()
            .map(|run| {
                let open = run.roles.iter().filter(|r| r.status == TeamRoleStatusWire::Running).count();
                format!(
                    "{}  {}  {}/{} still running  {}",
                    run.id,
                    run.name,
                    open,
                    run.roles.len(),
                    run.created_at
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(runs.iter().map(run_json).collect::<Vec<_>>()), human))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_role_splits_on_the_first_equals() {
        let (name, prompt) = parse_role("backend=port the API to v2 (x=y)").expect("parses");
        assert_eq!(name, "backend");
        assert_eq!(prompt, "port the API to v2 (x=y)", "later `=` stay in the prompt");
    }

    #[test]
    fn a_role_without_an_equals_is_a_usage_error() {
        assert_eq!(parse_role("backend").unwrap_err().exit, exit::USAGE);
    }

    #[test]
    fn an_empty_half_is_a_usage_error() {
        assert_eq!(parse_role("=prompt").unwrap_err().exit, exit::USAGE);
        assert_eq!(parse_role("backend=").unwrap_err().exit, exit::USAGE);
    }

    fn args(roles: &[&str], agents: &[&str], models: &[&str]) -> RunArgs {
        RunArgs {
            name: "sweep".into(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            agent: None,
            role_agents: agents.iter().map(|s| s.to_string()).collect(),
            role_models: models.iter().map(|s| s.to_string()).collect(),
            worktree_each: false,
        }
    }

    #[test]
    fn each_role_carries_the_agent_and_model_named_for_it() {
        let specs = role_specs(&args(
            &["plan=survey", "impl=build", "review=check"],
            &["plan=claude", "impl=codex"],
            &["plan=opus"],
        ))
        .expect("parses");

        assert_eq!(specs[0].agent_id.as_deref(), Some("claude"));
        assert_eq!(specs[0].model.as_deref(), Some("opus"));
        assert_eq!(specs[1].agent_id.as_deref(), Some("codex"));
        assert_eq!(specs[1].model, None, "a role given no model is not given its neighbour's");
        assert_eq!(specs[2].agent_id, None, "and one named by neither flag carries neither");
        assert_eq!(specs[2].prompt, "check", "prompts are unaffected");
    }

    /// The whole reason the check exists: `--role-agent` naming a role that
    /// does not exist would otherwise apply to nothing, and the run has
    /// already started by the time anyone notices it ran on the wrong agent.
    #[test]
    fn a_role_agent_for_an_unknown_role_is_a_usage_error() {
        let err = role_specs(&args(&["plan=survey", "impl=build"], &["typo=claude"], &[]))
            .expect_err("refused");
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("typo"), "it names the offender: {}", err.message);
        let steps = err.next_steps.join(" ");
        assert!(
            steps.contains("plan") && steps.contains("impl"),
            "and the roles that do exist: {steps}"
        );
    }

    #[test]
    fn a_role_model_for_an_unknown_role_is_a_usage_error() {
        let err = role_specs(&args(&["plan=survey"], &[], &["impl=opus"])).expect_err("refused");
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("--role-model"), "{}", err.message);
    }

    /// Last-wins would silently drop the earlier pick, which is exactly the
    /// class of failure a scripted run cannot see.
    #[test]
    fn naming_one_role_twice_is_a_usage_error() {
        let err =
            role_specs(&args(&["plan=survey"], &["plan=claude", "plan=codex"], &[])).expect_err(
                "refused",
            );
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("more than once"), "{}", err.message);
    }

    #[test]
    fn a_role_agent_without_an_equals_is_a_usage_error() {
        let err = role_specs(&args(&["plan=survey"], &["claude"], &[])).expect_err("refused");
        assert_eq!(err.exit, exit::USAGE);
        assert!(err.message.contains("NAME=AGENT_ID"), "{}", err.message);
    }

    /// The run-level `--agent` is deliberately NOT folded into each spec: the
    /// host owns the fallback chain, and duplicating one step of it here would
    /// put the rule in two places that could drift.
    #[test]
    fn the_run_level_agent_is_left_for_the_host_to_apply() {
        let mut args = args(&["plan=survey", "impl=build"], &["impl=codex"], &[]);
        args.agent = Some("claude".into());
        let specs = role_specs(&args).expect("parses");
        assert_eq!(specs[0].agent_id, None, "the host resolves this one to the run's agent");
        assert_eq!(specs[1].agent_id.as_deref(), Some("codex"));
    }

    /// The fallback's one decision. Sending a v22 ordinal to a v21 host gets
    /// `error: undecodable request frame` at exit 1 — measured against the
    /// released v20 binary — which blames the frame for a version problem.
    #[test]
    fn an_older_host_is_spoken_to_in_the_v18_shape() {
        assert!(!serves_per_role(18), "the version team runs shipped in");
        assert!(!serves_per_role(21), "and every version up to the one before this");
        assert!(serves_per_role(22), "v22 is where the per-role verbs land");
        assert!(serves_per_role(99), "and a newer host still serves them");
    }

    fn wire() -> CreateWire {
        CreateWire {
            name: "sweep".into(),
            cwd: "/w".into(),
            agent_id: None,
            worktree_each: false,
            roles: vec![TeamRoleSpecV2Wire {
                name: "impl".into(),
                prompt: "go".into(),
                agent_id: None,
                model: None,
            }],
        }
    }

    /// The predicate above is not enough on its own: deleting the fallback from
    /// a *call site* would still leave it passing. These pin the bytes that
    /// actually go out, per host version.
    ///
    /// Asserted on the encoded ordinal rather than the variant, because the
    /// ordinal is what an old host decodes — 53/55 are the v18 verbs, 64/65 the
    /// v22 ones.
    #[test]
    fn an_old_host_is_sent_the_v18_ordinals_and_a_new_one_the_v22() {
        let ordinal = |req: Request| req.to_bytes().expect("encode")[0];

        assert_eq!(ordinal(create_request(21, wire())), 53, "TeamRunCreate");
        assert_eq!(ordinal(create_request(22, wire())), 64, "TeamRunCreateV2");
        assert_eq!(ordinal(status_request(21, "run-1")), 55, "TeamStatus");
        assert_eq!(ordinal(status_request(22, "run-1")), 65, "TeamStatusV2");

        // A client that never completed `Hello` reports version 0. It must fall
        // back, not gamble — an unknown host is an old host until proven newer.
        assert_eq!(ordinal(create_request(0, wire())), 53, "an unknown host is spoken to as v18");
        assert_eq!(ordinal(status_request(0, "run-1")), 55);
    }

    /// The v18 request carries the roles it can, and drops only what that shape
    /// has no field for — a role must not be lost in the downgrade.
    #[test]
    fn the_v18_downgrade_keeps_every_role() {
        let Request::TeamRunCreate(req) = create_request(21, wire()) else {
            panic!("expected the v18 verb")
        };
        assert_eq!(req.roles.len(), 1);
        assert_eq!(req.roles[0].name, "impl");
        assert_eq!(req.roles[0].prompt, "go");
    }

    /// Both board shapes carry the same key set, so a script reading
    /// `.roles[].agent_id` gets `null` from an old host rather than a missing
    /// key it cannot distinguish from an error.
    #[test]
    fn both_board_shapes_emit_the_same_keys() {
        let v1 = role_json(&TeamRoleWire {
            name: "impl".into(),
            session_id: None,
            status: TeamRoleStatusWire::Running,
            summary: None,
            updated_at: "t".into(),
        });
        let v2 = role_json_v2(&TeamRoleV2Wire {
            name: "impl".into(),
            session_id: None,
            status: TeamRoleStatusWire::Running,
            summary: None,
            updated_at: "t".into(),
            agent_id: Some("codex".into()),
            model: None,
        });
        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().expect("object").keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v1), keys(&v2));
        assert!(v1["agent_id"].is_null(), "an old host reports no agent as null, not as absent");
        assert_eq!(v2["agent_id"], "codex");
    }

    #[test]
    fn a_board_line_shows_only_what_was_recorded() {
        assert_eq!(launched_with(Some("codex"), Some("opus")), "  agent codex  model opus");
        assert_eq!(launched_with(Some("codex"), None), "  agent codex");
        assert_eq!(launched_with(None, None), "", "no agent recorded prints nothing at all");
    }
}
