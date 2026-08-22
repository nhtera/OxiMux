//! OxiMux app library surface.
//!
//! Module declarations live here (rather than `main.rs`) so integration
//! tests under `tests/` can `use oximux_app::shell::*;`. The `oximux`
//! binary at `src/main.rs` imports from this library.

pub mod actions;
pub mod app_paths;
pub mod assets;
pub mod keymap_registry;
pub mod left_rail_layout;
pub mod notifier;
pub mod project_panes_factory;
pub mod remote_control;
pub mod scheduler;
pub mod shell;
pub mod state;
// Staging and swapping a `.app` bundle, verified against a codesign pin.
// Windows gets a versioned-directory updater of its own later.
#[cfg(target_os = "macos")]
pub mod updater;
pub mod workspace_root;

// Shared widget layer extracted to the `oximux-ui` crate. Re-exported under the
// historical `crate::ui` path so every `crate::ui::…` call site resolves
// unchanged (host depends on ui; ui never depends on the host).
pub use oximux_ui as ui;

// --- Grouped module folders (Tier-1 reorg) ---------------------------------
// Files are foldered one level deep for traversal; each submodule is
// re-exported below so existing `crate::<name>::…` / `oximux_app::<name>::…`
// call sites keep resolving unchanged.
pub mod agent_glue;
pub mod app_settings;
pub mod loaders;
pub mod platform;
pub mod session_restore;

#[doc(inline)]
pub use agent_glue::{
    agent_awake, agent_hook_dialects, agent_hooks_global, agent_status_hooks, pi_status_extension,
};
#[doc(inline)]
pub use shell::agent_chat::clear_stale_screen_control_grants;
#[doc(inline)]
pub use app_settings::{
    agent_launch_settings, appearance_settings, auto_update_settings, commit_message_ai_settings,
    computer_use_settings, dictation_settings, font_settings, keybindings_settings, motion_settings,
    port_label_settings, scm_layout_settings, terminal_settings,
};
#[doc(inline)]
pub use loaders::{
    browser_profiles, custom_commands_loader, file_http_client, project_scripts_loader,
};
#[doc(inline)]
pub use platform::{
    app_nap, menu, mic_permission, single_instance, window_factory, window_registry,
};
#[doc(inline)]
pub use session_restore::{
    catalog_cache, git_state_cache, persisted_chat, persisted_terminals, relay_cold_restore,
    relay_supervisor,
};
pub(crate) use session_restore::restore_fallback;
