//! `oximux schedule` — scheduled agent runs over the v10 schedule RPCs plus
//! the v17 manual fire. The host owns every clock: cadence validation, the
//! next-fire arithmetic, and run recording all happen there, so this side
//! only shapes arguments and renders replies.

use std::path::PathBuf;

use oximux_agents::schedule::recurrence::MIN_INTERVAL_MINUTES;
use oximux_remote_proto::messages::{
    RecurrenceV2Wire, RecurrenceWire, RunOutcomeWire, ScheduleRunWire, ScheduleV2Wire,
    ScheduleWire,
};
use oximux_remote_proto::proto::{Request, Response, SCHEDULE_CRON_MIN_VERSION};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// Exactly one cadence flag, parsed into the wire shape. Times are the
/// host's local clock — the natural reading for "every morning at 9".
fn parse_recurrence(
    every: Option<u32>,
    daily: Option<String>,
    weekly: Option<String>,
    cron: Option<String>,
) -> Result<RecurrenceV2Wire, Failure> {
    let usage = |msg: &str| Failure::new("usage", exit::USAGE, msg.to_string());
    if let Some(expr) = cron {
        // Deliberately *not* validated here, unlike every other cadence. The
        // host owns cron: it parses the expression, proves it fires, and holds
        // it to the same floor. Duplicating croner client-side would put two
        // parsers in the tree that must agree forever, and the one that matters
        // is the one guarding the store. An empty flag is still a usage error —
        // that is a mistyped argument, not a rejected rule.
        let expr = expr.trim().to_string();
        if expr.is_empty() {
            return Err(usage("--cron wants an expression, e.g. --cron \"0 9 * * 1-5\""));
        }
        return Ok(RecurrenceV2Wire::Cron { expr });
    }
    match (every, daily, weekly) {
        // The floor is checked here as well as on the host. The host's check is
        // the authority and stays — a client cannot be trusted to enforce an
        // invariant. This one decides the *class*: every other bad cadence
        // value (`--daily 25:00`, `--weekly "funday 09:00"`) is a usage error
        // caught without a host, and `--every 3` is wrong in exactly the same
        // way. Leaving it to the round trip made it exit 1 ("the host refused",
        // which a script may retry) instead of exit 2 ("fix the arguments").
        (Some(minutes), None, None) if minutes < MIN_INTERVAL_MINUTES => Err(usage(&format!(
            "--every wants at least {MIN_INTERVAL_MINUTES} minutes; each fire spawns an agent"
        ))),
        (Some(minutes), None, None) => Ok(RecurrenceV2Wire::EveryMinutes { minutes }),
        (None, Some(time), None) => {
            let (hour, minute) = parse_hhmm(&time)
                .ok_or_else(|| usage("--daily wants HH:MM, e.g. --daily 09:00"))?;
            Ok(RecurrenceV2Wire::DailyAt { hour, minute })
        }
        (None, None, Some(spec)) => {
            let (day, time) = spec
                .split_once(char::is_whitespace)
                .ok_or_else(|| usage("--weekly wants \"DAY HH:MM\", e.g. --weekly \"mon 09:00\""))?;
            let weekday = parse_weekday(day)
                .ok_or_else(|| usage("--weekly's day is mon/tue/wed/thu/fri/sat/sun"))?;
            let (hour, minute) = parse_hhmm(time.trim())
                .ok_or_else(|| usage("--weekly wants \"DAY HH:MM\", e.g. --weekly \"mon 09:00\""))?;
            Ok(RecurrenceV2Wire::WeeklyAt { weekday, hour, minute })
        }
        _ => Err(usage(
            "pick exactly one cadence: --every N, --daily HH:MM, --weekly \"DAY HH:MM\", \
             or --cron \"0 9 * * 1-5\"",
        )),
    }
}

fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.split_once(':')?;
    let hour: u8 = h.parse().ok()?;
    let minute: u8 = m.parse().ok()?;
    (hour < 24 && minute < 60).then_some((hour, minute))
}

/// 0 = Monday, matching the host's stored convention.
fn parse_weekday(s: &str) -> Option<u8> {
    match s.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(0),
        "tue" | "tuesday" => Some(1),
        "wed" | "wednesday" => Some(2),
        "thu" | "thursday" => Some(3),
        "fri" | "friday" => Some(4),
        "sat" | "saturday" => Some(5),
        "sun" | "sunday" => Some(6),
        _ => None,
    }
}

/// The v10 reply as JSON.
///
/// `cron` is always present and always null here: this shape cannot carry an
/// expression, and a key that appears only sometimes would make a script's
/// `.cron` read differently depending on which host answered. Null means "this
/// host could not tell us", which on a pre-v23 host is exactly true.
fn schedule_json(s: &ScheduleWire) -> Value {
    json!({
        "id": s.id,
        "name": s.name,
        "cwd": s.cwd,
        "prompt": s.prompt,
        "agent_id": s.agent_id,
        "enabled": s.enabled,
        "next_fire_at": s.next_fire_at,
        "summary": s.summary,
        "cron": Value::Null,
    })
}

/// The v23 reply as JSON — the same keys as [`schedule_json`], with `cron`
/// carrying the expression for a cron schedule and null for a preset one.
fn schedule_json_v2(s: &ScheduleV2Wire) -> Value {
    let cron = match &s.recurrence {
        RecurrenceV2Wire::Cron { expr } => Value::String(expr.clone()),
        _ => Value::Null,
    };
    json!({
        "id": s.id,
        "name": s.name,
        "cwd": s.cwd,
        "prompt": s.prompt,
        "agent_id": s.agent_id,
        "enabled": s.enabled,
        "next_fire_at": s.next_fire_at,
        "summary": s.summary,
        "cron": cron,
    })
}

fn run_json(r: &ScheduleRunWire) -> Value {
    json!({
        "schedule_id": r.schedule_id,
        "fired_at": r.fired_at,
        "outcome": match r.outcome { RunOutcomeWire::Ok => "ok", RunOutcomeWire::Failed => "failed" },
        "session_id": r.session_id,
        "detail": r.detail,
    })
}

fn run_line(r: &ScheduleRunWire) -> String {
    let outcome = match r.outcome {
        RunOutcomeWire::Ok => "ok",
        RunOutcomeWire::Failed => "failed",
    };
    let mut line = format!("{}  {}", r.fired_at, outcome);
    if let Some(session) = &r.session_id {
        line.push_str(&format!("  session {session}"));
    }
    if let Some(detail) = &r.detail {
        line.push_str(&format!("  — {detail}"));
    }
    line
}

pub struct CreateArgs {
    pub prompt: String,
    pub name: String,
    pub cwd: Option<PathBuf>,
    pub agent: Option<String>,
    pub every: Option<u32>,
    pub daily: Option<String>,
    pub weekly: Option<String>,
    pub cron: Option<String>,
}

/// Whether this host understands a cron recurrence.
fn serves_cron(host_version: u32) -> bool {
    host_version >= SCHEDULE_CRON_MIN_VERSION
}

/// Which create verb to send.
///
/// **V2 only when the recurrence needs it.** A preset cadence keeps taking the
/// v10 verb even against a v23 host, so every invocation that worked before
/// this phase encodes byte-for-byte as it did — the new path carries only what
/// could not travel the old one. `--cron` is separately refused before this by
/// `required_version`, so a `Cron` recurrence reaching here already implies a
/// v23 host, which is why this takes no version at all: a `Cron` recurrence
/// *is* the decision.
fn create_request(
    name: String,
    cwd: String,
    prompt: String,
    agent_id: Option<String>,
    recurrence: RecurrenceV2Wire,
) -> Request {
    let downgraded = match recurrence {
        RecurrenceV2Wire::EveryMinutes { minutes } => RecurrenceWire::EveryMinutes { minutes },
        RecurrenceV2Wire::DailyAt { hour, minute } => RecurrenceWire::DailyAt { hour, minute },
        RecurrenceV2Wire::WeeklyAt { weekday, hour, minute } => {
            RecurrenceWire::WeeklyAt { weekday, hour, minute }
        }
        cron @ RecurrenceV2Wire::Cron { .. } => {
            return Request::CreateScheduleV2 { name, cwd, prompt, agent_id, recurrence: cron };
        }
    };
    Request::CreateSchedule { name, cwd, prompt, agent_id, recurrence: downgraded }
}

/// Which list verb to ask this host for.
///
/// Unlike [`create_request`] this upgrades whenever it can, because the *reply*
/// is what differs: the v10 shape substitutes a stand-in recurrence for a cron
/// schedule, so a v23 host asked the old verb would hand back a listing that
/// reads right and cannot be trusted.
fn list_request(host_version: u32) -> Request {
    if serves_cron(host_version) { Request::ListSchedulesV2 } else { Request::ListSchedules }
}

pub async fn create(client: &Client, args: CreateArgs) -> Result<(Value, String), Failure> {
    let recurrence = parse_recurrence(args.every, args.daily, args.weekly, args.cron)?;
    let cwd = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    let reply = client
        .call(create_request(
            args.name,
            cwd.to_string_lossy().into_owned(),
            args.prompt,
            args.agent,
            recurrence,
        ))
        .await?;
    match reply {
        Response::ScheduleCreated(s) => {
            let human = format!("created {}  {}  next fire {}", s.id, s.summary, s.next_fire_at);
            Ok((schedule_json(&s), human))
        }
        Response::ScheduleCreatedV2(s) => {
            let human = format!("created {}  {}  next fire {}", s.id, s.summary, s.next_fire_at);
            Ok((schedule_json_v2(&s), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("CreateSchedule", &other)),
    }
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let (rows, human) = match client.call(list_request(client.host_version)).await? {
        Response::Schedules(rows) => (
            rows.iter().map(schedule_json).collect::<Vec<_>>(),
            rows.iter()
                .map(|s| row_line(&s.id, s.enabled, &s.name, &s.summary, &s.next_fire_at))
                .collect::<Vec<_>>(),
        ),
        Response::SchedulesV2(rows) => (
            rows.iter().map(schedule_json_v2).collect::<Vec<_>>(),
            rows.iter()
                .map(|s| row_line(&s.id, s.enabled, &s.name, &s.summary, &s.next_fire_at))
                .collect::<Vec<_>>(),
        ),
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListSchedules", &other)),
    };
    let human =
        if human.is_empty() { "no schedules".to_string() } else { human.join("\n") };
    Ok((json!(rows), human))
}

/// One `schedule ls` line. Shared by both reply shapes so the listing a user
/// reads never depends on which verb answered — the recurrence is rendered from
/// `summary`, which is exact in both.
fn row_line(id: &str, enabled: bool, name: &str, summary: &str, next_fire_at: &str) -> String {
    let state = if enabled { "on " } else { "off" };
    format!("{id}  {state}  {name}  {summary}  next {next_fire_at}")
}

pub async fn logs(client: &Client, id: &str, limit: u32) -> Result<(Value, String), Failure> {
    // One extra row is the truncation probe: the reply carries no has-more bit
    // (its postcard shape is frozen), so ask for limit+1 and show limit — a
    // full probe row proves older runs exist, and without it "20 of 20" and
    // "20 of 200" are the same listing.
    let probe = limit.saturating_add(1);
    let reply = client
        .call(Request::GetScheduleRuns { schedule_id: id.into(), limit: probe })
        .await?;
    let mut rows = match reply {
        Response::ScheduleRuns(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("GetScheduleRuns", &other)),
    };
    let truncated = probe != limit && rows.len() as u32 > limit;
    rows.truncate(limit as usize);
    let mut human = if rows.is_empty() {
        "no runs yet".to_string()
    } else {
        rows.iter().map(run_line).collect::<Vec<_>>().join("\n")
    };
    if truncated {
        human.push_str(&format!("\n… older runs exist beyond these {limit} — raise --limit"));
    }
    Ok((
        json!({
            "runs": rows.iter().map(run_json).collect::<Vec<_>>(),
            "truncated": truncated,
        }),
        human,
    ))
}

pub async fn set_enabled(
    client: &Client,
    id: &str,
    enabled: bool,
) -> Result<(Value, String), Failure> {
    let reply = client
        .call(Request::SetScheduleEnabled { id: id.into(), enabled })
        .await?;
    match reply {
        Response::Ack => {
            let verb = if enabled { "resumed" } else { "paused" };
            Ok((json!({ "id": id, "enabled": enabled }), format!("{verb} {id}")))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("SetScheduleEnabled", &other)),
    }
}

/// The manual fire. The reply is the recorded run — a fire that ran and
/// failed is a normal reply whose outcome says so, and it sets the exit code.
pub async fn run_once(client: &Client, id: &str) -> Result<(Value, String), Failure> {
    let reply = client.call(Request::RunScheduleNow { schedule_id: id.into() }).await?;
    match reply {
        Response::ScheduleRunRecorded(run) => {
            if run.outcome == RunOutcomeWire::Failed {
                let detail =
                    run.detail.clone().unwrap_or_else(|| "the run failed".to_string());
                return Err(Failure::new("run-once", exit::ERROR, detail).with_steps([format!(
                    "see the recorded run with `oximux schedule logs {id}`"
                )]));
            }
            let human = match &run.session_id {
                Some(session) => format!(
                    "fired {id} — running in session {session}\nfollow it with `oximux attach {session}`"
                ),
                None => format!("fired {id}"),
            };
            Ok((run_json(&run), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("RunScheduleNow", &other)),
    }
}

/// Idempotent, and the `Ack` says nothing about whether a row was there — the
/// store's `DELETE` reports success for zero rows. Stated as the postcondition
/// for the same reason [`super::worktree::rm`] is. Note the deliberate contrast
/// with `pause`/`resume`, which *do* refuse an unknown id: they change a
/// schedule's behaviour, so there has to be one to change.
pub async fn rm(client: &Client, id: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::DeleteSchedule { id: id.into() }).await? {
        Response::Ack => Ok((
            json!({ "id": id, "state": "absent" }),
            format!("no schedule `{id}` remains"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("DeleteSchedule", &other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_cadence_flag_parses_to_its_wire_shape() {
        assert_eq!(
            parse_recurrence(Some(30), None, None, None).unwrap(),
            RecurrenceV2Wire::EveryMinutes { minutes: 30 }
        );
        assert_eq!(
            parse_recurrence(None, Some("09:30".into()), None, None).unwrap(),
            RecurrenceV2Wire::DailyAt { hour: 9, minute: 30 }
        );
        assert_eq!(
            parse_recurrence(None, None, Some("fri 17:15".into()), None).unwrap(),
            RecurrenceV2Wire::WeeklyAt { weekday: 4, hour: 17, minute: 15 }
        );
    }

    #[test]
    fn zero_or_malformed_cadences_are_usage_errors() {
        assert_eq!(parse_recurrence(None, None, None, None).unwrap_err().exit, exit::USAGE);
        assert_eq!(
            parse_recurrence(None, Some("25:00".into()), None, None).unwrap_err().exit,
            exit::USAGE
        );
        assert_eq!(
            parse_recurrence(None, None, Some("someday 09:00".into()), None).unwrap_err().exit,
            exit::USAGE
        );
        assert_eq!(parse_recurrence(None, None, Some("mon".into()), None).unwrap_err().exit, exit::USAGE);
    }

    /// `--every` under the floor joins them, rather than being the one cadence
    /// value whose rejection needs a host.
    ///
    /// The test above already fixes the class for every *other* malformed
    /// cadence. `--every 3` was the exception: it parsed here and was refused
    /// on the wire, so it surfaced as exit 1 (`bad-request`) — the code a
    /// script reads as "the host refused, this may be worth retrying" — for an
    /// argument that can never be right. The host's own check is the authority
    /// and is unchanged; this only decides which exit code the caller sees.
    #[test]
    fn an_interval_under_the_floor_is_a_usage_error_like_every_other_bad_cadence() {
        for minutes in [0, 1, MIN_INTERVAL_MINUTES - 1] {
            let err = parse_recurrence(Some(minutes), None, None, None).unwrap_err();
            assert_eq!(err.exit, exit::USAGE, "--every {minutes}");
        }
        // The floor is a floor, not an off-by-one.
        assert_eq!(
            parse_recurrence(Some(MIN_INTERVAL_MINUTES), None, None, None).unwrap(),
            RecurrenceV2Wire::EveryMinutes { minutes: MIN_INTERVAL_MINUTES }
        );
    }

    /// `--cron` is passed through untouched, including case and inner spacing:
    /// the host is the parser, and normalising here would mean two grammars.
    #[test]
    fn a_cron_flag_reaches_the_wire_as_typed() {
        assert_eq!(
            parse_recurrence(None, None, None, Some("  0 9 * * 1-5  ".into())).unwrap(),
            RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() },
            "surrounding whitespace is trimmed; the expression itself is not touched"
        );
    }

    /// An empty `--cron` is a mistyped argument, not a rejected rule, so it is
    /// caught here (exit 2) rather than by the host (exit 1, which a script may
    /// retry). Every other malformed expression is deliberately the host's to
    /// refuse — see `parse_recurrence`.
    #[test]
    fn an_empty_cron_flag_is_a_usage_error() {
        for empty in ["", "   "] {
            let err = parse_recurrence(None, None, None, Some(empty.into())).unwrap_err();
            assert_eq!(err.exit, exit::USAGE, "--cron {empty:?}");
        }
    }

    /// Cron and a preset are mutually exclusive at the clap layer; this pins
    /// that `parse_recurrence` prefers cron rather than silently picking one,
    /// so a caller bypassing clap cannot land somewhere ambiguous.
    #[test]
    fn cron_wins_over_a_preset_rather_than_being_ignored() {
        assert_eq!(
            parse_recurrence(Some(30), None, None, Some("0 9 * * 1-5".into())).unwrap(),
            RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() }
        );
    }

    fn ordinal(req: Request) -> u8 {
        req.to_bytes().expect("encode")[0]
    }

    /// The version fork, pinned by the byte that actually goes on the wire.
    ///
    /// Asserting the `Request` variant would pass even if the ordinals moved;
    /// asserting the first encoded byte is what a pre-v23 host actually reads.
    #[test]
    fn only_a_cron_recurrence_takes_the_v23_verb() {
        let preset = |r| {
            create_request("n".into(), "/tmp".into(), "p".into(), None, r)
        };
        assert_eq!(
            ordinal(preset(RecurrenceV2Wire::EveryMinutes { minutes: 30 })),
            33,
            "a preset cadence keeps the v10 CreateSchedule"
        );
        assert_eq!(
            ordinal(preset(RecurrenceV2Wire::DailyAt { hour: 9, minute: 0 })),
            33,
            "and so does every other preset"
        );
        assert_eq!(
            ordinal(preset(RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() })),
            66,
            "only cron needs CreateScheduleV2"
        );
    }

    /// `ls` upgrades whenever the host can answer, because the v10 *reply*
    /// substitutes a stand-in recurrence for a cron schedule.
    #[test]
    fn ls_asks_the_richest_verb_the_host_serves() {
        assert_eq!(ordinal(list_request(22)), 32, "the version before cron shipped");
        assert_eq!(ordinal(list_request(0)), 32, "an unknown host is spoken to as v10");
        assert_eq!(ordinal(list_request(23)), 67, "v23 is where ListSchedulesV2 lands");
        assert_eq!(ordinal(list_request(99)), 67, "and a newer host still serves it");
    }

    #[test]
    fn serves_cron_starts_at_v23() {
        assert!(!serves_cron(22), "the version team runs shipped in");
        assert!(serves_cron(SCHEDULE_CRON_MIN_VERSION));
    }

    /// A script reading `.cron` must not have to know which host answered.
    #[test]
    fn both_board_shapes_emit_the_same_keys() {
        let v1 = ScheduleWire {
            id: "sch-1".into(),
            name: "n".into(),
            cwd: "/tmp".into(),
            prompt: "p".into(),
            agent_id: None,
            recurrence: RecurrenceWire::DailyAt { hour: 9, minute: 0 },
            enabled: true,
            next_fire_at: "2026-09-07T09:00:00+07:00".into(),
            summary: "daily at 09:00".into(),
        };
        let v2 = ScheduleV2Wire {
            id: "sch-1".into(),
            name: "n".into(),
            cwd: "/tmp".into(),
            prompt: "p".into(),
            agent_id: None,
            recurrence: RecurrenceV2Wire::Cron { expr: "0 9 * * 1-5".into() },
            enabled: true,
            next_fire_at: "2026-09-07T09:00:00+07:00".into(),
            summary: "at 09:00, on Monday".into(),
        };
        let keys = |v: &Value| {
            let mut k: Vec<String> =
                v.as_object().expect("object").keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&schedule_json(&v1)), keys(&schedule_json_v2(&v2)));
        assert_eq!(schedule_json(&v1)["cron"], Value::Null, "the v10 shape cannot know");
        assert_eq!(schedule_json_v2(&v2)["cron"], "0 9 * * 1-5");
        assert_eq!(
            schedule_json_v2(&ScheduleV2Wire {
                recurrence: RecurrenceV2Wire::DailyAt { hour: 9, minute: 0 },
                ..v2
            })["cron"],
            Value::Null,
            "a preset on the v23 shape has no expression either"
        );
    }
}
