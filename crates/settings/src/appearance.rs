//! The appearance choices a user actually gets: which palette the cockpit is
//! painted in, how much air it leaves around its text, how big the whole thing
//! is drawn, and which two typefaces it is drawn with.
//!
//! They all resolve into [`Theme`](crate::Theme), [`Density`](crate::Density)
//! and [`Typography`](crate::Typography) — they do not add a token set of their
//! own. A render path never asks "what did the user pick", it reads the tokens
//! it already read, and they come out different.
//!
//! The first three live in [`Appearance`], which is `Copy` and doubles as the
//! staleness stamp the pull compares. The faces cannot: see
//! [`fonts`](crate::fonts) for where they live and why.
//!
//! # Why density and zoom are two controls and not one
//!
//! They are easy to confuse and the difference is the whole point:
//!
//! * [`UiScale`] multiplies *everything* — type and chrome together. It is the
//!   answer to "this display is too dense for my eyes", and it changes how much
//!   fits on screen only as a side effect of making the text bigger.
//! * [`DensityPreset`] leaves the type exactly where it is and changes only the
//!   space around it. It is the answer to "I can read this fine, I just want
//!   more rows" — or the opposite.
//!
//! Collapsing them into one slider would give the settings pane two names for
//! the same knob, which is worse than having no knob.
//!
//! # The one token neither of them moves
//!
//! `h_top_bar` is pinned. Its height is chosen so the chrome row's vertical
//! centre lands on the macOS traffic-light glyphs, whose position is fixed at
//! window creation and cannot follow a live preference. Scaling it would slide
//! the row off the buttons it is aligned to. See `Density::cockpit`.

use serde::{Deserialize, Serialize};

/// How much air the cockpit leaves around text of a fixed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityPreset {
    /// The original — tight, maximum rows on screen.
    #[default]
    Cockpit,
    /// Roomier: the same type with more space around it. Every value is
    /// `1.25×` the cockpit one, rounded to an even pixel — see
    /// `Density::comfortable`.
    Comfortable,
}

impl DensityPreset {
    /// Every preset, in the order a picker should offer them (tightest first).
    pub const ALL: [Self; 2] = [Self::Cockpit, Self::Comfortable];

    /// Name for a settings control.
    pub fn label(self) -> &'static str {
        match self {
            Self::Cockpit => "Cockpit",
            Self::Comfortable => "Comfortable",
        }
    }

    /// One line saying what picking it does, for the row under the control.
    pub fn description(self) -> &'static str {
        match self {
            Self::Cockpit => "Tight rows. Fits the most on screen.",
            Self::Comfortable => "The same text with more room around it.",
        }
    }
}

/// Which palette the cockpit is painted in.
///
/// Named for materials rather than for "dark" and "light" because that is how
/// the dark one was already named, and because the pair says something the
/// polarity does not: charcoal on paper is one drawing in two media, which is
/// the relationship the two palettes are meant to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// The original, and still the default.
    #[default]
    Charcoal,
    /// The light counterpart.
    Paper,
}

impl ThemeChoice {
    /// Every palette, in the order a picker should offer them.
    pub const ALL: [Self; 2] = [Self::Charcoal, Self::Paper];

    /// Name for a settings control.
    pub fn label(self) -> &'static str {
        match self {
            Self::Charcoal => "Charcoal",
            Self::Paper => "Paper",
        }
    }

    /// The polarity, spelled out — a picker showing only two material names
    /// makes the reader guess which one is the dark one.
    pub fn polarity(self) -> &'static str {
        match self {
            Self::Charcoal => "dark",
            Self::Paper => "light",
        }
    }

    /// True when the palette paints dark text on a light ground.
    ///
    /// The question every polarity-aware token has to ask: an overlay that is
    /// white at 6% reads as a highlight on charcoal and as nothing at all on
    /// paper, where the same job wants black at 5%.
    pub fn is_light(self) -> bool {
        matches!(self, Self::Paper)
    }
}

/// Smallest and largest whole-UI zoom, in percent.
///
/// The floor is 80 rather than something smaller because `t_sub_label` is
/// already 9.5px and the cockpit's hairlines stop separating below that. The
/// ceiling is 160 because the left rail's 500px maximum stops being enough
/// room for a workspace card past it.
pub const MIN_SCALE_PERCENT: u16 = 80;
/// See [`MIN_SCALE_PERCENT`].
pub const MAX_SCALE_PERCENT: u16 = 160;
/// One press of zoom in / zoom out.
pub const SCALE_STEP_PERCENT: u16 = 10;

/// Whole-UI zoom, as a percentage of the size the cockpit was designed at.
///
/// Held as a percent rather than a float because it is a user-facing number:
/// it is written into a settings file a person may edit, and shown in the UI
/// as `110%`. Values are clamped and snapped to [`SCALE_STEP_PERCENT`] on the
/// way in, so a hand-edited `107` behaves like a real step rather than
/// stranding zoom-out one press away from the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UiScale(u16);

impl Default for UiScale {
    fn default() -> Self {
        Self(100)
    }
}

impl UiScale {
    /// Clamp to the supported range and snap to the nearest step.
    pub fn from_percent(percent: u16) -> Self {
        let clamped = percent.clamp(MIN_SCALE_PERCENT, MAX_SCALE_PERCENT);
        let step = SCALE_STEP_PERCENT;
        // Nearest multiple of `step`, half away from zero. The clamp above
        // keeps the result inside the range because both bounds are multiples.
        let snapped = ((clamped + step / 2) / step) * step;
        Self(snapped)
    }

    /// The percentage, for display and for the settings file.
    pub fn percent(self) -> u16 {
        self.0
    }

    /// The multiplier the token scales apply.
    pub fn factor(self) -> f32 {
        f32::from(self.0) / 100.0
    }

    /// True at the designed size, where scaling is a no-op.
    pub fn is_default(self) -> bool {
        self == Self::default()
    }

    /// One step larger, stopping at [`MAX_SCALE_PERCENT`].
    pub fn zoomed_in(self) -> Self {
        Self::from_percent(self.0.saturating_add(SCALE_STEP_PERCENT))
    }

    /// One step smaller, stopping at [`MIN_SCALE_PERCENT`].
    pub fn zoomed_out(self) -> Self {
        Self::from_percent(self.0.saturating_sub(SCALE_STEP_PERCENT))
    }

    /// `"100%"` — what a status readout or a menu item shows.
    pub fn label(self) -> String {
        format!("{}%", self.0)
    }
}

/// The user's appearance choices, together.
///
/// Small and `Copy` so a resolved [`Density`](crate::Density) can carry the
/// choices it was built from — that is what lets a view holding a stale token
/// snapshot notice, in one comparison, that it needs a fresh one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Which palette. See [`ThemeChoice`].
    pub theme: ThemeChoice,
    /// How much air. See [`DensityPreset`].
    pub density: DensityPreset,
    /// How big. See [`UiScale`].
    pub scale: UiScale,
}

impl Appearance {
    /// Name of the settings file in the app data dir.
    pub const FILE_NAME: &'static str = "appearance.toml";

    /// Parse a settings file. Unknown keys are ignored for forward-compat and
    /// missing ones keep their defaults, matching every other settings file.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Re-clamp anything a hand edit could have put out of range.
    pub fn sanitized(mut self) -> Self {
        self.scale = UiScale::from_percent(self.scale.percent());
        self
    }
}

/// Serialize both halves of the appearance settings into one document.
///
/// `appearance.toml` has two readers — this struct and
/// [`FontChoice`](crate::fonts::FontChoice) — because the font names cannot
/// live in a `Copy` stamp; see the [`fonts`](crate::fonts) module docs. Reading
/// splits cleanly, since each type ignores the keys it does not own. Writing
/// does not, so this is the one function that knows the whole file.
///
/// Concatenation is sound because both halves serialize to flat scalars: TOML
/// requires every key/value pair to precede any table header, and neither
/// struct emits one. `a_written_file_round_trips_through_both_readers` is what
/// keeps that true if either side ever gains a nested field.
pub fn to_toml_string(appearance: &Appearance, fonts: &crate::fonts::FontChoice) -> String {
    let mut doc = toml::to_string_pretty(appearance).unwrap_or_default();
    doc.push_str(&toml::to_string_pretty(fonts).unwrap_or_default());
    doc
}

#[cfg(feature = "gpui")]
impl gpui::Global for Appearance {}

/// The appearance in force, or the shipped default if none was installed
/// (headless tests, and the window or two before startup finishes).
#[cfg(feature = "gpui")]
pub fn active(cx: &gpui::App) -> Appearance {
    cx.try_global::<Appearance>().copied().unwrap_or_default()
}

/// Bring a view's cached tokens up to date with the current appearance.
///
/// # Why views cache tokens at all, and why this exists
///
/// Tokens are threaded down the view tree as plain values: a view is handed a
/// `Density` and a `Typography` when it is built and keeps them. That is a
/// deliberate choice — it keeps every render path total and testable, with no
/// ambient state to stub — but it means a live preference change leaves ~50
/// snapshots stale, scattered across views that nothing can enumerate.
///
/// Pushing new values down to all of them would need a setter on each view and
/// a fan-out that has to be extended by hand every time a view is added; the
/// first one anybody forgets renders at the old size forever, and nothing
/// fails. So the refresh is a *pull* instead, from the one place every view
/// already has: the top of its `render`. Call it there and a view cannot be
/// stale for longer than a frame.
///
/// The [`Density::appearance`](crate::Density::appearance) stamp makes this
/// nearly free — the common case is one `Appearance` comparison per view per
/// frame, and nothing is rebuilt or allocated until the user actually changes
/// something.
#[cfg(feature = "gpui")]
pub fn sync(
    theme: &mut crate::Theme,
    density: &mut crate::Density,
    typography: &mut crate::Typography,
    cx: &gpui::App,
) {
    let current = active(cx);
    let fonts = crate::fonts::active(cx);
    if density.appearance == current && faces_are_current(typography, fonts) {
        return;
    }
    *theme = crate::Theme::for_appearance(current);
    *density = crate::Density::for_appearance(current);
    *typography = crate::Typography::for_appearance(current).with_fonts(fonts);
}

/// True when `typography` is already drawn in the faces `fonts` asks for.
///
/// A second comparison is needed because the face choice is not in the
/// `Appearance` stamp — see [`fonts`](crate::fonts). It compares the *resolved*
/// names rather than the `Option`s, so clearing a choice back to the platform
/// face is a change like any other; an `is_some` test would leave every view
/// painted in the typeface the user had just removed.
#[cfg(feature = "gpui")]
fn faces_are_current(typography: &crate::Typography, fonts: &crate::fonts::FontChoice) -> bool {
    typography.family_ui.as_ref() == fonts.resolved_ui()
        && typography.family_mono.as_ref() == fonts.resolved_mono()
}

/// The type scale in force: the current sizes, in the current faces.
///
/// The one correct way to build a `Typography` from scratch outside a view's
/// cached snapshot. [`Typography::for_appearance`](crate::Typography::for_appearance)
/// alone answers half the question — it sizes the scale and leaves the faces at
/// the platform default — so a caller that stops there paints its surface in a
/// typeface the user replaced, while everything around it obeys. That failure
/// is invisible until someone picks a font, which is why `xtask
/// appearance-lint` fails on a call to the half-answer outside this crate.
#[cfg(feature = "gpui")]
pub fn typography(cx: &gpui::App) -> crate::Typography {
    crate::Typography::for_appearance(active(cx)).with_fonts(crate::fonts::active(cx))
}

/// The spacing scale in force.
///
/// The companion to [`typography`], for a surface that resolves its tokens per
/// render rather than caching them. Unlike the type scale this needs no second
/// half — a density is a set of numbers and nothing outside `Appearance` can
/// change it — so [`Density::for_appearance`](crate::Density::for_appearance)
/// is a complete answer and stays callable anywhere. This exists so the two
/// read alike at a call site that wants both.
#[cfg(feature = "gpui")]
pub fn density(cx: &gpui::App) -> crate::Density {
    crate::Density::for_appearance(active(cx))
}

/// [`sync`] for a view that keeps a type scale but no density, and so has no
/// stamp to compare. One size stands in for the whole scale, which is sound
/// because the zoom moves all of them by the same factor.
#[cfg(feature = "gpui")]
pub fn sync_typography(typography: &mut crate::Typography, cx: &gpui::App) {
    let current = active(cx);
    let fonts = crate::fonts::active(cx);
    let want = crate::Typography::cockpit().t_body_sm * current.scale.factor();
    if (typography.t_body_sm - want).abs() < f32::EPSILON && faces_are_current(typography, fonts) {
        return;
    }
    *typography = crate::Typography::for_appearance(current).with_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_appearance_is_the_shipped_cockpit() {
        // Every existing screenshot, spacing decision and test in the tree was
        // taken at this pair. A change here is a change to what the app looks
        // like on first launch, which is not something to do by accident.
        let a = Appearance::default();
        assert_eq!(a.density, DensityPreset::Cockpit);
        assert_eq!(a.scale.percent(), 100);
        assert!(a.scale.is_default());
        assert_eq!(a.scale.factor(), 1.0);
    }

    #[test]
    fn zoom_stops_at_the_ends_rather_than_wrapping() {
        let mut s = UiScale::default();
        for _ in 0..20 {
            s = s.zoomed_in();
        }
        assert_eq!(s.percent(), MAX_SCALE_PERCENT);
        for _ in 0..40 {
            s = s.zoomed_out();
        }
        assert_eq!(s.percent(), MIN_SCALE_PERCENT);
        // And the floor is a real floor, not a saturating-subtract artefact:
        // one more press must not underflow into a huge percentage.
        assert_eq!(s.zoomed_out().percent(), MIN_SCALE_PERCENT);
    }

    #[test]
    fn an_off_grid_percentage_snaps_onto_the_step() {
        // The trap this closes: a file saying 107 would otherwise leave zoom-out
        // landing on 97, and the user never gets back to a round number.
        assert_eq!(UiScale::from_percent(107).percent(), 110);
        assert_eq!(UiScale::from_percent(104).percent(), 100);
        assert_eq!(UiScale::from_percent(105).percent(), 110);
    }

    #[test]
    fn an_out_of_range_percentage_is_clamped_both_ways() {
        assert_eq!(UiScale::from_percent(1).percent(), MIN_SCALE_PERCENT);
        assert_eq!(UiScale::from_percent(0).percent(), MIN_SCALE_PERCENT);
        assert_eq!(UiScale::from_percent(u16::MAX).percent(), MAX_SCALE_PERCENT);
    }

    #[test]
    fn a_settings_file_round_trips() {
        let original = Appearance {
            theme: ThemeChoice::default(),
            density: DensityPreset::Comfortable,
            scale: UiScale::from_percent(130),
        };
        let doc = to_toml_string(&original, &crate::fonts::FontChoice::default());
        let parsed = Appearance::from_toml_str(&doc).expect("round-trip parse");
        assert_eq!(original, parsed);
    }

    #[test]
    fn a_written_file_round_trips_through_both_readers() {
        // The writer concatenates two documents, which only stays valid TOML
        // while both halves are flat scalars. If either grows a nested field
        // its `[table]` header lands above the other half's bare keys and the
        // whole file stops parsing — this is where that shows up.
        let appearance = Appearance {
            theme: ThemeChoice::Paper,
            density: DensityPreset::Comfortable,
            scale: UiScale::from_percent(120),
        };
        let fonts = crate::fonts::FontChoice {
            ui: Some("Inter".into()),
            mono: Some("Cascadia Code".into()),
        };
        let doc = to_toml_string(&appearance, &fonts);

        assert_eq!(
            Appearance::from_toml_str(&doc).expect("appearance half parses"),
            appearance
        );
        assert_eq!(
            crate::fonts::FontChoice::from_toml_str(&doc).expect("font half parses"),
            fonts
        );
    }

    #[test]
    fn a_partial_file_keeps_the_other_default() {
        let only_density = Appearance::from_toml_str("density = \"comfortable\"\n").expect("parse");
        assert_eq!(only_density.density, DensityPreset::Comfortable);
        assert_eq!(only_density.scale, UiScale::default());

        let only_scale = Appearance::from_toml_str("scale = 120\n").expect("parse");
        assert_eq!(only_scale.density, DensityPreset::Cockpit);
        assert_eq!(only_scale.scale.percent(), 120);
    }

    #[test]
    fn a_hand_edited_file_is_sanitized_rather_than_trusted() {
        // Parsing does not clamp — serde builds the struct straight from the
        // number — so the loader has to. Without this, `scale = 900` renders a
        // cockpit whose top bar is off the bottom of the screen.
        let wild = Appearance::from_toml_str("scale = 900\n").expect("parse");
        assert_eq!(wild.sanitized().scale.percent(), MAX_SCALE_PERCENT);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let parsed = Appearance::from_toml_str("scale = 110\nfuture_key = true\n")
            .expect("unknown keys tolerated");
        assert_eq!(parsed.scale.percent(), 110);
    }

    #[test]
    fn every_preset_is_offered_and_named() {
        // `ALL` drives the settings picker; a preset missing from it is a
        // preset the user cannot choose.
        assert_eq!(DensityPreset::ALL.len(), 2);
        for preset in DensityPreset::ALL {
            assert!(!preset.label().is_empty());
            assert!(!preset.description().is_empty());
        }
        assert_eq!(DensityPreset::ALL[0], DensityPreset::default());
    }
}
