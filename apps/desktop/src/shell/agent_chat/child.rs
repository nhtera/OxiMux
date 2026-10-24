//! A chat started by another agent: the child of a spawn.
//!
//! The same view as any chat; what differs is that its posture (reviewer,
//! verifier) is fixed from birth, applied to its first spawn and every respawn
//! after, and never offered to the mode picker.

use std::path::PathBuf;

use gpui::{Context, Window};
use oximux_agents::thread::launch_posture::SpawnPosture;
use oximux_agents::thread::{ChatBackend, ChatThread};
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
}
