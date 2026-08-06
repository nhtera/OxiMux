//! The manual-fire seam behind [`Request::RunScheduleNow`], plus the shared
//! store-row → wire conversion every schedule surface uses.
//!
//! Like [`SessionLauncher`](crate::SessionLauncher), the real work lives above
//! this crate — the ticker that owns the in-flight guard and the durable claim
//! is host-side, because only the process holding the ticker lock may fire.
//! The dispatcher checks *authorization*; the runner reports whether the fire
//! could happen and how it went.

use oximux_agents::schedule::{RunOutcome, ScheduleRun, Ticker, ticker::RunNowError as TickerError};
use oximux_remote_proto::messages::{RunOutcomeWire, ScheduleRunWire};

/// Why a manual fire was refused (as opposed to firing and failing, which the
/// reply's run row reports). Curated: no host paths, no store error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunNowError {
    /// No schedule has that id.
    NoSuchSchedule,
    /// The schedule is already firing (the tick loop or another run-now).
    AlreadyFiring,
    /// The host cannot start a session right now (draining, or nothing to
    /// host the run) — nothing was recorded; try again later.
    Unavailable,
    /// The run could not be recorded. Details are in the host's log.
    Failed,
}

/// How a host fires one schedule on demand.
#[async_trait::async_trait]
pub trait ScheduleRunner: Send + Sync {
    /// Fire now, record the run without advancing cadence, and return it.
    async fn run_now(&self, schedule_id: &str) -> Result<ScheduleRunWire, RunNowError>;
}

/// The one real implementation: any host holding the shared [`Ticker`] runs
/// manual fires through it, so run-now and the tick loop contend on the same
/// in-flight guard instead of racing each other.
pub struct TickerRunner(pub std::sync::Arc<Ticker>);

#[async_trait::async_trait]
impl ScheduleRunner for TickerRunner {
    async fn run_now(&self, schedule_id: &str) -> Result<ScheduleRunWire, RunNowError> {
        match self.0.run_now(schedule_id, chrono::Local::now()).await {
            Ok(run) => Ok(schedule_run_to_wire(&run)),
            Err(TickerError::NotFound) => Err(RunNowError::NoSuchSchedule),
            Err(TickerError::Busy) => Err(RunNowError::AlreadyFiring),
            Err(TickerError::NotNow) => Err(RunNowError::Unavailable),
            Err(TickerError::Store(detail)) => {
                tracing::warn!(%detail, schedule_id, "manual fire could not be recorded");
                Err(RunNowError::Failed)
            }
        }
    }
}

/// Store row → wire. Shared by the dispatcher's history reply, the run-now
/// reply, and the hosts' recorded-run push, so all three ship one shape.
pub fn schedule_run_to_wire(r: &ScheduleRun) -> ScheduleRunWire {
    ScheduleRunWire {
        schedule_id: r.schedule_id.clone(),
        fired_at: r.fired_at.to_rfc3339(),
        outcome: match r.outcome {
            RunOutcome::Ok => RunOutcomeWire::Ok,
            RunOutcome::Failed => RunOutcomeWire::Failed,
        },
        session_id: r.session_id.clone(),
        detail: r.detail.clone(),
    }
}
