//! Usage-meter data model.
//!
//! The status-bar meter shows each configured agent account's REAL rate-limit
//! utilization, taken from whatever that provider itself publishes — never
//! estimated from local logs. When a source cannot be read the meter reports
//! [`UsageState::Unavailable`] with the reason, rather than guessing a
//! denominator and presenting a fabricated percentage as if it were real.
//!
//! Providers disagree about how many windows they have and how long those
//! windows are, so a reading is a *list* of labeled windows rather than a
//! fixed pair. Labels are derived from the span (see
//! [`UsageWindow::short_label`]), which means a provider reporting a window
//! shape OxiMux has never seen still renders something true.

/// A five-hour window's span, in minutes — the short rolling limit both
/// current sources report.
pub const FIVE_HOUR_MINUTES: u32 = 300;
/// A seven-day window's span, in minutes.
pub const WEEK_MINUTES: u32 = 10_080;

/// One rate-limit window: how long it spans, how much of it is used, and when
/// it resets.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageWindow {
    /// Span in minutes — [`FIVE_HOUR_MINUTES`] or [`WEEK_MINUTES`] for the
    /// windows in use today. Stored rather than an enum so an unfamiliar span
    /// degrades to a plain `90m` label instead of being dropped.
    pub window_minutes: u32,
    /// Utilization 0–100, straight from the provider.
    pub utilization: f64,
    /// When the window resets (unix ms). `None` when the provider did not say
    /// — a rolling window, or an idle one with no active block.
    pub resets_at_ms: Option<i64>,
}

impl UsageWindow {
    /// Used fraction in [0, 1]; the meter renders this as `NN%`.
    pub fn ratio(&self) -> f32 {
        (self.utilization / 100.0).clamp(0.0, 1.0) as f32
    }

    /// Compact label for the status-bar chip: `5h`, `wk`, `90m`.
    pub fn short_label(&self) -> String {
        match self.window_minutes {
            WEEK_MINUTES => "wk".to_string(),
            m if m >= 60 && m % 60 == 0 => format!("{}h", m / 60),
            m => format!("{m}m"),
        }
    }

    /// Name for the popover's window block.
    pub fn name(&self) -> String {
        match self.window_minutes {
            FIVE_HOUR_MINUTES => "Session".to_string(),
            WEEK_MINUTES => "Weekly".to_string(),
            _ => self.short_label(),
        }
    }

    /// What replaces the countdown when there is no reset time to count to.
    pub fn idle_note(&self) -> &'static str {
        match self.window_minutes {
            WEEK_MINUTES => "rolling 7 days",
            _ => "no active block",
        }
    }

    /// Whether this reading still describes a window that is running.
    ///
    /// A window past its reset carries a utilization for a period that no
    /// longer exists. A source fetched live never hits this — the server
    /// computes the number on the spot — but a source read off disk hits it
    /// constantly, and showing the old percentage would be exactly the
    /// fabricated number this meter exists to refuse.
    pub fn is_current(&self, now_ms: i64) -> bool {
        self.resets_at_ms.is_none_or(|t| t > now_ms)
    }
}

/// A usage reading: every window the provider reports, plus the plan they
/// belong to.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSnapshot {
    /// The provider's windows, shortest span first.
    pub windows: Vec<UsageWindow>,
    /// Raw plan/tier slug for display (e.g. `default_claude_max_5x`, `plus`);
    /// empty when the provider didn't say.
    pub tier: String,
    /// `Some(t)` when the reading was *captured* at unix-ms `t` rather than
    /// fetched live this tick — either a cached reading standing in for a
    /// failed live fetch, or a source that only ever publishes to disk. The UI
    /// keeps the numbers visible but discloses "updated N ago" rather than
    /// passing a stale reading off as live. `None` when the reading is current.
    pub captured_at_ms: Option<i64>,
}

impl UsageSnapshot {
    /// The window closest to its ceiling — what the compact chip shows and
    /// what colors the meter. Ties keep the shorter span, which sorts first.
    /// `None` only when every window has been dropped as no longer current.
    pub fn tightest(&self) -> Option<&UsageWindow> {
        self.windows
            .iter()
            .reduce(|best, w| if w.utilization > best.utilization { w } else { best })
    }
}

/// Which agent account a reading belongs to.
///
/// A meter row exists per variant; the display name and icon are the UI's
/// business, but the slug is shared with the agent registry so a row can be
/// traced back to the adapter it describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageProvider {
    ClaudeCode,
    Codex,
}

impl UsageProvider {
    /// Stable slug, matching the agent registry's ids.
    pub fn slug(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    /// Name for the popover block header.
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

/// One provider's meter reading. A provider that left no trace on this machine
/// produces no `ProviderUsage` at all — the meter shows a row for accounts the
/// user actually has, and a failing row only for one that is set up.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub provider: UsageProvider,
    pub state: UsageState,
}

/// What the status-bar meter should display for one provider this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum UsageState {
    /// A live or recently-captured exact reading.
    Available(UsageSnapshot),
    /// The source could not be read or authenticated; the meter shows an
    /// "unavailable" segment and surfaces this reason in its popover.
    Unavailable { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(minutes: u32, utilization: f64) -> UsageWindow {
        UsageWindow {
            window_minutes: minutes,
            utilization,
            resets_at_ms: None,
        }
    }

    #[test]
    fn ratio_clamps_to_unit_interval() {
        assert_eq!(window(FIVE_HOUR_MINUTES, 0.0).ratio(), 0.0);
        assert_eq!(window(FIVE_HOUR_MINUTES, 50.0).ratio(), 0.5);
        assert_eq!(window(FIVE_HOUR_MINUTES, 130.0).ratio(), 1.0);
    }

    #[test]
    fn labels_derive_from_the_span() {
        assert_eq!(window(FIVE_HOUR_MINUTES, 0.0).short_label(), "5h");
        assert_eq!(window(WEEK_MINUTES, 0.0).short_label(), "wk");
        assert_eq!(window(FIVE_HOUR_MINUTES, 0.0).name(), "Session");
        assert_eq!(window(WEEK_MINUTES, 0.0).name(), "Weekly");
    }

    #[test]
    fn an_unfamiliar_span_still_labels() {
        // A provider reporting something neither source uses today must render
        // a true label rather than be dropped or mislabeled as one of the two.
        assert_eq!(window(90, 0.0).short_label(), "90m");
        assert_eq!(window(90, 0.0).name(), "90m");
        assert_eq!(window(180, 0.0).short_label(), "3h");
    }

    #[test]
    fn tightest_is_the_most_used_window() {
        let snap = UsageSnapshot {
            windows: vec![window(FIVE_HOUR_MINUTES, 12.0), window(WEEK_MINUTES, 40.0)],
            tier: String::new(),
            captured_at_ms: None,
        };
        assert_eq!(snap.tightest().unwrap().window_minutes, WEEK_MINUTES);
    }

    #[test]
    fn tightest_keeps_the_shorter_span_on_a_tie() {
        let snap = UsageSnapshot {
            windows: vec![window(FIVE_HOUR_MINUTES, 20.0), window(WEEK_MINUTES, 20.0)],
            tier: String::new(),
            captured_at_ms: None,
        };
        assert_eq!(snap.tightest().unwrap().window_minutes, FIVE_HOUR_MINUTES);
    }

    #[test]
    fn tightest_of_an_empty_reading_is_none() {
        let snap = UsageSnapshot {
            windows: vec![],
            tier: String::new(),
            captured_at_ms: None,
        };
        assert!(snap.tightest().is_none());
    }

    #[test]
    fn a_window_past_its_reset_is_no_longer_current() {
        let mut w = window(FIVE_HOUR_MINUTES, 23.0);
        w.resets_at_ms = Some(1_000);
        assert!(w.is_current(999));
        assert!(!w.is_current(1_000), "the reset instant itself is over");
        assert!(!w.is_current(1_001));
        // No reset time to pass: a rolling window is always current.
        assert!(window(WEEK_MINUTES, 5.0).is_current(i64::MAX));
    }
}
