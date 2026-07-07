//! oximux-settings
//!
//! Theme tokens, density constants, and typography scale. Single source of
//! truth for the visual identity defined in `docs/design-guidelines.md`.
//!
//! Phase 0 ships dark-only. Light mode is a Phase 8+ decision.

pub mod agent_launch;
pub mod autosave;
pub mod commit_message_ai;
pub mod custom_commands;
pub mod density;
pub mod keybindings;
pub mod motion;
pub mod project_scripts;
pub mod terminal;
pub mod theme;
pub mod typography;

pub use agent_launch::{AgentLaunchSettings, OpenMode, PerAgentLaunch, Transport, split_args};
pub use autosave::AutosaveSettings;
pub use commit_message_ai::{AgentSettings, CommitMessageAiMode, CommitMessageAiSettings};
pub use custom_commands::{CustomCommand, CustomCommandsFile, load_and_merge};
pub use density::Density;
pub use keybindings::KeybindingOverrides;
pub use motion::{Motion, ease_out_spring};
pub use project_scripts::{ProjectScripts, ScriptKind};
pub use terminal::{BellStyle, TerminalSettings};
pub use theme::{GitDecorations, Theme};
pub use typography::Typography;
