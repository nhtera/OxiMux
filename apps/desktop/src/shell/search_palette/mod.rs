//! Search palette — the left rail's Search row and ⌘J.
//!
//! Empty query lists recent tabs and recent worktrees; typing searches open
//! tabs, worktrees, projects, settings panes and actions. Pure data + ranking
//! live in `model` / `sources` / `recency` / `rank` / `sections`; the GPUI
//! view is separate so the logic stays unit-testable.

pub mod filter_popover;
pub mod keys;
pub mod model;
pub mod rank;
pub mod recency;
pub mod render_chrome;
pub mod render_rows;
pub mod sections;
pub mod settings_items;
pub mod sources;
pub mod state;
pub mod view;

pub use view::{SearchPalette, SearchPaletteEvent};

#[cfg(test)]
mod rank_tests;
#[cfg(test)]
mod sections_tests;
