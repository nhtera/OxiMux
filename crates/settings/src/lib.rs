//! oximux-settings
//!
//! Theme tokens, density constants, and typography scale. Single source of
//! truth for the visual identity defined in `docs/design-guidelines.md`.
//!
//! Phase 0 ships dark-only. Light mode is a Phase 8+ decision.

pub mod density;
pub mod terminal;
pub mod theme;
pub mod typography;

pub use density::Density;
pub use terminal::{BellStyle, TerminalSettings};
pub use theme::{GitDecorations, Theme};
pub use typography::Typography;
