//! Which typefaces the cockpit is drawn in: the faces each platform is
//! guaranteed to ship, and the two a person can put in front of them.
//!
//! # Why the choice does not live in [`Appearance`](crate::Appearance)
//!
//! `Appearance` is `Copy`, and a copy of it is stamped into every
//! [`Density`](crate::Density) so a view can tell in one comparison whether the
//! tokens it cached are still current — that stamp is what makes the pull in
//! [`appearance::sync`](crate::appearance::sync) nearly free. Two owned
//! `String`s inside it would take `Copy` off `Density` and put a heap
//! allocation in every token snapshot in the app: a permanent cost for two
//! names that change about as often as a person changes their mind about
//! typefaces.
//!
//! So the names get a global of their own, and the pull compares them against
//! the names the cached [`Typography`](crate::Typography) is already carrying.
//! That leaves no third source of truth — `FontChoice` is what was asked for,
//! `Typography` is what gets drawn, and [`FontChoice::resolved_ui`] is the one
//! function that turns the first into the second.
//!
//! Both halves live in one `appearance.toml`. Reading it is two independent
//! parses, each ignoring the keys it does not own, which is the forward-compat
//! rule that file already documents. Writing cannot be split the same way, so
//! [`appearance::to_toml_string`](crate::appearance::to_toml_string) is the one
//! place that knows the whole document.
//!
//! # What this module deliberately does not do
//!
//! It does not check that a name is a family the machine actually has. Only the
//! text system can answer that, and it lives two crates up — see the desktop's
//! `font_settings`, which validates on the way in so the name of a font that is
//! not installed never reaches [`Typography`]. [`platform`] is where that
//! matters: gpui looks a family up verbatim, and a miss does not fall back to
//! anything a person chose.

use serde::{Deserialize, Serialize};

/// The faces each platform is guaranteed to ship.
///
/// GPUI looks `Font::family` up verbatim, and `FontFallbacks` only cascades for
/// *individual glyph* lookups inside a family that already loaded. It does not
/// rescue a primary that fails to resolve at all — that goes to
/// `TextSystem::resolve_font`, which walks a hardcoded stack inside gpui whose
/// only monospace entry is the `.ZedMono` sentinel. So the primary should still
/// be a face the OS always has: a miss means the grid is drawn in a typeface
/// nobody chose, and `terminal_canvas` pins every glyph to a `cell_width`
/// measured from `'m'`, so any width mismatch shows up as uneven spacing.
///
/// `apps/desktop` bundles Lilex, which is what `.ZedMono` resolves to, so the
/// floor under that path is at least monospace rather than the proportional
/// `Segoe UI` it used to land on. Keeping the platform primary correct is still
/// the first line of defence; the bundled font is the net, not the plan.
///
/// Lilex is last in every `MONO_FALLBACKS` for the *other* reason — glyphs
/// missing from a family that did load. It maps all 32 Block Elements at its
/// ASCII advance, so it backstops Consolas's 8-of-32.
#[cfg(target_os = "macos")]
pub mod platform {
    /// Menlo is the only mono face guaranteeable on every macOS 13+ install
    /// (Geist Mono is opt-in, and "SF Mono" registers as `.SF NS Mono` /
    /// `SFMono-Regular`, which font-kit's family selector misses). It
    /// carries the full Block Elements range (U+2580–259F) and Box Drawing
    /// (U+2500–257F), which half-block pixel art needs — Claude Code's
    /// mascot is the canonical regression case.
    pub const MONO: &str = "Menlo";
    pub const MONO_FALLBACKS: &[&str] = &["SF Mono", "Monaco", "Lilex"];
    pub const UI: &str = "Helvetica Neue";
    pub const UI_FALLBACKS: &[&str] = &["Helvetica"];
}

/// See the macOS [`platform`] module for what a primary has to satisfy.
#[cfg(target_os = "windows")]
pub mod platform {
    /// Consolas ships with every Windows since Vista. It covers Box Drawing in
    /// full, and of Block Elements it carries exactly the eight that half-block
    /// rendering uses (▀ ▄ █ ▌ ▐ ░ ▒ ▓) — the 24 it lacks are the
    /// eighth-fraction blocks that sparkline-style output wants. Bundled Lilex
    /// covers those (32/32), so unlike the other two fallbacks it is not a
    /// maybe. Cascadia Mono is still worth naming ahead of it for Braille,
    /// which neither Consolas nor Lilex has, but it arrives with Windows
    /// Terminal rather than with Windows.
    pub const MONO: &str = "Consolas";
    pub const MONO_FALLBACKS: &[&str] = &["Cascadia Mono", "Segoe UI Symbol", "Lilex"];
    /// Segoe UI is what the Helvetica Neue lookup was already landing on by
    /// accident. Naming it is a no-op visually and stops the UI chrome from
    /// depending on where GPUI's default happens to point.
    pub const UI: &str = "Segoe UI";
    pub const UI_FALLBACKS: &[&str] = &["Tahoma"];
}

/// Not a platform we ship, but the crate should still build and render
/// something monospaced if someone compiles for it. Unlike macOS and Windows
/// there is no face a Linux install is *guaranteed* to have — a minimal
/// container can lack all three of these — which is the case bundled Lilex
/// exists for.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub mod platform {
    pub const MONO: &str = "DejaVu Sans Mono";
    pub const MONO_FALLBACKS: &[&str] = &["Liberation Mono", "Noto Sans Mono", "Lilex"];
    pub const UI: &str = "DejaVu Sans";
    pub const UI_FALLBACKS: &[&str] = &["Liberation Sans"];
}

/// The two faces a person can override, as stored.
///
/// `None` is not the same as naming the platform family explicitly: the
/// default follows the machine it runs on, a name does not. A settings file
/// carried from a Mac to a Windows box keeps working precisely because the
/// unset case stays unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontChoice {
    /// The face the chrome is drawn in. `None` means [`platform::UI`].
    #[serde(rename = "ui_font", skip_serializing_if = "Option::is_none")]
    pub ui: Option<String>,
    /// The face terminals, diffs and code are drawn in. `None` means
    /// [`platform::MONO`].
    #[serde(rename = "mono_font", skip_serializing_if = "Option::is_none")]
    pub mono: Option<String>,
}

impl FontChoice {
    /// The family the chrome actually gets.
    pub fn resolved_ui(&self) -> &str {
        self.ui.as_deref().unwrap_or(platform::UI)
    }

    /// The family the terminal grid actually gets.
    pub fn resolved_mono(&self) -> &str {
        self.mono.as_deref().unwrap_or(platform::MONO)
    }

    /// True when both faces are left to the platform.
    pub fn is_default(&self) -> bool {
        self.ui.is_none() && self.mono.is_none()
    }

    /// Parse the font keys out of `appearance.toml`, ignoring the rest of the
    /// file — see the module docs for why one document has two readers.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[cfg(feature = "gpui")]
impl gpui::Global for FontChoice {}

/// The face choices in force, or neither of them when the global was never
/// installed — headless tests, and the moments before startup finishes.
///
/// Returns a reference rather than a clone because [`appearance::sync`] calls
/// this once per view per frame purely to compare two names; cloning two
/// `String`s to answer "did anything change" would undo the point of the stamp.
///
/// [`appearance::sync`]: crate::appearance::sync
#[cfg(feature = "gpui")]
pub fn active(cx: &gpui::App) -> &FontChoice {
    static PLATFORM: FontChoice = FontChoice {
        ui: None,
        mono: None,
    };
    cx.try_global::<FontChoice>().unwrap_or(&PLATFORM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_face_resolves_to_the_platform_family() {
        let choice = FontChoice::default();
        assert!(choice.is_default());
        assert_eq!(choice.resolved_ui(), platform::UI);
        assert_eq!(choice.resolved_mono(), platform::MONO);
    }

    #[test]
    fn a_named_face_wins_over_the_platform_family() {
        let choice = FontChoice {
            ui: Some("Inter".into()),
            mono: None,
        };
        assert!(!choice.is_default());
        assert_eq!(choice.resolved_ui(), "Inter");
        assert_eq!(
            choice.resolved_mono(),
            platform::MONO,
            "the face that was not chosen stays with the platform"
        );
    }

    #[test]
    fn the_font_keys_survive_the_appearance_keys_around_them() {
        // The file has two readers and neither owns all of it. This is the
        // half that would break silently: a stricter parse here would reject
        // every real appearance.toml, since all of them carry `theme`.
        let file = concat!(
            "theme = 'paper'\n",
            "density = 'comfortable'\n",
            "scale = 120\n",
            "mono_font = 'Cascadia Code'\n",
        );
        let parsed = FontChoice::from_toml_str(file).expect("appearance keys tolerated");
        assert_eq!(parsed.mono.as_deref(), Some("Cascadia Code"));
        assert!(parsed.ui.is_none());
    }

    #[test]
    fn an_unset_face_is_left_out_of_the_file_rather_than_written_as_empty() {
        // `mono_font = ""` would resolve to a family called "" — a miss, and
        // gpui does not fall back from one gracefully. Absent is the only
        // spelling of "use the platform face" the reader understands.
        let only_ui = FontChoice {
            ui: Some("Inter".into()),
            mono: None,
        };
        let doc = toml::to_string_pretty(&only_ui).expect("serialize");
        assert!(doc.contains("ui_font"));
        assert!(!doc.contains("mono_font"), "got: {doc}");
    }
}
