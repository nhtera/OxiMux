//! AI commit-message generation lifecycle for the sparkles button.
//!
//! Owns the `AiState` enum + the `CommitArea` methods that drive
//! it (`set_staged_snapshot`, `is_generating_ai`,
//! `start_ai_generation`, `cancel_ai_generation`). Split out of
//! `commit_area.rs` so the main module stays under the workspace
//! file-size cap; the methods are still on `CommitArea` via a
//! re-opened `impl` block — call sites are unchanged.
//!
//! The split also draws a clean line between commit-area
//! lifecycle (composer, message state, primary-action surface) and
//! the heuristic generation it dispatches. When agent-mode lands
//! (v1.1) the new code lands here, not in the main module.
//!
//! Cooperative cancellation: the spawned task holds an
//! `Arc<AtomicBool>` shared with the `AiState::Generating` variant.
//! Three sites set the flag:
//!
//! 1. The Stop button on the overlay → `cancel_ai_generation`.
//! 2. The InputEvent::Change observer in `CommitArea::new` →
//!    `area.ai_state.signal_cancel()` (user typed during gen).
//! 3. Entity drop — replacing `AiState` with `Idle` drops the
//!    held `Task<()>`, which gpui cancels.
//!
//! The task itself reads the flag twice (after the spinner-paint
//! timer, then again inside the `update_in` block before mutating
//! the textarea). The second check is mandatory: gpui serialises
//! entity updates, so a cancel set inside the update window
//! (via the Change observer) lands before the task's `update_in`
//! body executes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{Context, Task, Window};
use oximux_agents::commit_message_heuristic;
use oximux_core::FileStatus;

use crate::shell::source_control::commit_area::CommitArea;

/// AI generation lifecycle. `Idle` is the default; `Generating`
/// holds the cancel flag shared with the spawned task plus the
/// task handle (dropping the handle cancels gpui's await, but the
/// cancel flag is what the task body actually polls before
/// applying its result).
pub(in crate::shell::source_control) enum AiState {
    Idle,
    Generating {
        cancel: Arc<AtomicBool>,
        /// Held for the duration of the generation so dropping the
        /// `AiState` (e.g. on entity drop or explicit reset to
        /// `Idle`) cancels the task. The task itself ALSO checks
        /// the cancel flag before mutating; this field is the
        /// gpui-side cancel handle.
        _task: Task<()>,
    },
}

impl AiState {
    pub(in crate::shell::source_control) fn is_generating(&self) -> bool {
        matches!(self, AiState::Generating { .. })
    }

    /// Set the cancel flag on the in-flight generation (if any) so
    /// the task drops its result instead of inserting it.
    /// Idempotent: calling on `Idle` is a no-op.
    pub(in crate::shell::source_control) fn signal_cancel(&self) {
        if let AiState::Generating { cancel, .. } = self {
            cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// Spinner paint delay before computing the heuristic. Tuned so a
/// generation that completes in microseconds still shows the
/// overlay for at least one frame — without this the user sees a
/// glitch flash instead of a deliberate-looking AI surface.
const SPINNER_PAINT_DELAY: Duration = Duration::from_millis(80);

impl CommitArea {
    /// Replace the cached staged-file snapshot. Called by the
    /// panel's state observer once per poll tick with the
    /// staged-filtered file list. Equality-guarded so identical
    /// snapshots don't fire a spurious `cx.notify` (sparkles
    /// button only depends on non-empty / empty, so non-meaningful
    /// list reorderings cost nothing to ignore).
    pub fn set_staged_snapshot(
        &mut self,
        snapshot: Vec<FileStatus>,
        cx: &mut Context<Self>,
    ) {
        if self.staged_snapshot == snapshot {
            return;
        }
        self.staged_snapshot = snapshot;
        cx.notify();
    }

    /// True when the heuristic generator is currently running.
    pub fn is_generating_ai(&self) -> bool {
        self.ai_state.is_generating()
    }

    /// Cancel any in-flight AI generation. Idempotent; safe to
    /// call while `Idle`. Used by the Stop button on the overlay;
    /// the user-typing cancel path goes through the existing
    /// InputEvent::Change observer in `CommitArea::new`.
    pub fn cancel_ai_generation(&mut self, cx: &mut Context<Self>) {
        if !self.ai_state.is_generating() {
            return;
        }
        self.ai_state.signal_cancel();
        // Drop the task immediately so the overlay disappears
        // synchronously rather than waiting for the task to
        // observe the cancel flag and self-clear.
        self.ai_state = AiState::Idle;
        cx.notify();
    }

    /// Begin AI generation. No-op when already generating, when
    /// no staged files exist, or when a commit is in flight. The
    /// spawned task waits one spinner-paint frame, then computes
    /// the heuristic synchronously, then checks the cancel flag
    /// before inserting into the textarea via `set_value`
    /// (`set_value` suppresses the Change observer, so the
    /// programmatic insert never trips our own cancel path).
    pub fn start_ai_generation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ai_state.is_generating() {
            return;
        }
        if self.staged_snapshot.is_empty() {
            return;
        }
        if self.in_flight.load(Ordering::Relaxed) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = cancel.clone();
        let staged = self.staged_snapshot.clone();
        // `spawn_in(window, …)` so the resulting task's
        // `update_in` call has live `&mut Window` — needed because
        // `InputState::set_value` requires it.
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SPINNER_PAINT_DELAY).await;
            if cancel_for_task.load(Ordering::SeqCst) {
                let _ = this.update(cx, |area, cx| {
                    area.ai_state = AiState::Idle;
                    cx.notify();
                });
                return;
            }
            let message = commit_message_heuristic::generate_heuristic(&staged);
            let _ = this.update_in(cx, |area, window, cx| {
                if cancel_for_task.load(Ordering::SeqCst) {
                    area.ai_state = AiState::Idle;
                    cx.notify();
                    return;
                }
                area.message_state
                    .update(cx, |s, cx| s.set_value(message.clone(), window, cx));
                // Mirror to disk explicitly — set_value suppresses
                // the Change observer that normally schedules the
                // debounced save (mirrors the auto-clear path in
                // `apply_result`).
                area.schedule_draft_save(message, cx);
                area.ai_state = AiState::Idle;
                cx.notify();
            });
        });
        self.ai_state = AiState::Generating {
            cancel,
            _task: task,
        };
        cx.notify();
    }
}
