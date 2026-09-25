//! iOS Simulator panel.
//!
//! P4 lands the app-wide device registry service ([`hub`]); the panel UI
//! arrives in P5. See `plans/260924-1433-ios-simulator-panel/`.

pub mod hub;

pub use hub::{HubEvent, SimulatorHub, hub, install, on_quit};
