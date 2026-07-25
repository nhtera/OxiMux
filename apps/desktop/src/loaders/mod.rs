//! Loaders — pull external config/assets into the app at startup.
//!
//! `custom_commands_loader` + `project_scripts_loader` read user/project
//! command definitions, `browser_profiles` enumerates Chromium/Firefox
//! profiles, and `file_http_client` serves local files to the inline webview.
//! Grouped for traversal; re-exported at the crate root to preserve paths.

pub mod browser_profiles;
pub mod custom_commands_loader;
pub mod file_http_client;
pub mod project_scripts_loader;
