//! Typography scale — sizes, weights, font stacks.
//!
//! Sizes are CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

use gpui::{FontWeight, SharedString};

#[derive(Debug, Clone)]
pub struct Typography {
    // Sizes
    pub t_label_xs: f32,
    pub t_label_caps: f32,
    pub t_body_sm: f32,
    pub t_brand: f32,
    pub t_body_md: f32,
    pub t_body_lg: f32,

    // Weights
    pub w_regular: FontWeight,
    pub w_medium: FontWeight,
    pub w_semibold: FontWeight,

    // Font families. Caller picks one per surface; font-kit resolves the
    // first match in the stack. Strings are space-separated CSS fallbacks.
    pub family_mono: SharedString,
    pub family_ui: SharedString,
}

impl Typography {
    pub fn cockpit() -> Self {
        Self {
            t_label_xs: 10.0,
            t_label_caps: 10.5,
            t_body_sm: 11.0,
            t_brand: 12.0,
            t_body_md: 13.0,
            t_body_lg: 14.0,

            w_regular: FontWeight::NORMAL,
            w_medium: FontWeight::MEDIUM,
            w_semibold: FontWeight::SEMIBOLD,

            family_mono: "Geist Mono, SF Mono, Menlo, monospace".into(),
            family_ui: "Inter, SF Pro Text, system-ui".into(),
        }
    }
}

impl Default for Typography {
    fn default() -> Self {
        Self::cockpit()
    }
}
