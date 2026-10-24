//! The per-backend postures a chat persists and respawns under (Pi's tool
//! gate, omp's approval mode, Codex's approval policy and sandbox, Claude's
//! fast mode), read back from the composer's feature picks. Moved verbatim out of `mod.rs` for
//! file-size headroom.

use oximux_agents::thread::pi::posture::{self as pi_posture, PiPosture};
use oximux_agents::thread::{FeatureValue, Transport};

use super::AgentChatView;

impl AgentChatView {
    /// Claude's fast-mode pick as the user last set it, read from the composer's
    /// feature picks. `None` for a non-Claude chat or when the toggle was never
    /// touched. This is what persists; whether it is *applied* to a spawn is
    /// [`super::claude_fast_mode_to_apply`]'s call.
    pub(super) fn claude_fast_mode_snapshot(&self) -> Option<bool> {
        if self.backend.transport != Transport::StreamJson {
            return None;
        }
        match self.feature_values.get(oximux_agents::thread::FEATURE_FAST_MODE) {
            Some(FeatureValue::Bool(on)) => Some(*on),
            _ => None,
        }
    }

    /// Pi's tool posture, read from the composer's feature picks. `None` for a
    /// non-Pi chat or when nothing was changed (restore then applies the
    /// deliberate default).
    ///
    /// Unlike Codex's, this posture is the session's ONLY tool gate — pi never
    /// asks before running anything — so it is snapshotted for persistence
    /// rather than left to be re-derived.
    pub(super) fn pi_posture_snapshot(&self) -> Option<PiPosture> {
        if self.backend.transport != Transport::Rpc {
            return None;
        }
        let tools = match self.feature_values.get(pi_posture::FEATURE_TOOLS) {
            Some(FeatureValue::Choice(wire)) => Some(wire.clone()),
            _ => None,
        };
        let context_files = match self.feature_values.get(pi_posture::FEATURE_CONTEXT_FILES) {
            Some(FeatureValue::Bool(on)) => Some(*on),
            _ => None,
        };
        if tools.is_none() && context_files.is_none() {
            return None;
        }
        Some(PiPosture::from_parts(tools.as_deref(), context_files))
    }

    /// omp's approval posture, read from the composer's feature picks. `None`
    /// for a non-omp chat or when the picker was never touched (restore then
    /// applies the deliberate Write default — the spawn flag stays explicit
    /// either way, so omp's own yolo default is unreachable).
    pub(super) fn omp_posture_snapshot(&self) -> Option<oximux_agents::thread::omp::posture::OmpPosture> {
        if self.backend.transport != Transport::OmpRpc {
            return None;
        }
        match self.feature_values.get(oximux_agents::thread::omp::posture::FEATURE_APPROVALS) {
            Some(FeatureValue::Choice(wire)) => {
                oximux_agents::thread::omp::posture::OmpPosture::from_wire(wire)
            }
            _ => None,
        }
    }

    /// The Codex posture `(approval_policy, sandbox)` the user has selected, read
    /// from the composer's feature picks. `None` for a non-Codex chat or when the
    /// posture was never changed from the default (nothing to persist).
    pub(super) fn codex_posture_snapshot(&self) -> Option<(String, String)> {
        if self.backend.transport != Transport::AppServer {
            return None;
        }
        let choice = |id: &str| match self.feature_values.get(id) {
            Some(FeatureValue::Choice(wire)) => Some(wire.clone()),
            _ => None,
        };
        match (choice("codex_approval_policy"), choice("codex_sandbox")) {
            (None, None) => None,
            (approval, sandbox) => Some((
                approval.unwrap_or_else(|| "on-request".to_string()),
                sandbox.unwrap_or_else(|| "workspace-write".to_string()),
            )),
        }
    }
}
