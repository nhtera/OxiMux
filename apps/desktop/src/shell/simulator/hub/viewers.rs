//! Viewers of a worktree's device beyond its window's panel: the grid's
//! tiles (and, later, remote views). The registry counts them
//! ([`oximux_simulator::viewers`]); the hub numbers tiles and applies the
//! stream profile the registry settles on.
//!
//! A lower profile is applied at the source only where the source can
//! change it live and nothing else depends on its size: a simulator's
//! helper takes a new scale and frame rate per session. A real iPhone keeps
//! its stream (its points per pixel and its recordings come from it), and
//! an Android stream's size and rate are fixed when scrcpy starts (a
//! restart would break an agent's `settle`): an unfocused tile of either
//! only paints less often (see `ScreenView`).
//!
//! Configure calls run in the background, one device at a time and in
//! order: of several asked in a row, only the newest is sent, so a focus
//! that moves twice in a tick never leaves the older profile behind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::Context;
use oximux_simulator::DeviceId;
use oximux_simulator::registry::WorktreeKey;
use oximux_simulator::session::StreamSession;
use oximux_simulator::viewers::{StreamProfile, Viewer, ViewerId};

use super::SimulatorHub;

/// An unfocused tile's frame rate at the source (and on the Mac).
pub(crate) const REDUCED_FPS: f64 = 10.0;
/// An unfocused tile's scale, as a fraction of the user's.
const REDUCED_SCALE: f64 = 0.5;

/// One device's configure calls: taken in turn, newest wins.
#[derive(Default)]
pub(super) struct Turns {
    lock: Mutex<()>,
    newest: AtomicU64,
}

impl SimulatorHub {
    /// A number for a new viewer (a tile, a window's panel), unique for the run.
    pub(crate) fn next_viewer(&mut self) -> u32 {
        self.next_viewer += 1;
        self.next_viewer
    }

    /// A viewer of `worktree`'s device was shown, hidden or refocused.
    pub(crate) fn set_viewer(&mut self, worktree: &Path, id: ViewerId, viewer: Viewer, cx: &mut Context<Self>) {
        let effects = self.registry.set_viewer(&WorktreeKey::from_path(worktree), id, viewer, Instant::now());
        self.run(effects, cx);
    }

    /// The viewer went away (its tile closed, or its window).
    pub(crate) fn release_viewer(&mut self, worktree: &Path, id: ViewerId, cx: &mut Context<Self>) {
        let effects = self.registry.release_viewer(&WorktreeKey::from_path(worktree), id, Instant::now());
        self.run(effects, cx);
    }

    /// Whether any viewer shows `udid` now.
    #[cfg(test)]
    pub(crate) fn is_shown(&self, udid: &DeviceId) -> bool {
        self.registry.is_shown(udid)
    }

    /// Every worktree with a device attached, app-wide.
    pub(crate) fn attachments(&self) -> Vec<(PathBuf, DeviceId)> {
        self.registry.attachments().into_iter().map(|(w, d)| (w.path().to_path_buf(), d)).collect()
    }

    /// The scale and fps to stream `udid`'s `session` at, from the user's:
    /// lowered while only unfocused tiles show a simulator.
    pub(super) fn stream_values(&self, udid: &DeviceId, session: &StreamSession, scale: f64, fps: f64) -> (f64, f64) {
        let lowered = self.registry.profile(udid) == StreamProfile::Reduced && matches!(session, StreamSession::Ios(_));
        if lowered { (scale * REDUCED_SCALE, fps.min(REDUCED_FPS)) } else { (scale, fps) }
    }

    /// The registry settled on a new profile for `udid`'s session.
    pub(super) fn apply_profile(&mut self, udid: &DeviceId, session: StreamSession, cx: &mut Context<Self>) {
        if !matches!(session, StreamSession::Ios(_)) {
            return; // its source keeps streaming as it is: the Mac paints less
        }
        let stream = cx
            .try_global::<crate::app_settings::simulator_settings::SimulatorSettings>()
            .map(|s| s.stream)
            .unwrap_or_default();
        let (scale, fps) = self.stream_values(udid, &session, f64::from(stream.effective_scale()), f64::from(stream.fps));
        tracing::debug!(%udid, scale, fps, "simulator stream profile");
        let target = udid.clone();
        // Quietly: the key frame (or JPEG size) it brings is not change on
        // screen to an agent's settle.
        self.in_turn(udid, cx, move || session.restream(scale, fps, Duration::from_secs(5)).map(drop), move |e| {
            tracing::debug!(udid = %target, "simulator stream profile change failed: {e}");
        });
    }

    /// Run `call` for `udid` in the background, after any earlier one for
    /// it, unless a newer one was queued meanwhile.
    pub(super) fn in_turn<E: Send + 'static>(
        &mut self,
        udid: &DeviceId,
        cx: &mut Context<Self>,
        call: impl FnOnce() -> Result<(), E> + Send + 'static,
        failed: impl FnOnce(E) + Send + 'static,
    ) {
        let turns = self.turns.entry(udid.clone()).or_default().clone();
        let mine = turns.newest.fetch_add(1, Ordering::AcqRel) + 1;
        cx.background_executor()
            .spawn(async move {
                let _turn = turns.lock.lock().unwrap_or_else(PoisonError::into_inner);
                if turns.newest.load(Ordering::Acquire) != mine {
                    return; // a newer call follows
                }
                if let Err(e) = call() {
                    failed(e);
                }
            })
            .detach();
    }
}
