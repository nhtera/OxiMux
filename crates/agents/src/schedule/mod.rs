//! Scheduled agent runs: what repeats, when it fires next, where that is
//! stored, and the shared engine that fires it.
//!
//! **A schedule fires only while an OxiMux host is running** — the desktop app
//! or `oximux serve`; whichever holds the data dir's ticker lock ticks (see
//! [`ticker`]). There is no `launchd` job: a scheduled run is a ticker inside
//! a host process. The desktop's keep-awake stops the Mac idle-sleeping while
//! it holds the role, but nothing here wakes a sleeping machine or relaunches
//! a quit host, so an overnight run needs a host left running. Every surface
//! that lets someone create a schedule is expected to say so before they
//! commit to one — a missed run is otherwise indistinguishable from a broken
//! feature.
//!
//! The pieces are split so the parts that are easy to get wrong are testable
//! without a database or a UI: [`recurrence`] is pure time arithmetic,
//! [`store`] is persistence with no opinion about when anything runs, and
//! [`ticker`] is the engine with no opinion about how a host fires.

pub mod recurrence;
pub mod store;
pub mod ticker;

pub use recurrence::{Recurrence, RecurrenceError, describe};
pub use store::{NewSchedule, Schedule, ScheduleRun, ScheduleStore, ScheduleTarget, RunOutcome};
pub use ticker::{FireOutcome, RunNowError, ScheduleFirer, TICK, TICKER_LOCK_FILENAME, Ticker};
