//! oximux-settings
//!
//! Theme tokens, density constants, and typography scale. Single source of
//! truth for the visual identity defined in `docs/design-guidelines.md`.
//!
//! Phase 0 ships dark-only. Light mode is a Phase 8+ decision.

pub mod agent_launch;
pub mod appearance;
pub mod auto_update;
pub mod autosave;
pub mod commit_message_ai;
pub mod computer_use;
pub mod custom_commands;
pub mod density;
pub mod dictation;
pub mod dictation_languages;
pub mod keybindings;
pub mod motion;
pub mod project_scripts;
pub mod terminal;
// Theme and typography carry gpui types (`Hsla`, `Font`) in their public
// structs, so unlike the settings modules — where only the `Global` impl is
// gated — the whole module goes with the feature.
#[cfg(feature = "gpui")]
pub mod theme;
#[cfg(feature = "gpui")]
pub mod typography;

pub use agent_launch::{
    ACP_PRESETS, AcpPreset, AgentLaunchSettings, OpenMode, PerAgentLaunch, Transport, acp_preset,
    import_resume_command, split_args,
};
pub use appearance::{Appearance, DensityPreset, UiScale};
pub use auto_update::AutoUpdateSettings;
pub use autosave::AutosaveSettings;
pub use commit_message_ai::{AgentSettings, CommitMessageAiMode, CommitMessageAiSettings};
pub use computer_use::{AppGrant, ComputerUseSettings};
pub use custom_commands::{CustomCommand, CustomCommandsFile, load_and_merge};
pub use density::Density;
pub use dictation::{DictationMode, DictationSettings, ModelUnloadTimeout};
pub use dictation_languages::{WHISPER_LANGUAGES, display_name as language_display_name};
pub use keybindings::KeybindingOverrides;
pub use motion::{Motion, ease_out_spring};
pub use project_scripts::{ProjectScripts, ScriptKind, load_for_project};
pub use terminal::{BellStyle, TerminalSettings};
#[cfg(feature = "gpui")]
pub use theme::{GitDecorations, SyntaxPalette, Theme};
#[cfg(feature = "gpui")]
pub use typography::Typography;
