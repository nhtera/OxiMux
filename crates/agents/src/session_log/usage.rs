//! Usage-meter data model.
//!
//! The status-bar meter shows the account's REAL rate-limit utilization,
//! fetched from the primary CLI's account usage API (see `usage_oauth`) —
//! the same numbers its own `/usage` panel renders, account-wide across
//! devices. There is deliberately no local-session-log estimate: when the
//! API can't be reached or authenticated the meter reports
//! [`UsageState::Unavailable`] with the reason, rather than guessing a
//! denominator and presenting a fabricated percentage as if it were real.

/// One rate-limit window: utilization percentage + reset time.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Utilization 0–100, straight from the account usage API.
    pub utilization: f64,
    /// When the window resets (unix ms). `None` for the rolling weekly
    /// window and for an idle 5-hour window with no active block.
    pub resets_at_ms: Option<i64>,
}

impl UsageWindow {
    /// Used fraction in [0, 1]; the meter renders this as `NN%`.
    pub fn ratio(&self) -> f32 {
        (self.utilization / 100.0).clamp(0.0, 1.0) as f32
    }
}

/// A usage reading: both windows plus the account tier they belong to.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    pub five_hour: UsageWindow,
    pub weekly: UsageWindow,
    /// Raw tier slug for display (e.g. `default_claude_max_5x`); empty when
    /// the account config didn't provide one.
    pub tier: String,
    /// `Some(t)` when this is a *cached* reading captured at unix-ms `t` (the
    /// live fetch was unavailable this tick) — the UI keeps the numbers
    /// visible but discloses "updated N ago" rather than passing a slightly
    /// stale reading off as live. `None` when the reading is current.
    pub captured_at_ms: Option<i64>,
}

/// What the status-bar meter should display this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageState {
    /// A live or recently-cached exact reading from the account usage API.
    Available(UsageSnapshot),
    /// The usage API could not be reached or authenticated; the meter shows
    /// an "unavailable" chip and surfaces this reason in its popover.
    Unavailable { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_clamps_to_unit_interval() {
        assert_eq!(
            UsageWindow {
                utilization: 0.0,
                resets_at_ms: None
            }
            .ratio(),
            0.0
        );
        assert_eq!(
            UsageWindow {
                utilization: 50.0,
                resets_at_ms: None
            }
            .ratio(),
            0.5
        );
        assert_eq!(
            UsageWindow {
                utilization: 130.0,
                resets_at_ms: None
            }
            .ratio(),
            1.0
        );
    }
}
