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

    /// A reviewer or verifier whose agent announced, at start, a mode or a tool
    /// its posture does not allow is stopped before it acts: the launch flags
    /// were verified on one CLI build, and a later one could ignore one. The
    /// chat says why; nothing downgrades silently.
    pub(super) fn guard_spawn_posture(&mut self, ev: &ThreadEvent) {
        let ThreadEvent::SessionInit { permission_mode, meta, .. } = ev else { return };
        if self.spawn_posture == SpawnPosture::Default || self.backend.transport != Transport::StreamJson {
            return;
        }
        if let Err(why) = claude_init_holds(self.spawn_posture, permission_mode, &meta.tools) {
            tracing::warn!(%why, "posture not held; stopping the child");
            self.thread.last_error = Some(format!(
                "Stopped: this {} agent could not be held to its posture ({why}).",
                self.spawn_posture.as_text()
            ));
            if let Some(conn) = self.connection.take() {
                conn.shutdown();
            }
        }
    }
}
