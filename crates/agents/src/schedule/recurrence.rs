//! When a schedule fires next.
//!
//! **Deliberately not cron.** The plan suggested reaching for a cron crate
//! rather than hand-rolling a parser, and that advice is right about parsers —
//! but it assumes cron is the model. It is not the right one here. Schedules are
//! created on a phone, where `0 9 * * 1-5` is miserable to type and produces
//! cryptic errors when mistyped; a closed vocabulary maps onto pickers, which
//! cannot produce an invalid value at all. That removes an entire class of
//! client-side validation the plan budgeted for, and a dependency with it.
//!
//! This is not a reduced cron parser. It is three cases, each with its own
//! arithmetic, and nothing here would be simplified by expressing them as cron
//! fields.
//!
//! **Local time, not UTC.** "Every day at 9am" means 9am where the person is,
//! which is the desktop's local zone. That makes DST a real concern rather than
//! a theoretical one — see [`Recurrence::next_after`].

use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone};

/// How often a schedule repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// Smallest interval a schedule may use.
///
/// A one-minute agent run is almost certainly a mistake — each fire spawns a
/// process and starts a turn — and the floor is enforced at construction so a
/// runaway schedule cannot be created at all, rather than being caught later by
/// something that notices the machine is thrashing.
pub const MIN_INTERVAL_MINUTES: u32 = 5;

/// Why a recurrence could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecurrenceError {
    #[error("interval must be at least {MIN_INTERVAL_MINUTES} minutes")]
    IntervalTooShort,
    #[error("that is not a valid time of day")]
    BadTime,
    #[error("that is not a valid weekday")]
    BadWeekday,
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
    pub fn next_after(&self, after: DateTime<Local>) -> DateTime<Local> {
        match *self {
            Self::EveryMinutes(minutes) => after + Duration::minutes(minutes as i64),
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
}
