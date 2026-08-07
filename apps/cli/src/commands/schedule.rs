//! `oximux schedule` — scheduled agent runs over the v10 schedule RPCs plus
//! the v17 manual fire. The host owns every clock: cadence validation, the
//! next-fire arithmetic, and run recording all happen there, so this side
//! only shapes arguments and renders replies.

use std::path::PathBuf;

use oximux_agents::schedule::recurrence::MIN_INTERVAL_MINUTES;
use oximux_remote_proto::messages::{
    RecurrenceWire, RunOutcomeWire, ScheduleRunWire, ScheduleWire,
};
use oximux_remote_proto::proto::{Request, Response};
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
) -> Result<RecurrenceWire, Failure> {
    let usage = |msg: &str| Failure::new("usage", exit::USAGE, msg.to_string());
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
        (Some(minutes), None, None) => Ok(RecurrenceWire::EveryMinutes { minutes }),
        (None, Some(time), None) => {
            let (hour, minute) = parse_hhmm(&time)
                .ok_or_else(|| usage("--daily wants HH:MM, e.g. --daily 09:00"))?;
            Ok(RecurrenceWire::DailyAt { hour, minute })
        }
        (None, None, Some(spec)) => {
            let (day, time) = spec
                .split_once(char::is_whitespace)
                .ok_or_else(|| usage("--weekly wants \"DAY HH:MM\", e.g. --weekly \"mon 09:00\""))?;
            let weekday = parse_weekday(day)
                .ok_or_else(|| usage("--weekly's day is mon/tue/wed/thu/fri/sat/sun"))?;
            let (hour, minute) = parse_hhmm(time.trim())
                .ok_or_else(|| usage("--weekly wants \"DAY HH:MM\", e.g. --weekly \"mon 09:00\""))?;
            Ok(RecurrenceWire::WeeklyAt { weekday, hour, minute })
        }
        _ => Err(usage("pick exactly one cadence: --every N, --daily HH:MM, or --weekly \"DAY HH:MM\"")),
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
}

pub async fn create(client: &Client, args: CreateArgs) -> Result<(Value, String), Failure> {
    let recurrence = parse_recurrence(args.every, args.daily, args.weekly)?;
    let cwd = match args.cwd {
        Some(dir) => dir,
        None => std::env::current_dir().map_err(|e| {
            Failure::new("cwd", exit::ERROR, format!("cannot read the current directory: {e}"))
        })?,
    };
    let reply = client
        .call(Request::CreateSchedule {
            name: args.name,
            cwd: cwd.to_string_lossy().into_owned(),
            prompt: args.prompt,
            agent_id: args.agent,
            recurrence,
        })
        .await?;
    match reply {
        Response::ScheduleCreated(s) => {
            let human =
                format!("created {}  {}  next fire {}", s.id, s.summary, s.next_fire_at);
            Ok((schedule_json(&s), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("CreateSchedule", &other)),
    }
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListSchedules).await? {
        Response::Schedules(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListSchedules", &other)),
    };
    let human = if rows.is_empty() {
        "no schedules".to_string()
    } else {
        rows.iter()
            .map(|s| {
                let state = if s.enabled { "on " } else { "off" };
                format!("{}  {}  {}  {}  next {}", s.id, state, s.name, s.summary, s.next_fire_at)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(rows.iter().map(schedule_json).collect::<Vec<_>>()), human))
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
            parse_recurrence(Some(30), None, None).unwrap(),
            RecurrenceWire::EveryMinutes { minutes: 30 }
        );
        assert_eq!(
            parse_recurrence(None, Some("09:30".into()), None).unwrap(),
            RecurrenceWire::DailyAt { hour: 9, minute: 30 }
        );
        assert_eq!(
            parse_recurrence(None, None, Some("fri 17:15".into())).unwrap(),
            RecurrenceWire::WeeklyAt { weekday: 4, hour: 17, minute: 15 }
        );
    }

    #[test]
    fn zero_or_malformed_cadences_are_usage_errors() {
        assert_eq!(parse_recurrence(None, None, None).unwrap_err().exit, exit::USAGE);
        assert_eq!(
            parse_recurrence(None, Some("25:00".into()), None).unwrap_err().exit,
            exit::USAGE
        );
        assert_eq!(
            parse_recurrence(None, None, Some("someday 09:00".into())).unwrap_err().exit,
            exit::USAGE
        );
        assert_eq!(parse_recurrence(None, None, Some("mon".into())).unwrap_err().exit, exit::USAGE);
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
            let err = parse_recurrence(Some(minutes), None, None).unwrap_err();
            assert_eq!(err.exit, exit::USAGE, "--every {minutes}");
        }
        // The floor is a floor, not an off-by-one.
        assert_eq!(
            parse_recurrence(Some(MIN_INTERVAL_MINUTES), None, None).unwrap(),
            RecurrenceWire::EveryMinutes { minutes: MIN_INTERVAL_MINUTES }
        );
    }
}
