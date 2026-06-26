//! `oximux-ui` — the shared, app-agnostic widget layer.
//!
//! Anything several shell modules would otherwise re-implement (button variant
//! wrappers, surface-chrome recipes like the floating overlay, generic dialogs)
//! lives here so the design system stays single-sourced. Widgets take their
//! data via params and emit their own local events; this crate depends only
//! downward (`gpui`, `gpui-component`, `oximux-settings` for tokens) and never
//! on `oximux-app`. The host crate re-exports it as `crate::ui`.

pub mod buttons;
pub mod overlay;

pub use buttons::danger_ghost;
pub use overlay::FloatingSurface;
