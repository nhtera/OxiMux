//! A chat started by another agent: the child of a spawn.
//!
//! The same view as any chat; what differs is that its posture (reviewer,
//! verifier) is fixed from birth, applied to its first spawn and every respawn
//! after, and never offered to the mode picker.

use std::path::PathBuf;

use gpui::{Context, Window};
use oximux_agents::thread::launch_posture::{SpawnPosture, claude_init_holds};
use oximux_agents::thread::{ChatBackend, ChatThread, ThreadEvent, Transport};
use oximux_settings::{Density, Theme, Typography};

use super::session_persistence::ConnectMode;
use super::{AgentChatView, RestoredPosture};

impl AgentChatView {
    /// Open a spawned child's chat, connecting at once under `posture`.
    #[allow(clippy::too_many_arguments)]
    pub fn new_child(
        cwd: PathBuf,
        model: Option<String>,
        backend: ChatBackend,
        posture: SpawnPosture,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::assemble(
            cwd,
            model,
            backend,
            ChatThread::new(),
            ConnectMode::Connect,
            RestoredPosture { spawn: posture, ..Default::default() },
            theme,
            density,
            typography,
            window,
            cx,
        )
    }

    /// A reviewer or verifier whose agent announces (in its per-turn init) a
    /// mode or a tool its posture does not allow is stopped as soon as the view
    /// sees that init: the launch flags were verified on one CLI build, and a
    /// later one could ignore one. The first init follows the first prompt, so
    /// a tool call already in flight may have started; nothing that turn asks
    /// for after the stop runs, and no respawn revives the agent. The chat says
    /// why; nothing downgrades silently.
    pub(super) fn guard_spawn_posture(&mut self, ev: &ThreadEvent) {
        let ThreadEvent::SessionInit { permission_mode, meta, .. } = ev else { return };
        if self.spawn_posture == SpawnPosture::Default || self.backend.transport != Transport::StreamJson {
            return;
        }
        if let Err(why) = claude_init_holds(self.spawn_posture, permission_mode, &meta.tools) {
            tracing::warn!(%why, "posture not held; stopping the child");
            self.posture_refused = Some(format!(
                "Stopped: this {} agent could not be held to its posture ({why}).",
                self.spawn_posture.as_text()
            ));
            // Not a user Stop: the exit that follows must leave the chat down,
            // with this reason, rather than resumable-idle.
            self.interrupted = false;
            self.thread.last_error = self.posture_refused.clone();
            if let Some(conn) = self.connection.take() {
                conn.shutdown();
            }
        }
    }

    /// Whether a respawn must not happen because the agent was stopped for its
    /// posture: the same CLI would ignore the same flags again, and a revived
    /// process would take the next prompt unconfined. Every respawn path (a
    /// send after Stop, a model switch, a remote open, Retry) goes through
    /// `respawn_with_env`, which asks this first.
    pub(super) fn respawn_refused_for_posture(&mut self) -> bool {
        let Some(why) = self.posture_refused.clone() else { return false };
        self.disconnected = true;
        self.interrupted = false;
        self.thread.last_error = Some(why);
        true
    }
}
