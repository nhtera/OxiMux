//! iOS Simulator panel.
//!
//! P4 lands the app-wide device registry service ([`hub`]); the panel UI
//! arrives in P5. See `plans/260924-1433-ios-simulator-panel/`.

pub mod hub;
pub(crate) mod panel;
mod root_glue;
pub mod state;
pub mod widths;

pub use hub::{HubEvent, SimulatorHub, hub, install, on_quit};
pub use panel::SimulatorPanel;
pub(crate) use root_glue::RootSimulator;
