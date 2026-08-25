//! omp's session approval posture — the `--approval-mode` spawn flag.
//!
//! Unlike Pi (whose gating is a spawn-time tool ALLOWLIST with no per-call
//! approval anywhere), omp has real tiered tool approval: under `always-ask`
//! every tool asks, under `write` only write/exec-tier tools ask, and under
//! `yolo` nothing asks. The mode is fixed at spawn for the rpc-ui protocol —
//! changing it means a respawn, exactly like Pi's posture.
//!
//! 🚨 **omp's own default is `yolo`** (`settings-schema.ts` —
//! `tools.approvalMode: "yolo"`, auto-approve everything including exec), and
//! rpc-mode setting overrides do not reset it. OxiMux therefore NEVER omits
//! the flag: every spawn and respawn emits `--approval-mode <mode>`
//! explicitly, with [`OmpPosture::default`] = `Write` as the deliberate
//! OxiMux default. `to_args` is the single source of those flags, and its
//! unconditional emit is locked by test.

use serde::{Deserialize, Serialize};

/// The three approval modes omp's `--approval-mode` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OmpPosture {
    /// Every tool call asks first.
    AlwaysAsk,
    /// Read-tier tools run free; write/exec-tier tools ask. The OxiMux
    /// default — omp's own default (`yolo`) auto-approves exec, which is not a
    /// posture to put a user in without their say-so.
    #[default]
    Write,
    /// Nothing asks. Explicitly chosen only.
    Yolo,
}

/// The feature-control id the composer's posture picker uses.
pub const FEATURE_APPROVALS: &str = "omp.approvals";

/// Wire strings, matching omp's `--approval-mode` values.
pub const APPROVAL_ALWAYS_ASK: &str = "always-ask";
pub const APPROVAL_WRITE: &str = "write";
pub const APPROVAL_YOLO: &str = "yolo";

impl OmpPosture {
    /// The `--approval-mode` value.
    pub fn wire(&self) -> &'static str {
        match self {
            OmpPosture::AlwaysAsk => APPROVAL_ALWAYS_ASK,
            OmpPosture::Write => APPROVAL_WRITE,
            OmpPosture::Yolo => APPROVAL_YOLO,
        }
    }

    /// Parse a wire string (the picker's selection, a persisted blob).
    pub fn from_wire(wire: &str) -> Option<Self> {
        match wire {
            APPROVAL_ALWAYS_ASK => Some(OmpPosture::AlwaysAsk),
            APPROVAL_WRITE => Some(OmpPosture::Write),
            APPROVAL_YOLO => Some(OmpPosture::Yolo),
            _ => None,
        }
    }

    /// The spawn flags. ALWAYS two entries — an omitted flag would silently
    /// fall to omp's own `yolo` default, which is the exact hazard this type
    /// exists to close.
    pub fn to_args(&self) -> Vec<String> {
        vec!["--approval-mode".to_string(), self.wire().to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_always_emitted_whatever_the_posture() {
        // omp's OWN default is yolo; relying on "default" would auto-approve
        // exec for a user who never touched the picker. Every posture —
        // including the default one — must spell the flag out.
        for p in [OmpPosture::AlwaysAsk, OmpPosture::Write, OmpPosture::Yolo] {
            let args = p.to_args();
            assert_eq!(args[0], "--approval-mode");
            assert_eq!(args.len(), 2);
        }
        assert_eq!(OmpPosture::default().to_args(), vec!["--approval-mode", "write"]);
    }

    #[test]
    fn wire_round_trips() {
        for p in [OmpPosture::AlwaysAsk, OmpPosture::Write, OmpPosture::Yolo] {
            assert_eq!(OmpPosture::from_wire(p.wire()), Some(p));
        }
        assert_eq!(OmpPosture::from_wire("nope"), None);
    }

    #[test]
    fn the_default_is_write_not_omps_yolo() {
        assert_eq!(OmpPosture::default(), OmpPosture::Write);
    }
}
