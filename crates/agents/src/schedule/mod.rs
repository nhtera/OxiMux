//! Scheduled agent runs: what repeats, when it fires next, and where that is
//! stored.
//!
//! **A schedule fires only while the desktop app is running.** There is no
//! `launchd` job and no headless binary — a scheduled run is a ticker inside the
//! GPUI process. Keep-awake stops the Mac idle-sleeping while the app is open,
//! but nothing here wakes a sleeping Mac or relaunches a quit app, so an
//! overnight run does not happen unless the app was left running. That limit is
//! a deliberate v1 scope decision, and every surface that lets someone create a
//! schedule is expected to say so before they commit to one — a missed run is
//! otherwise indistinguishable from a broken feature.
//!
//! The pieces are split so the parts that are easy to get wrong are testable
//! without a database or a UI: [`recurrence`] is pure time arithmetic, and
//! [`store`] is persistence with no opinion about when anything runs.

pub mod recurrence;
pub mod store;

pub use recurrence::{Recurrence, RecurrenceError, describe};
pub use store::{NewSchedule, Schedule, ScheduleRun, ScheduleStore, RunOutcome};
