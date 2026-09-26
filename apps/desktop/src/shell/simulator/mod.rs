//! iOS Simulator panel.
//!
//! The app-wide device registry service ([`hub`]), the right-sidebar panel
//! ([`panel`]) and its live screen ([`screen`]). See
//! `plans/260924-1433-ios-simulator-panel/`.

pub(crate) mod agent_ops;
pub(crate) mod auto_open;
mod annotate;
pub mod hub;
pub(crate) mod panel;
mod root_glue;
mod screen;
pub mod state;
pub mod widths;

pub use hub::{HubEvent, SimulatorHub, hub, install, on_quit};
pub use panel::SimulatorPanel;
pub use screen::register_screen_key_bindings;
pub(crate) use auto_open::note_chat_event;
pub(crate) use root_glue::{RootSimulator, simulator_actions};
