//! Main area — center pane shown when no `MainPane` entity is mounted.
//!
//! Delegates to `welcome_view::view` so the empty state is a proper card
//! (logo, brand, action stubs, keyboard hints) instead of a placeholder
//! string. Phase 04 will extend the predicate to gate on workspace
//! selection state.

use gpui::IntoElement;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::welcome_view;

pub fn view(theme: Theme, density: Density, typography: &Typography) -> impl IntoElement {
    welcome_view::view(theme, density, typography)
}
