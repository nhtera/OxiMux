//! Cross-cutting UI helpers that don't belong to any single shell module.
//!
//! Anything that several shell modules would otherwise re-implement (button
//! variant wrappers, surface chrome recipes, list-row helpers) lives here so
//! the design system stays single-sourced.

pub mod buttons;
pub mod spinner;

pub use buttons::danger_ghost;
pub use spinner::{spinning_loader, spinning_loader_sized};
