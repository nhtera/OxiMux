//! `oximux heartbeat` — a session's own recurring wake-ups.
//!
//! The verb an agent runs on itself: from inside a session it needs no
//! `--session`, because the host resolves the target from the credential the
//! agent is holding. An operator names one.
//!
//! **Cron, mapped onto the host's closed recurrence set.** The flag takes a
//! standard five-field expression because that is the notation people already
//! have in their fingers, but the host stores recurrence as three cases
//! (`every N minutes`, `daily at`, `weekly at`) and those cross the wire as a
//! closed enum. Widening that enum is not an option: `RecurrenceWire` rides
//! inside `Vec<ScheduleWire>` in a reply older clients already ask for, and
//! postcard would misparse every element after a new variant. So an expression
//! outside the mappable set is refused by name rather than silently rounded to
//! something near it — see [`parse_cron`].
//!
//! **Schedules moved past this in v23; heartbeats have not.** A full cron
//! expression now has a host-side home (`Recurrence::Cron`) and a wire to ride
//! on, but only for schedules: `Request::CreateScheduleV2` carries
//! `RecurrenceV2Wire`, while `Request::CreateHeartbeat` still carries the closed
//! `RecurrenceWire` and has no v2 twin. So `heartbeat --cron` and
//! `schedule --cron` accept different grammars for now — a known wart, not an
//! oversight. Widening this one means giving heartbeats the same V2 treatment
//! schedules got, degradation path included.

use oximux_agents::schedule::recurrence::MIN_INTERVAL_MINUTES;
use oximux_remote_proto::messages::{CreateHeartbeatReq, HeartbeatWire, RecurrenceWire};
use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The cron shapes that map onto the host's recurrence vocabulary, quoted in
/// every refusal so the error teaches the supported set instead of just
/// rejecting.
const SUPPORTED_CRON: &str = "supported: \"*/N * * * *\" (every N minutes, N ≥ 5), \
     \"M H * * *\" (daily at H:M), \"M H * * DOW\" (weekly)";

/// Where a full cron expression *is* accepted, named in every refusal so the
/// error points somewhere rather than only saying no. A heartbeat wakes a
/// session that already exists and a schedule opens a fresh one, so this is a
/// redirection with a real trade-off — but a user who wrote `0 9 * * 1-5`
/// wants that cadence, and it exists.
const CRON_LIVES_AT: &str =
    "a full five-field expression (ranges, lists, day-of-month) works on \
     `oximux schedule create --cron`, which opens a new session per fire";

/// The `N ≥ 5` above is written out in prose, which a `const &str` cannot
/// interpolate — so pin it here instead of letting the help text drift away
/// from the rule it claims to teach.
const _: () = assert!(
    MIN_INTERVAL_MINUTES == 5,
    "the interval floor moved; update the `N ≥ 5` in SUPPORTED_CRON to match",
);

/// Parse a five-field cron expression into the host's recurrence vocabulary.
///
/// Deliberately narrow. Ranges, lists, step values outside the minute field,
/// and day-of-month/month constraints cannot be expressed by the closed
/// `RecurrenceWire` this verb sends, and accepting them would mean firing on a
/// cadence the user did not write.
///
/// The narrowness is now a **wire** limit, not a host one. Until v23 the host
/// genuinely could not store such a rule; `Recurrence::Cron` can, and
/// `schedule --cron` reaches it. `CreateHeartbeat` has no v2 twin to carry it,
/// so this stays narrow until one exists — and the refusal points at
/// `schedule --cron` rather than pretending the shape is unrepresentable.
pub fn parse_cron(expr: &str) -> Result<RecurrenceWire, Failure> {
    let refuse = |msg: String| Failure::new("usage", exit::USAGE, msg).with_steps([SUPPORTED_CRON.into(), CRON_LIVES_AT.into()]);
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(refuse(format!(
            "a cron expression has five fields (minute hour day month weekday); got {}",
            fields.len()
        )));
    }
    let [minute, hour, dom, month, dow] = [fields[0], fields[1], fields[2], fields[3], fields[4]];
    if dom != "*" || month != "*" {
        return Err(refuse(
            "day-of-month and month must be `*` — this host schedules by interval, day, or weekday"
                .into(),
        ));
    }

    // `*/N * * * *` — every N minutes.
    if let Some(step) = minute.strip_prefix("*/") {
        if hour != "*" || dow != "*" {
            return Err(refuse(
                "a stepped minute field must stand alone: \"*/N * * * *\"".into(),
            ));
        }
        let minutes: u32 = step
            .parse()
            .map_err(|_| refuse(format!("`{step}` is not a number of minutes")))?;
        // The floor is checked here as well as on the host, because the two
        // answer different questions. The host's check is the authority and
        // stays: a client cannot be trusted to enforce an invariant. This one
        // decides the *class* of the mistake — `*/3` is wrong in the argv, the
        // same way `7 3 5 * *` is, and a caller that branches on exit 2 ("fix
        // the arguments") versus exit 1 ("the host refused") was getting the
        // wrong answer for the one cron shape whose rejection needed a round
        // trip. It also reaches the refusal that quotes the supported set.
        if minutes < MIN_INTERVAL_MINUTES {
            return Err(refuse(format!(
                "`*/{minutes}` fires too often; the floor is {MIN_INTERVAL_MINUTES} minutes"
            )));
        }
        return Ok(RecurrenceWire::EveryMinutes { minutes });
    }
    if minute == "*" {
        return Err(refuse(
            "every-minute firing is not offered; the floor is five minutes (\"*/5 * * * *\")"
                .into(),
        ));
    }

    let minute: u8 = minute
        .parse()
        .ok()
        .filter(|m| *m < 60)
        .ok_or_else(|| refuse(format!("`{}` is not a minute (0–59)", fields[0])))?;
    let hour: u8 = hour
        .parse()
        .ok()
        .filter(|h| *h < 24)
        .ok_or_else(|| refuse(format!("`{hour}` is not an hour (0–23)")))?;

    if dow == "*" {
        return Ok(RecurrenceWire::DailyAt { hour, minute });
    }
    let weekday = parse_cron_weekday(dow)
        .ok_or_else(|| refuse(format!("`{dow}` is not a weekday (0–7 or mon…sun)")))?;
    Ok(RecurrenceWire::WeeklyAt { weekday, hour, minute })
}

/// Cron's weekday (0 or 7 = Sunday) into the host's (0 = Monday).
///
/// The two conventions genuinely differ, and getting it wrong shifts every
/// weekly heartbeat by a day — so the conversion is explicit and tested rather
/// than an inline modulo.
fn parse_cron_weekday(field: &str) -> Option<u8> {
    let cron_dow: u8 = match field.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        digits => digits.parse().ok().filter(|d| *d <= 7)?,
    };
    // 7 is cron's second spelling of Sunday.
    let cron_dow = cron_dow % 7;
    // cron 0=Sun → host 6; cron 1=Mon → host 0.
    Some((cron_dow + 6) % 7)
}

fn heartbeat_json(h: &HeartbeatWire) -> Value {
    json!({
        "id": h.id,
        "session_id": h.session_id,
        "name": h.name,
        "prompt": h.prompt,
        "enabled": h.enabled,
        "next_fire_at": h.next_fire_at,
        "summary": h.summary,
    })
}

pub async fn create(
    client: &Client,
    session: Option<String>,
    name: String,
    cron: &str,
    prompt: String,
) -> Result<(Value, String), Failure> {
    let recurrence = parse_cron(cron)?;
    let reply = client
        .call(Request::CreateHeartbeat(CreateHeartbeatReq {
            session_id: session,
            name,
            prompt,
            recurrence,
        }))
        .await?;
    match reply {
        Response::HeartbeatCreated(h) => {
            let human = format!(
                "armed {}  {}  next fire {}  (session {})",
                h.id, h.summary, h.next_fire_at, h.session_id
            );
            Ok((heartbeat_json(&h), human))
        }
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("CreateHeartbeat", &other)),
    }
}

pub async fn ls(client: &Client, session: Option<String>) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListHeartbeats { session_id: session }).await? {
        Response::Heartbeats(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListHeartbeats", &other)),
    };
    let human = if rows.is_empty() {
        "no heartbeats".to_string()
    } else {
        rows.iter()
            .map(|h| {
                let state = if h.enabled { "on " } else { "off" };
                format!("{}  {}  {}  {}  next {}", h.id, state, h.name, h.summary, h.next_fire_at)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok((json!(rows.iter().map(heartbeat_json).collect::<Vec<_>>()), human))
}

pub async fn rm(client: &Client, id: &str) -> Result<(Value, String), Failure> {
    match client.call(Request::DeleteHeartbeat { id: id.into() }).await? {
        // `Ack` carries no did-anything-happen bit, and the host answers one
        // already gone with the same `Ack` on purpose
        // (`dispatcher/heartbeats.rs`). So state the postcondition rather than
        // claiming a disarm that may never have had anything to disarm.
        Response::Ack => Ok((
            json!({ "id": id, "state": "absent" }),
            format!("no heartbeat `{id}` remains"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("DeleteHeartbeat", &other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_mappable_shapes_parse() {
        assert_eq!(
            parse_cron("*/15 * * * *").unwrap(),
            RecurrenceWire::EveryMinutes { minutes: 15 }
        );
        assert_eq!(
            parse_cron("0 9 * * *").unwrap(),
            RecurrenceWire::DailyAt { hour: 9, minute: 0 }
        );
        assert_eq!(
            parse_cron("30 17 * * fri").unwrap(),
            RecurrenceWire::WeeklyAt { weekday: 4, hour: 17, minute: 30 }
        );
    }

    /// The convention conversion, spelled out: cron counts from Sunday, the
    /// host from Monday, and an off-by-one here moves every weekly wake-up a
    /// day.
    #[test]
    fn cron_weekdays_convert_to_the_hosts_monday_first_numbering() {
        assert_eq!(parse_cron_weekday("0"), Some(6), "cron Sunday is host 6");
        assert_eq!(parse_cron_weekday("7"), Some(6), "7 is Sunday too");
        assert_eq!(parse_cron_weekday("1"), Some(0), "cron Monday is host 0");
        assert_eq!(parse_cron_weekday("6"), Some(5), "cron Saturday is host 5");
        assert_eq!(parse_cron_weekday("mon"), Some(0));
        assert_eq!(parse_cron_weekday("sun"), Some(6));
        assert_eq!(parse_cron_weekday("8"), None);
        assert_eq!(parse_cron_weekday("someday"), None);
    }

    /// An expression this host cannot represent is refused by name — never
    /// rounded to a nearby cadence the user did not ask for.
    #[test]
    fn unmappable_expressions_are_refused_with_the_supported_set() {
        for expr in [
            "0 9 1 * *",       // day-of-month
            "0 9 * 3 *",       // month
            "* * * * *",       // every minute
            "0 9,17 * * *",    // list
            "0 9-17 * * *",    // range
            "*/15 9 * * *",    // stepped minute with a constrained hour
            "0 9 * *",         // four fields
            "*/3 * * * *",     // under the interval floor
            "*/0 * * * *",     // zero
        ] {
            let err = parse_cron(expr).expect_err(expr);
            assert_eq!(err.exit, exit::USAGE, "{expr}");
            assert!(
                err.next_steps.iter().any(|s| s.contains("supported:")),
                "{expr} must teach the supported set"
            );
        }
    }

    #[test]
    fn out_of_range_times_are_refused() {
        assert!(parse_cron("60 9 * * *").is_err());
        assert!(parse_cron("0 24 * * *").is_err());
    }

    /// A cadence under the floor is a usage error, not a host refusal.
    ///
    /// This is about the exit *class*, which is the part a script acts on.
    /// `*/3` used to parse cleanly here and be rejected by the host, so it came
    /// back as exit 1 (`bad-request`) with no next steps — while every other
    /// malformed value of the same flag was exit 2 with the supported set
    /// attached. A caller that treats exit 2 as "fix the arguments" and exit 1
    /// as "the host said no, maybe retry" got the wrong answer for the one
    /// shape whose rejection needed a round trip. The host still enforces the
    /// floor; this only classifies it.
    #[test]
    fn a_cadence_under_the_floor_is_refused_as_usage_not_as_a_host_error() {
        let err = parse_cron("*/3 * * * *").expect_err("under the floor");
        assert_eq!(err.exit, exit::USAGE);
        assert!(
            err.next_steps.iter().any(|s| s.contains("supported:")),
            "and it teaches the supported set"
        );
        // The floor itself still parses, so the check is a floor and not an
        // off-by-one that moved the whole cadence range.
        assert_eq!(
            parse_cron("*/5 * * * *").unwrap(),
            RecurrenceWire::EveryMinutes { minutes: MIN_INTERVAL_MINUTES }
        );
    }

    /// A shape this verb cannot take must say where it *can* be taken.
    ///
    /// Since v23 the host can store a full cron expression — just not through
    /// `CreateHeartbeat`, whose `RecurrenceWire` is closed. A bare "unsupported"
    /// would now be misleading: the cadence the user wrote is representable,
    /// only on the other verb. Pinned because the split is a known wart, and a
    /// wart with a signpost is very different from one without.
    #[test]
    fn a_shape_this_verb_cannot_take_points_at_the_one_that_can() {
        for full_cron in ["0 9 * * 1-5", "0 9 1 * *", "0 9,17 * * *"] {
            let err = parse_cron(full_cron).expect_err("outside the mappable set");
            assert_eq!(err.exit, exit::USAGE, "{full_cron}");
            assert!(
                err.next_steps.iter().any(|s| s.contains("schedule create --cron")),
                "{full_cron}: the refusal must name where a full expression works, got {:?}",
                err.next_steps
            );
            assert!(
                err.next_steps.iter().any(|s| s.contains("supported:")),
                "{full_cron}: and still teach this verb's own set"
            );
        }
    }
}
