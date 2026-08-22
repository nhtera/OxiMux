//! App-level settings modules.
//!
//! These are the *host* crate's own settings surfaces (terminal, motion, SCM
//! layout, keybindings, commit-message AI, agent launch defaults) — distinct
//! from the workspace-wide `oximux-settings` crate, which owns the on-disk
//! settings store. Grouped here purely for traversal; each submodule is
//! re-exported at the crate root so existing `crate::terminal_settings::…`
//! paths keep resolving.

pub mod agent_launch_settings;
pub mod appearance_settings;
pub mod commit_message_ai_settings;
pub mod auto_update_settings;
pub mod computer_use_settings;
pub mod dictation_settings;
pub mod keybindings_settings;
pub mod motion_settings;
pub mod port_label_settings;
pub mod scm_layout_settings;
pub mod terminal_settings;
