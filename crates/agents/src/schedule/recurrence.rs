//! When a schedule fires next.
//!
//! **Presets first, cron as the escape hatch.** The three preset variants came
//! first and stay first: schedules are created on a phone, where `0 9 * * 1-5`
//! is miserable to type and cryptic when mistyped, and a closed vocabulary maps
//! onto pickers that cannot produce an invalid value at all. Nothing about the
//! presets is expressed in cron fields, and none of them would be simplified by
//! it.
//!
//! [`Recurrence::Cron`] exists because a closed vocabulary cannot say
//! "weekdays at 09:00" — the recurrence people actually wanted, which until now
//! required a heartbeat workaround. It is deliberately the only open-ended
//! variant, and it is validated hard at construction (see
//! [`Recurrence::cron`]) precisely because it is the one a user can get wrong.
//!
//! **Local time, not UTC.** "Every day at 9am" means 9am where the person is,
//! which is the desktop's local zone. That makes DST a real concern rather than
//! a theoretical one — see [`Recurrence::next_after`].

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};
use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};

/// How often a schedule repeats.
///
/// Not `Copy`: [`Recurrence::Cron`] owns its expression. Every preset variant
/// still is in effect, but the type is matched by reference throughout rather
/// than duplicating the enum to keep a `Copy` fast path for three integers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recurrence {
    /// Every N minutes, measured from the previous fire.
    ///
    /// Unlike the wall-clock variants this has no anchor: it repeats relative to
    /// whenever it last ran, so a desktop that was closed for an hour resumes
    /// spacing from the moment it comes back rather than firing immediately for
    /// every interval it missed.
    EveryMinutes(u32),
    /// Every day at a wall-clock time.
    DailyAt { hour: u8, minute: u8 },
    /// Every week on one weekday at a wall-clock time. `weekday` is 0=Monday.
    WeeklyAt { weekday: u8, hour: u8, minute: u8 },
    /// A five-field cron expression (`minute hour day-of-month month
    /// day-of-week`), evaluated in the host's local zone like every other
    /// variant.
    ///
    /// Stored as the text the user typed, not as a parsed pattern: the string
    /// is what round-trips through the database and the wire, and re-parsing it
    /// per fire costs microseconds against a schedule that fires at most every
    /// five minutes. Keeping the text also means the expression a user reads
    /// back is character-for-character the one they wrote.
    ///
    /// **Cron's weekday numbering is its own** — 0 and 7 are both Sunday —
    /// and deliberately not reconciled with [`Recurrence::WeeklyAt`]'s
    /// 0=Monday. They are separate variants; a user writing cron is writing
    /// cron.
    Cron(String),
}

/// Smallest interval a schedule may use.
///
/// A one-minute agent run is almost certainly a mistake — each fire spawns a
/// process and starts a turn — and the floor is enforced at construction so a
/// runaway schedule cannot be created at all, rather than being caught later by
/// something that notices the machine is thrashing.
pub const MIN_INTERVAL_MINUTES: u32 = 5;

/// How many consecutive fires [`Recurrence::cron`] walks to find the pattern's
/// tightest gap.
///
/// The minimum gap of a five-field pattern is fixed by its **minute** field:
/// the hour, day and month fields only ever remove fires, and removing a fire
/// can only widen the gaps around it. So a walk long enough to see the minute
/// field repeat sees the true minimum, and 64 fires spans that many times over.
///
/// **Not paid once.** `row_to_schedule` rebuilds a stored row through
/// [`Recurrence::cron`], so this walk runs on every *read* of a cron schedule,
/// not only when one is created — and `ScheduleStore::due` reads on every tick.
/// That is deliberate (a row hand-edited below the floor must not load), and it
/// is affordable: measured at **188µs per cron row per read in a debug build**,
/// against a 30-second tick. A release build is materially faster still, this
/// being compute-bound work.
///
/// The pathological pattern is one that fires yearly — `0 9 29 2 *` walks 262
/// years of leap days — and even that measures under 5ms, once, in debug.
const FLOOR_PROBE_FIRES: usize = 64;

/// Why a recurrence could not be built.
///
/// Not `Copy`: [`RecurrenceError::BadCron`] carries the parser's own message.
/// A generic "that is not a valid cron expression" would be strictly less
/// useful than `Number out of bounds` or `Pattern must have 5 fields`, and the
/// whole point of validating at construction is to hand back something the user
/// can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecurrenceError {
    #[error("interval must be at least {MIN_INTERVAL_MINUTES} minutes")]
    IntervalTooShort,
    #[error("that is not a valid time of day")]
    BadTime,
    #[error("that is not a valid weekday")]
    BadWeekday,
    #[error("{0}")]
    BadCron(String),
    /// A pattern that parses but describes an instant the calendar never
    /// reaches — `0 9 30 2 *` (February 30th), `0 9 31 4 *` (April 31st).
    #[error("that expression is valid but never comes around — check the day and month")]
    CronNeverFires,
    #[error("that expression fires more often than every {MIN_INTERVAL_MINUTES} minutes")]
    CronTooFrequent,
}

impl Recurrence {
    /// Every `minutes` minutes, refusing anything under [`MIN_INTERVAL_MINUTES`].
    pub fn every_minutes(minutes: u32) -> Result<Self, RecurrenceError> {
        if minutes < MIN_INTERVAL_MINUTES {
            return Err(RecurrenceError::IntervalTooShort);
        }
        Ok(Self::EveryMinutes(minutes))
    }

    pub fn daily_at(hour: u8, minute: u8) -> Result<Self, RecurrenceError> {
        check_time(hour, minute)?;
        Ok(Self::DailyAt { hour, minute })
    }

    pub fn weekly_at(weekday: u8, hour: u8, minute: u8) -> Result<Self, RecurrenceError> {
        if weekday > 6 {
            return Err(RecurrenceError::BadWeekday);
        }
        check_time(hour, minute)?;
        Ok(Self::WeeklyAt { weekday, hour, minute })
    }

    /// A five-field cron expression, rejected here rather than at fire time.
    ///
    /// Three things are checked, and all three are the same kind of check: a
    /// schedule that cannot do what it says is worse than one that refuses to
    /// exist, because nothing later contradicts it.
    ///
    /// 1. **It parses as five fields.** Six- and seven-field patterns are
    ///    refused rather than reinterpreted — the extra field would silently
    ///    become seconds, changing what the user asked for.
    /// 2. **It fires at least once.** `0 9 30 2 *` parses cleanly and then
    ///    never comes around, which would create a schedule that looks armed
    ///    and never runs.
    /// 3. **It respects [`MIN_INTERVAL_MINUTES`].** Otherwise
    ///    `* * * * *` would walk straight past a floor whose whole claim is
    ///    that a runaway schedule cannot be created at all.
    ///
    /// The cadence checks are measured from *now*, which is what the caller is
    /// creating the schedule against. They read the pattern, not the clock:
    /// a pattern's tightest gap does not depend on when you start looking.
    pub fn cron(expr: &str) -> Result<Self, RecurrenceError> {
        let expr = expr.trim();
        let parsed = parse_cron(expr)?;
        check_cron_cadence(&parsed, Local::now())?;
        Ok(Self::Cron(expr.to_string()))
    }

    /// The first fire strictly after `after`.
    ///
    /// Strictly after, never equal: a schedule that has just fired at exactly
    /// its target time must not resolve to that same instant and fire again in a
    /// loop.
    ///
    /// **DST.** The wall-clock variants ask for a local time that may not exist
    /// (spring forward skips 02:30) or may exist twice (autumn back repeats it).
    /// A skipped time advances to the next day rather than silently firing at
    /// some nearby hour — a run the user placed inside the skipped window has no
    /// correct time that day, and inventing one is worse than waiting. An
    /// ambiguous time takes the **earlier** of the two, so the schedule fires
    /// once rather than twice.
    ///
    /// **Cron resolves DST itself, and agrees on only one of those two.**
    /// [`Recurrence::Cron`] delegates to croner, so the rules above do not
    /// apply to it. Measured against croner 4.0 under `TZ=Europe/Stockholm`,
    /// not read off its source:
    ///
    /// - *Ambiguous* (2026-10-25, 02:30 twice): croner returns the same instant
    ///   the presets do. No divergence, and no double fire.
    /// - *Skipped* (2026-03-29, 02:30 absent): `30 2 * * *` fires at **03:00**,
    ///   the first real instant after the gap — the opposite of the preset rule
    ///   directly above, which would wait a day.
    ///
    /// That divergence is **accepted, not overlooked**. Someone writing a cron
    /// expression is asking for cron's semantics, and cron's answer to a
    /// skipped hour is the one every other cron implementation gives; making
    /// OxiMux the exception would surprise the people most likely to use this
    /// variant. It costs one early fire, once a year, on one date.
    ///
    /// **Not pinned by a test, deliberately — do not add one.** Asserting this
    /// needs `TZ` set for the whole process, and that is unreliable twice over.
    /// `TZ` is ignored on the Windows runner CI also uses; and on Unix,
    /// chrono (0.4.45, `offset/local/unix.rs`) keeps the parsed zone in a
    /// `thread_local!` and reuses it *unconditionally* when it is under a
    /// second old, re-reading `TZ` only after that. So a test that sets `TZ`
    /// and builds a `Local` instant gets the **stale** zone whenever its worker
    /// thread touched `Local` in the previous second — a flake that depends on
    /// test scheduling order, with no deterministic fix short of sleeping a
    /// second per test.
    ///
    /// Reproduce by hand instead: `TZ=Europe/Stockholm`, then
    /// `find_next_occurrence` from 2026-03-28. The residual risk is that a
    /// croner minor bump changes this silently.
    pub fn next_after(&self, after: DateTime<Local>) -> DateTime<Local> {
        match *self {
            Self::EveryMinutes(minutes) => after + Duration::minutes(minutes as i64),
            // Re-parsed per fire rather than cached: the parse is microseconds
            // against a schedule that fires at most every five minutes, and a
            // cache would need somewhere to live that survives the round trip
            // through the database. A parse failure is unreachable for a value
            // built through `Recurrence::cron`, and the far-future answer keeps
            // such a row inert rather than firing constantly — the same choice
            // `next_wall_clock` makes for its own unreachable branch.
            Self::Cron(ref expr) => parse_cron(expr)
                .ok()
                .and_then(|c| c.find_next_occurrence(&after, false).ok())
                .unwrap_or_else(|| after + Duration::days(365)),
            Self::DailyAt { hour, minute } => {
                next_wall_clock(after, hour, minute, |d| d + Duration::days(1))
            }
            Self::WeeklyAt { weekday, hour, minute } => {
                // Walk to the target weekday first, then apply the same
                // wall-clock resolution — so a weekly run lands on its day even
                // when today already passed its time.
                let target = weekday as u32;
                let mut day = after.date_naive();
                let today = day.weekday().num_days_from_monday();
                let ahead = (target + 7 - today) % 7;
                day += Duration::days(ahead as i64);
                let candidate = resolve_local(day, hour, minute);
                match candidate {
                    Some(dt) if dt > after => dt,
                    // Either today's slot has passed or it does not exist this
                    // week; try each following week until one resolves.
                    _ => {
                        let mut next = day + Duration::days(7);
                        loop {
                            if let Some(dt) = resolve_local(next, hour, minute)
                                && dt > after
                            {
                                return dt;
                            }
                            next += Duration::days(7);
                        }
                    }
                }
            }
        }
    }
}

/// The one parser configuration this module uses.
///
/// `Seconds::Disallowed` and `Year::Disallowed` pin the grammar to exactly five
/// fields. Without them croner accepts six- and seven-field patterns too, and a
/// user who typed one extra field would get a schedule that means something
/// other than what they wrote — the leading field would become *seconds*. A
/// refusal naming the field count is the better answer.
fn cron_parser() -> CronParser {
    CronParser::builder().seconds(Seconds::Disallowed).year(Year::Disallowed).build()
}

fn parse_cron(expr: &str) -> Result<Cron, RecurrenceError> {
    cron_parser().parse(expr).map_err(|e| RecurrenceError::BadCron(e.to_string()))
}

/// Reject a pattern that never fires, or fires tighter than the floor.
///
/// Walks [`FLOOR_PROBE_FIRES`] consecutive occurrences from `from`. The first
/// lookup failing means the pattern never comes around at all; a later one
/// failing means the walk ran off the end of croner's search horizon, which is
/// not a cadence problem — a pattern sparse enough to exhaust the horizon is by
/// definition nowhere near the floor, so the gaps seen so far are the answer.
fn check_cron_cadence(pattern: &Cron, from: DateTime<Local>) -> Result<(), RecurrenceError> {
    let floor = Duration::minutes(MIN_INTERVAL_MINUTES as i64);
    let mut prev = match pattern.find_next_occurrence(&from, false) {
        Ok(first) => first,
        Err(_) => return Err(RecurrenceError::CronNeverFires),
    };
    for _ in 1..FLOOR_PROBE_FIRES {
        let Ok(next) = pattern.find_next_occurrence(&prev, false) else { break };
        if next - prev < floor {
            return Err(RecurrenceError::CronTooFrequent);
        }
        prev = next;
    }
    Ok(())
}

fn check_time(hour: u8, minute: u8) -> Result<(), RecurrenceError> {
    if hour > 23 || minute > 59 {
        return Err(RecurrenceError::BadTime);
    }
    Ok(())
}

/// The next occurrence of a daily wall-clock time strictly after `after`.
fn next_wall_clock(
    after: DateTime<Local>,
    hour: u8,
    minute: u8,
    advance: impl Fn(chrono::NaiveDate) -> chrono::NaiveDate,
) -> DateTime<Local> {
    let mut day = after.date_naive();
    // Bounded rather than `loop`: a run of days where the target time does not
    // resolve is not something DST can actually produce, but an unbounded loop
    // here would hang the scheduler tick rather than misbehave visibly.
    for _ in 0..8 {
        if let Some(dt) = resolve_local(day, hour, minute)
            && dt > after
        {
            return dt;
        }
        day = advance(day);
    }
    // Unreachable in practice; returning a far-future time keeps the schedule
    // inert instead of firing constantly if it ever were reached.
    after + Duration::days(1)
}

/// A local `DateTime` for `day` at `hour:minute`, resolving DST.
///
/// `None` when that wall-clock time does not exist locally (spring forward).
/// When it exists twice (autumn back) the earlier instant wins, so the schedule
/// fires once.
fn resolve_local(day: chrono::NaiveDate, hour: u8, minute: u8) -> Option<DateTime<Local>> {
    let time = NaiveTime::from_hms_opt(hour as u32, minute as u32, 0)?;
    match Local.from_local_datetime(&day.and_time(time)) {
        chrono::LocalResult::Single(dt) => Some(dt),
        chrono::LocalResult::Ambiguous(earlier, _later) => Some(earlier),
        chrono::LocalResult::None => None,
    }
}

/// Render a recurrence for a person, e.g. `every 30 minutes`, `daily at 09:00`.
///
/// Lives here rather than in the UI so the desktop and the phone describe a
/// schedule identically — two renderings of the same rule that drifted would
/// have a user reading different things about one schedule on two screens.
pub fn describe(r: &Recurrence) -> String {
    const DAYS: [&str; 7] =
        ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    match *r {
        Recurrence::EveryMinutes(m) if m % 60 == 0 && m >= 60 => {
            let hours = m / 60;
            if hours == 1 { "every hour".into() } else { format!("every {hours} hours") }
        }
        Recurrence::EveryMinutes(m) => format!("every {m} minutes"),
        Recurrence::DailyAt { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
        Recurrence::WeeklyAt { weekday, hour, minute } => {
            let day = DAYS.get(weekday as usize).copied().unwrap_or("?");
            format!("{day}s at {hour:02}:{minute:02}")
        }
        Recurrence::Cron(ref expr) => describe_cron(expr),
    }
}

/// Phrase a cron expression the way the preset variants phrase themselves.
///
/// Borrowed from croner rather than hand-written: it already renders every
/// field combination in English, and a second describer would drift from the
/// pattern it claims to describe. Its output is a capitalised sentence with a
/// full stop (`At 09:00, on Monday, ... and Friday.`); both are trimmed so the
/// result composes into the caller's `"{recurrence} · {next fire}"` line the
/// same way `daily at 09:00` does.
///
/// Falls back to the raw expression if the pattern will not parse. That is
/// unreachable for anything built through [`Recurrence::cron`], and showing the
/// user their own text beats showing them nothing.
fn describe_cron(expr: &str) -> String {
    let Ok(pattern) = parse_cron(expr) else { return expr.to_string() };
    let described = pattern.pattern.describe();
    let trimmed = described.trim_end_matches('.');
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
        None => expr.to_string(),
    }
}

/// Whether `hour`/`minute` name a real time of day — exposed so a caller
/// validating client input rejects it before building anything.
pub fn is_valid_time(hour: u8, minute: u8) -> bool {
    check_time(hour, minute).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    fn at(s: &str) -> DateTime<Local> {
        // Parsed as a local naive time, matching how a user reads a schedule.
        let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").expect("parse");
        Local.from_local_datetime(&naive).single().expect("unambiguous test instant")
    }

    #[test]
    fn an_interval_counts_from_the_previous_fire() {
        let r = Recurrence::every_minutes(30).unwrap();
        assert_eq!(r.next_after(at("2026-07-21 10:00:00")), at("2026-07-21 10:30:00"));
    }

    /// The floor exists so a runaway schedule cannot be created at all — each
    /// fire spawns a process and starts a turn.
    #[test]
    fn an_interval_under_the_floor_is_refused() {
        assert_eq!(Recurrence::every_minutes(1), Err(RecurrenceError::IntervalTooShort));
        assert!(Recurrence::every_minutes(MIN_INTERVAL_MINUTES).is_ok());
    }

    #[test]
    fn a_daily_schedule_takes_todays_slot_when_it_is_still_ahead() {
        let r = Recurrence::daily_at(9, 0).unwrap();
        assert_eq!(r.next_after(at("2026-07-21 08:00:00")), at("2026-07-21 09:00:00"));
    }

    #[test]
    fn a_daily_schedule_rolls_to_tomorrow_once_todays_slot_has_passed() {
        let r = Recurrence::daily_at(9, 0).unwrap();
        assert_eq!(r.next_after(at("2026-07-21 10:00:00")), at("2026-07-22 09:00:00"));
    }

    /// Strictly after, never equal. Resolving to the same instant a schedule
    /// just fired at would fire it again immediately, in a loop.
    #[test]
    fn a_daily_schedule_does_not_return_the_instant_it_just_fired() {
        let r = Recurrence::daily_at(9, 0).unwrap();
        assert_eq!(r.next_after(at("2026-07-21 09:00:00")), at("2026-07-22 09:00:00"));
    }

    #[test]
    fn a_weekly_schedule_finds_its_weekday() {
        // 2026-07-21 is a Tuesday; weekday 4 is Friday.
        let r = Recurrence::weekly_at(4, 9, 0).unwrap();
        let next = r.next_after(at("2026-07-21 10:00:00"));
        assert_eq!(next.weekday(), chrono::Weekday::Fri);
        assert_eq!(chrono::Timelike::hour(&next), 9);
        assert_eq!(next, at("2026-07-24 09:00:00"));
    }

    /// Same weekday, time already gone: it must go a week out, not fire today.
    #[test]
    fn a_weekly_schedule_skips_a_week_when_todays_slot_has_passed() {
        // 2026-07-21 is a Tuesday; weekday 1 is Tuesday.
        let r = Recurrence::weekly_at(1, 9, 0).unwrap();
        assert_eq!(r.next_after(at("2026-07-21 10:00:00")), at("2026-07-28 09:00:00"));
    }

    #[test]
    fn invalid_times_and_weekdays_are_refused() {
        assert_eq!(Recurrence::daily_at(24, 0), Err(RecurrenceError::BadTime));
        assert_eq!(Recurrence::daily_at(0, 60), Err(RecurrenceError::BadTime));
        assert_eq!(Recurrence::weekly_at(7, 9, 0), Err(RecurrenceError::BadWeekday));
        assert!(Recurrence::weekly_at(6, 23, 59).is_ok());
    }

    /// Whatever the local zone, a computed next-fire is always in the future.
    /// This is the invariant the tick loop depends on: a next-fire at or before
    /// `now` would fire again on the very next tick, forever.
    #[test]
    fn every_recurrence_always_advances() {
        let now = Local::now();
        let cases = [
            Recurrence::every_minutes(5).unwrap(),
            Recurrence::daily_at(0, 0).unwrap(),
            Recurrence::daily_at(23, 59).unwrap(),
            Recurrence::weekly_at(0, 12, 0).unwrap(),
            Recurrence::weekly_at(6, 3, 30).unwrap(),
        ];
        for case in cases {
            assert!(case.next_after(now) > now, "{case:?} did not advance past now");
        }
    }

    /// Chained fires keep advancing — a schedule cannot stall on one instant.
    #[test]
    fn repeated_next_after_keeps_moving_forward() {
        let r = Recurrence::daily_at(9, 0).unwrap();
        let mut t = at("2026-07-21 08:00:00");
        for _ in 0..400 {
            let next = r.next_after(t);
            assert!(next > t, "stalled at {t}");
            t = next;
        }
        // 400 daily fires from July 2026 crosses two DST transitions in most
        // zones, so this also exercises the resolution path rather than only
        // the happy one.
        assert!(t > at("2027-07-21 00:00:00"));
    }

    #[test]
    fn descriptions_read_naturally() {
        assert_eq!(describe(&Recurrence::every_minutes(30).unwrap()), "every 30 minutes");
        assert_eq!(describe(&Recurrence::every_minutes(60).unwrap()), "every hour");
        assert_eq!(describe(&Recurrence::every_minutes(180).unwrap()), "every 3 hours");
        assert_eq!(describe(&Recurrence::daily_at(9, 5).unwrap()), "daily at 09:05");
        assert_eq!(describe(&Recurrence::weekly_at(0, 9, 0).unwrap()), "Mondays at 09:00");
    }

    // ---- cron ----

    /// The success criterion of the phase, stated as an assertion: the
    /// recurrence the presets could not express.
    #[test]
    fn a_weekday_cron_fires_on_weekdays_and_never_on_a_weekend() {
        let r = Recurrence::cron("0 9 * * 1-5").expect("valid");
        let mut t = at("2026-09-04 12:00:00"); // a Friday, after 09:00
        let mut seen = Vec::new();
        for _ in 0..10 {
            t = r.next_after(t);
            seen.push(t);
        }
        for fire in &seen {
            assert_eq!(fire.hour(), 9, "every fire is at 09:00 local");
            assert_eq!(fire.minute(), 0);
            assert!(
                !matches!(fire.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun),
                "{fire} fell on a weekend"
            );
        }
        // Ten weekday fires occupy eleven calendar days, not nine: the walk
        // steps over one weekend. Pinning the span proves the weekend was
        // *skipped* rather than the fires merely landing on odd days — a bug
        // that added 24 hours blindly would land seen[9] nine days out.
        assert_eq!(seen[0].weekday(), chrono::Weekday::Mon, "the next one skips the weekend");
        assert_eq!(seen[9] - seen[0], Duration::days(11), "ten weekday fires span one weekend");
    }

    /// Strictly after, never equal — the same rule the preset variants keep, and
    /// the one that stops a schedule re-firing at the instant it just fired.
    #[test]
    fn a_cron_fire_is_strictly_after_the_instant_asked_about() {
        let r = Recurrence::cron("0 9 * * *").expect("valid");
        let nine = at("2026-09-04 09:00:00");
        assert!(r.next_after(nine) > nine, "09:00 must not resolve to itself");
        assert_eq!(r.next_after(nine), at("2026-09-05 09:00:00"));
    }

    #[test]
    fn a_cron_that_will_not_parse_is_refused_with_the_parsers_own_words() {
        let err = Recurrence::cron("not a cron").unwrap_err();
        let RecurrenceError::BadCron(msg) = &err else {
            panic!("expected BadCron, got {err:?}");
        };
        assert!(!msg.is_empty(), "the message a user reads must not be empty");
        // Six and seven field patterns are refused rather than reinterpreted:
        // silently treating the leading field as seconds would change what the
        // user asked for.
        for extra in ["0 0 9 * * 1-5", "0 9 * * 1-5 2026"] {
            assert!(
                matches!(Recurrence::cron(extra), Err(RecurrenceError::BadCron(_))),
                "{extra} must be refused, not read as a seconds/year pattern"
            );
        }
    }

    /// A pattern that parses and then never comes around. Accepting one would
    /// create a schedule that looks armed and silently never runs.
    #[test]
    fn a_cron_that_can_never_fire_is_refused() {
        for never in ["0 9 30 2 *", "0 9 31 4 *"] {
            assert_eq!(
                Recurrence::cron(never),
                Err(RecurrenceError::CronNeverFires),
                "{never} names a date the calendar never reaches"
            );
        }
        // February 29th is the near miss: rare, but real, and must be allowed.
        assert!(Recurrence::cron("0 9 29 2 *").is_ok(), "leap days do come around");
    }

    /// The floor holds on the cron path too, or `MIN_INTERVAL_MINUTES`'s claim
    /// that a runaway schedule "cannot be created at all" becomes false.
    #[test]
    fn a_cron_tighter_than_the_floor_is_refused() {
        for runaway in ["* * * * *", "*/2 * * * *", "0,1 9 * * *", "0,2,4 * * * *"] {
            assert_eq!(
                Recurrence::cron(runaway),
                Err(RecurrenceError::CronTooFrequent),
                "{runaway} fires inside the floor"
            );
        }
    }

    /// The other half of the floor: a pattern that merely *looks* dense must
    /// still be accepted, or the check is just refusing cron.
    #[test]
    fn a_cron_at_or_above_the_floor_is_accepted() {
        for ok in ["*/5 * * * *", "*/10 * * * *", "0 * * * *", "0 9 * * 1-5", "0 0 1 1 *"] {
            assert!(Recurrence::cron(ok).is_ok(), "{ok} respects the floor");
        }
    }

    /// The gap that trips the floor need not be the first one.
    ///
    /// `0,3 9 * * *` fires at 09:00 and 09:03. Probed from 09:01 the first gap
    /// is nearly a full day (09:03 today to 09:00 tomorrow) and only the
    /// *second* is the three-minute one — so a check that looked at one gap and
    /// stopped would accept this.
    ///
    /// Driven through `check_cron_cadence` with an explicit instant rather than
    /// through `Recurrence::cron`, which probes from `Local::now()`: run at the
    /// wrong hour that would take the easy path and see the tight gap first,
    /// making the test pass for a reason it does not claim.
    #[test]
    fn the_floor_looks_past_the_first_gap() {
        let pattern = parse_cron("0,3 9 * * *").expect("valid");
        assert_eq!(
            check_cron_cadence(&pattern, at("2026-09-04 09:01:00")),
            Err(RecurrenceError::CronTooFrequent),
            "the tight gap is the second one from here"
        );
        // And the public constructor agrees, whatever the clock says.
        assert_eq!(Recurrence::cron("0,3 9 * * *"), Err(RecurrenceError::CronTooFrequent));
    }

    #[test]
    fn a_cron_expression_is_stored_as_typed_but_trimmed() {
        assert_eq!(
            Recurrence::cron("  0 9 * * 1-5 ").unwrap(),
            Recurrence::Cron("0 9 * * 1-5".into())
        );
    }

    /// The phrasing that reaches a phone. It must compose with the caller's
    /// `"{recurrence} · {next fire}"` line the way the presets do — lower case,
    /// no trailing stop.
    #[test]
    fn a_cron_describes_itself_in_the_house_style() {
        let described = describe(&Recurrence::cron("0 9 * * 1-5").unwrap());
        assert!(!described.ends_with('.'), "no trailing stop: {described}");
        assert!(
            described.starts_with(|c: char| c.is_lowercase()),
            "starts lower case like `daily at 09:00`: {described}"
        );
        assert!(described.contains("09:00"), "names the time: {described}");
        assert!(described.contains("Monday"), "names the days: {described}");
        // The presets are unchanged by any of this.
        assert_eq!(describe(&Recurrence::daily_at(9, 0).unwrap()), "daily at 09:00");
        assert_eq!(describe(&Recurrence::every_minutes(30).unwrap()), "every 30 minutes");
    }

    /// An unparseable expression can only reach `describe` through a corrupted
    /// row, and showing the user their own text beats showing them nothing.
    #[test]
    fn describing_an_unparseable_cron_falls_back_to_the_expression() {
        assert_eq!(describe(&Recurrence::Cron("garbage".into())), "garbage");
    }

    /// A `Cron` whose expression cannot parse must go inert, not fire in a
    /// loop — the same choice `next_wall_clock` makes for its own unreachable
    /// branch.
    #[test]
    fn an_unparseable_cron_goes_inert_rather_than_firing_constantly() {
        let now = at("2026-09-04 12:00:00");
        let next = Recurrence::Cron("garbage".into()).next_after(now);
        assert!(next - now >= Duration::days(300), "a broken rule must not fire soon");
    }
}
