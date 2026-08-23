//! Typography scale — sizes, weights, font stacks.
//!
//! Sizes are CSS pixels. Convert at the call site with `gpui::px(...)`.
//! Source of truth: `docs/design-guidelines.md`.

use gpui::{Font, FontFallbacks, FontFeatures, FontStyle, FontWeight, SharedString};
// The platform families live in `fonts` rather than here so the names stay
// reachable without gpui: this module carries gpui types and is feature-
// gated, but a family name is just a string, and `FontChoice` has to
// resolve one either way.
use crate::fonts::{FontChoice, platform as platform_fonts};

#[derive(Debug, Clone)]
pub struct Typography {
    // Sizes
    /// Sub-label text — metadata annotations that read subordinate to
    /// `t_label_xs`. Used for the tiny "vs main · 3 commits" sub-row
    /// under workspace cards in the left rail and similar secondary
    /// labels. Replaces hand-coded `t_body_sm * 0.85` arithmetic.
    pub t_sub_label: f32,
    pub t_label_xs: f32,
    pub t_label_caps: f32,
    pub t_body_sm: f32,
    /// The step between `t_body_sm` and `t_body_md`. For surfaces that want
    /// their body copy a notch above the cockpit default without ratcheting
    /// the whole scale — the Source Control panel, where file names and commit
    /// subjects scan from arm's length.
    ///
    /// Shares a value with `t_brand` and stays a separate field anyway: they
    /// are different concepts that happen to agree today, and a surface asking
    /// for "brand size" to get readable file rows would be a trap for whoever
    /// changes the wordmark next.
    pub t_body_base: f32,
    pub t_brand: f32,
    pub t_body_md: f32,
    pub t_body_lg: f32,
    /// Display heading — the one tier above body copy.
    ///
    /// A cockpit has no headings: every other surface in the app is a dense
    /// working view whose largest text is `t_body_lg`. Onboarding is the
    /// exception, because it is the one screen that is a landing page rather
    /// than a tool, and a welcome line set at body size reads as a form label.
    ///
    /// The gap to `t_body_lg` is deliberately wide. A near-miss above the body
    /// scale would look like a mistake; this is meant to be unmistakably a
    /// heading, and it is used in exactly one place.
    pub t_display: f32,

    // Weights
    pub w_regular: FontWeight,
    pub w_medium: FontWeight,
    pub w_semibold: FontWeight,

    // Font families. `family_mono` / `family_ui` are the *primary* family
    // name passed to GPUI's `font_family()`. GPUI does NOT parse CSS-style
    // comma fallback lists — it looks the string up verbatim. Fallbacks
    // are held separately in `mono_fallbacks` / `ui_fallbacks` and applied
    // via `.font(font_mono(...))` at the call site, which threads them
    // into GPUI's `FontFallbacks` resolution chain.
    pub family_mono: SharedString,
    pub family_ui: SharedString,
    pub mono_fallbacks: Vec<SharedString>,
    pub ui_fallbacks: Vec<SharedString>,
}

impl Typography {
    pub fn cockpit() -> Self {
        Self {
            t_sub_label: 9.5,
            t_label_xs: 10.0,
            t_label_caps: 10.5,
            t_body_sm: 11.0,
            t_body_base: 12.0,
            t_brand: 12.0,
            t_body_md: 13.0,
            t_body_lg: 14.0,
            t_display: 21.0,

            w_regular: FontWeight::NORMAL,
            w_medium: FontWeight::MEDIUM,
            w_semibold: FontWeight::SEMIBOLD,

            // Per-platform because the primary MUST resolve — see
            // `platform_fonts` for why a missing primary degrades the
            // terminal grid rather than just swapping a typeface. The
            // fallbacks are glyph-coverage backups only.
            family_mono: platform_fonts::MONO.into(),
            family_ui: platform_fonts::UI.into(),
            mono_fallbacks: platform_fonts::MONO_FALLBACKS
                .iter()
                .map(|name| (*name).into())
                .collect(),
            ui_fallbacks: platform_fonts::UI_FALLBACKS
                .iter()
                .map(|name| (*name).into())
                .collect(),
        }
    }

    /// The type *sizes* a given set of user choices resolves to.
    ///
    /// Only the zoom reaches here. A density preset deliberately does not
    /// touch type — it changes the space around the text, not the text — which
    /// is what keeps the two controls from being two names for one knob. See
    /// [`crate::appearance`].
    ///
    /// This is half an answer, and the wrong one to reach for outside this
    /// crate: it leaves both faces at the platform default, so a surface built
    /// from it ignores a chosen font while everything around it obeys. Use
    /// [`crate::appearance::typography`], which applies both. `xtask
    /// appearance-lint` fails on a call to this one from anywhere else.
    pub fn for_appearance(appearance: crate::appearance::Appearance) -> Self {
        Self::cockpit().scaled(appearance.scale.factor())
    }

    /// Every size multiplied by `factor`. Families, weights and fallbacks are
    /// untouched — zoom changes how big the type is, not which type it is.
    pub fn scaled(&self, factor: f32) -> Self {
        if factor == 1.0 {
            return self.clone();
        }
        Self {
            t_sub_label: self.t_sub_label * factor,
            t_label_xs: self.t_label_xs * factor,
            t_label_caps: self.t_label_caps * factor,
            t_body_sm: self.t_body_sm * factor,
            t_body_base: self.t_body_base * factor,
            t_brand: self.t_brand * factor,
            t_body_md: self.t_body_md * factor,
            t_body_lg: self.t_body_lg * factor,
            t_display: self.t_display * factor,
            ..self.clone()
        }
    }

    /// The same scale, drawn in the faces the user asked for.
    ///
    /// A `None` on either side keeps the platform face. An override pushes the
    /// family it replaced onto the front of that side's fallback list: the
    /// chain only covers glyphs *missing from a family that loaded*, and a
    /// hand-picked face is far likelier than the platform primary to be short
    /// a box-drawing or block-element glyph the terminal grid needs.
    ///
    /// The names are taken on trust. Whether the machine actually has a family
    /// is a question only the text system can answer, and guessing at it here
    /// would be worse than not asking: gpui resolves `Font::family` verbatim,
    /// so an absent primary lands on a sentinel face nobody chose, with
    /// nothing to say why. Validation happens where the choice is made — see
    /// the desktop's `font_settings`.
    pub fn with_fonts(mut self, choice: &FontChoice) -> Self {
        if let Some(ui) = &choice.ui
            && ui.as_str() != self.family_ui.as_ref()
        {
            let replaced = std::mem::replace(&mut self.family_ui, ui.clone().into());
            self.ui_fallbacks.insert(0, replaced);
        }
        if let Some(mono) = &choice.mono
            && mono.as_str() != self.family_mono.as_ref()
        {
            let replaced = std::mem::replace(&mut self.family_mono, mono.clone().into());
            self.mono_fallbacks.insert(0, replaced);
        }
        self
    }

    /// Build a GPUI `Font` for terminal/editor surfaces with the configured
    /// fallback chain. Call sites use `.font(typography.mono_font())` rather
    /// than `.font_family(...)` so glyphs the primary lacks cascade through
    /// `mono_fallbacks` instead of painting as tofu.
    pub fn mono_font(&self) -> Font {
        Font {
            family: self.family_mono.clone(),
            // Ligatures off: terminals must show literal byte sequences.
            // `->` should be two characters, not `→`; `==` two equals, not
            // `⩵`. This stopped being hypothetical when Lilex joined the
            // chain — it ligates by default, and a ligature spanning two
            // character cells cannot survive `shape_line`'s per-glyph
            // `force_width`, which is what keeps glyph cells 1:1 with
            // character cells.
            features: FontFeatures::disable_ligatures(),
            fallbacks: Some(FontFallbacks::from_fonts(
                self.mono_fallbacks.iter().map(|s| s.to_string()).collect(),
            )),
            weight: self.w_regular,
            style: FontStyle::Normal,
        }
    }

    /// UI-chrome counterpart of `mono_font` for sidebar, top bar, etc.
    pub fn ui_font(&self) -> Font {
        Font {
            family: self.family_ui.clone(),
            features: FontFeatures::default(),
            fallbacks: Some(FontFallbacks::from_fonts(
                self.ui_fallbacks.iter().map(|s| s.to_string()).collect(),
            )),
            weight: self.w_regular,
            style: FontStyle::Normal,
        }
    }
}

impl Default for Typography {
    fn default() -> Self {
        Self::cockpit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::{Appearance, DensityPreset, ThemeChoice, UiScale};

    #[test]
    fn the_default_appearance_leaves_the_type_scale_alone() {
        let (base, resolved) = (Typography::cockpit(), Typography::for_appearance(
            Appearance::default(),
        ));
        assert_eq!(resolved.t_body_sm, base.t_body_sm);
        assert_eq!(resolved.t_body_lg, base.t_body_lg);
        assert_eq!(resolved.t_sub_label, base.t_sub_label);
    }

    #[test]
    fn zoom_moves_every_size_by_the_same_factor() {
        // Proportional, not per-field: a scale that drifted between sizes
        // would break the hierarchy the scale exists to encode.
        let base = Typography::cockpit();
        let big = base.scaled(1.5);
        for (small, large) in [
            (base.t_sub_label, big.t_sub_label),
            (base.t_label_xs, big.t_label_xs),
            (base.t_label_caps, big.t_label_caps),
            (base.t_body_sm, big.t_body_sm),
            (base.t_body_base, big.t_body_base),
            (base.t_brand, big.t_brand),
            (base.t_body_md, big.t_body_md),
            (base.t_body_lg, big.t_body_lg),
        ] {
            assert!((large - small * 1.5).abs() < f32::EPSILON, "{small} → {large}");
        }
    }

    #[test]
    fn the_density_preset_does_not_touch_type() {
        // The two controls have to stay distinguishable: if picking
        // Comfortable also grew the text, the settings pane would be offering
        // the zoom twice under different names.
        let roomy = Typography::for_appearance(Appearance {
            theme: ThemeChoice::default(),
            density: DensityPreset::Comfortable,
            scale: UiScale::default(),
        });
        assert_eq!(roomy.t_body_sm, Typography::cockpit().t_body_sm);
    }

    #[test]
    fn zoom_changes_the_size_and_not_the_typeface() {
        let base = Typography::cockpit();
        let big = base.scaled(1.4);
        assert_eq!(big.family_mono, base.family_mono);
        assert_eq!(big.family_ui, base.family_ui);
        assert_eq!(big.mono_fallbacks, base.mono_fallbacks);
        assert_eq!(big.w_semibold, base.w_semibold);
    }

    #[test]
    fn scaling_by_one_changes_nothing() {
        let base = Typography::cockpit();
        let same = base.scaled(1.0);
        assert_eq!(same.t_body_sm, base.t_body_sm);
        assert_eq!(same.t_body_lg, base.t_body_lg);
    }

    #[test]
    fn an_unset_choice_leaves_the_platform_faces_alone() {
        let base = Typography::cockpit();
        let same = base.clone().with_fonts(&FontChoice::default());
        assert_eq!(same.family_ui, base.family_ui);
        assert_eq!(same.family_mono, base.family_mono);
        assert_eq!(
            same.mono_fallbacks, base.mono_fallbacks,
            "nothing to demote, so nothing added to the chain"
        );
    }

    #[test]
    fn a_chosen_face_takes_the_primary_and_demotes_the_platform_one() {
        // The demotion is the point: a person picking a display-ish mono face
        // should still get box drawing from the family that definitely has it,
        // rather than tofu in the one place tofu is least acceptable.
        let base = Typography::cockpit();
        let picked = base.clone().with_fonts(&FontChoice {
            ui: None,
            mono: Some("Cascadia Code".into()),
        });
        assert_eq!(picked.family_mono.as_ref(), "Cascadia Code");
        assert_eq!(picked.mono_fallbacks[0], base.family_mono);
        assert_eq!(
            picked.family_ui, base.family_ui,
            "the side that was not chosen is untouched"
        );
        assert_eq!(picked.ui_fallbacks, base.ui_fallbacks);
    }

    #[test]
    fn choosing_the_face_already_in_use_does_not_grow_the_fallback_chain() {
        // Naming the platform family explicitly is a legitimate thing to do
        // from the picker, and it must not push that same family onto its own
        // fallback list — once per launch would be harmless, but this runs on
        // every appearance change.
        let base = Typography::cockpit();
        let same = base.clone().with_fonts(&FontChoice {
            ui: Some(base.family_ui.to_string()),
            mono: Some(base.family_mono.to_string()),
        });
        assert_eq!(same.ui_fallbacks, base.ui_fallbacks);
        assert_eq!(same.mono_fallbacks, base.mono_fallbacks);
    }

    #[test]
    fn a_chosen_face_survives_the_zoom() {
        // `scaled` rebuilds the size fields and clones the rest; a face chosen
        // before a zoom must not be dropped by it.
        let picked = Typography::cockpit().with_fonts(&FontChoice {
            ui: Some("Inter".into()),
            mono: None,
        });
        let big = picked.scaled(1.4);
        assert_eq!(big.family_ui.as_ref(), "Inter");
        assert_eq!(big.ui_fallbacks, picked.ui_fallbacks);
    }
}
