//! Autosave settings — the editor's debounced save cadence.
//!
//! Held as a GPUI [`Global`] so the editor reads a single source of truth:
//! startup installs the value (default or user-configured) and every editor
//! view's autosave pump snapshots it. Defaults to ON with a 1000ms debounce —
//! frequent enough that an unsaved edit window stays small, lazy enough that it
//! doesn't thrash the disk on every keystroke. The debounce is clamped to a
//! sane range so a mis-set config can neither starve saves nor hammer the FS.

use std::time::Duration;

use gpui::Global;

/// Lower bound on the debounce — below this autosave starts to thrash on fast
/// typing.
pub const MIN_DEBOUNCE_MS: u64 = 250;
/// Upper bound — beyond this the unsaved-edit window grows uncomfortably large.
pub const MAX_DEBOUNCE_MS: u64 = 10_000;
/// Default debounce (parity with the benchmark editor's autosave cadence).
pub const DEFAULT_DEBOUNCE_MS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutosaveSettings {
    /// When false the pump never writes; the dirty-close guard is the only
    /// data-safety net (its purpose).
    pub enabled: bool,
    /// Idle delay after the last edit before a save fires. Read via
    /// [`AutosaveSettings::debounce`] which clamps it.
    pub debounce_ms: u64,
}

impl Default for AutosaveSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        }
    }
}

impl AutosaveSettings {
    /// Debounce clamped to `[MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS]`.
    pub fn clamped_debounce_ms(&self) -> u64 {
        self.debounce_ms.clamp(MIN_DEBOUNCE_MS, MAX_DEBOUNCE_MS)
    }

    /// Clamped debounce as a [`Duration`] — what the pump actually waits.
    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.clamped_debounce_ms())
    }
}

impl Global for AutosaveSettings {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_on_at_one_second() {
        let s = AutosaveSettings::default();
        assert!(s.enabled);
        assert_eq!(s.debounce_ms, 1000);
        assert_eq!(s.debounce(), Duration::from_millis(1000));
    }

    #[test]
    fn debounce_clamps_both_ends() {
        let too_fast = AutosaveSettings {
            enabled: true,
            debounce_ms: 10,
        };
        assert_eq!(too_fast.clamped_debounce_ms(), MIN_DEBOUNCE_MS);
        let too_slow = AutosaveSettings {
            enabled: true,
            debounce_ms: 60_000,
        };
        assert_eq!(too_slow.clamped_debounce_ms(), MAX_DEBOUNCE_MS);
    }
}
