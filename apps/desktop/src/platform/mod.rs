//! Platform glue — macOS/OS-level integration and window lifecycle.
//!
//! `app_nap` (App-Nap suppression), `single_instance` (flock guard),
//! `window_factory` + `window_registry` (window creation/tracking), and
//! `menu` (native menu bar). Grouped for traversal; re-exported at the crate
//! root so existing `crate::app_nap::…` paths keep resolving.

pub mod app_nap;
pub mod escape_tap;
pub mod menu;
pub mod mic_permission;
pub mod screen_control_indicator;
pub mod secure_input;
pub mod single_instance;
pub mod window_factory;
pub mod window_registry;
