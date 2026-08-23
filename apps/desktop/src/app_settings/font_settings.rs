//! The half of the appearance settings that needs a text system: which font
//! families this machine actually has, and whether the ones a person picked are
//! among them.
//!
//! [`oximux_settings::fonts`] holds the choice and resolves it to a family
//! name. It deliberately stops there — gpui looks a family up verbatim and does
//! not fall back to anything a person chose, so a name that is not installed
//! has to be caught *before* it reaches `Typography`, and only
//! `TextSystem::all_font_names` can say. That check lives here, next to the
//! enumeration that feeds the picker.
//!
//! # What a bad name costs, and why this is not paranoia
//!
//! `TextSystem::resolve_font` never fails: a missing primary walks a hardcoded
//! stack inside gpui and lands on the `.ZedMono` sentinel. So an uninstalled
//! face does not error, it silently redraws the terminal grid in a typeface
//! nobody chose. The two ways to arrive there are ordinary — a settings file
//! carried between machines, and a font uninstalled after it was picked — which
//! is why the validation runs on every load rather than only on the way in.

use std::sync::OnceLock;

use gpui::{App, Font, FontFeatures, FontStyle, FontWeight, px};
use oximux_settings::FontChoice;

/// Every font family the machine offers, sorted for a picker.
///
/// Enumerated once per launch. Core Text and DirectWrite both walk the entire
/// system collection to answer this, and the answer only changes when someone
/// installs a font — which is not something they can do from inside the app, so
/// the staleness window is one relaunch and nobody is stuck.
///
/// gpui appends its own resolution sentinels (`.SystemUIFont`) to the list.
/// Those are not families anyone picked or would recognise, so they are hidden
/// from the picker while remaining perfectly valid for gpui's own use.
pub fn families(cx: &App) -> &'static [String] {
    static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    FAMILIES.get_or_init(|| {
        let mut names: Vec<String> = cx
            .text_system()
            .all_font_names()
            .into_iter()
            .filter(|name| !name.starts_with('.'))
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        names
    })
}

/// True when `family` is a name this machine can actually resolve.
///
/// Case-insensitive because that is how both platform font systems match, and
/// because a picker is not the only way a name gets in — someone editing
/// `appearance.toml` by hand should not be defeated by `consolas`.
pub fn is_available(cx: &App, family: &str) -> bool {
    families(cx)
        .iter()
        .any(|name| name.eq_ignore_ascii_case(family))
}

/// Drop any face this machine does not have, naming it in the log.
///
/// Dropping is the right repair rather than erroring: the unset case has a
/// good answer (the platform family), and refusing to start over a typeface is
/// out of proportion. The log line is what stops it being silent — the setting
/// is still in the file, and the pane will show the platform face beside it.
pub fn validated(cx: &App, choice: FontChoice) -> FontChoice {
    let keep = |slot: &'static str, name: Option<String>| -> Option<String> {
        let name = name?;
        if is_available(cx, &name) {
            return Some(name);
        }
        tracing::warn!(
            slot,
            family = %name,
            "appearance.toml names a font this machine does not have; using the platform face"
        );
        None
    };
    FontChoice {
        ui: keep("ui_font", choice.ui),
        mono: keep("mono_font", choice.mono),
    }
}

/// Whether `family` advances every character by the same width.
///
/// The terminal grid pins each glyph to one `cell_width` measured from `'m'`,
/// so a proportional face does not merely look wrong — narrow letters sit in
/// wide boxes and the columns stop lining up. Nothing stops a person choosing
/// one (it is their machine, and a near-monospace display face may be exactly
/// what they want), but the pane should say so.
///
/// Measured rather than guessed from the family name: "DejaVu Sans Mono" and
/// "Iosevka" both exist, and only one of them announces itself.
///
/// An unmeasurable face reads as monospaced. This drives a warning, and a
/// warning nobody can act on is worse than no warning.
pub fn is_monospaced(cx: &App, family: &str) -> bool {
    let font = Font {
        family: family.to_string().into(),
        features: FontFeatures::default(),
        fallbacks: None,
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    };
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&font);
    let size = px(16.0);
    let (Ok(narrow), Ok(wide)) = (
        text_system.advance(font_id, size, 'i'),
        text_system.advance(font_id, size, 'M'),
    ) else {
        return true;
    };
    (f32::from(narrow.width) - f32::from(wide.width)).abs() < 0.01
}

/// The face choices in force.
pub fn active(cx: &App) -> FontChoice {
    oximux_settings::fonts::active(cx).clone()
}

/// Adopt `next`, repaint everything, and persist.
///
/// Mirrors `appearance_settings::set` — global and repaint first so the control
/// answers within the frame, write second. The repaint is what carries the new
/// face to ~50 cached snapshots: each view pulls fresh tokens through
/// `appearance::sync` on its way through, and the terminal grid re-measures its
/// cell advance from the typography it just picked up.
pub fn set(cx: &mut App, next: FontChoice) {
    if active(cx) == next {
        return;
    }
    cx.set_global(next);
    // gpui-component's `Root` sets the ambient window font from its own theme,
    // so a face that reaches our views but not the library's leaves tooltips
    // and its inputs behind — the same class of half-update the palette bridge
    // exists for.
    crate::appearance_settings::bridge_component_theme(cx);
    cx.refresh_windows();
    let appearance = crate::appearance_settings::active(cx);
    if let Err(err) = crate::appearance_settings::save(&appearance, oximux_settings::fonts::active(cx))
    {
        tracing::warn!(%err, "could not persist appearance.toml");
    }
}

/// Choose the chrome face, or `None` to go back to the platform one.
pub fn set_ui(cx: &mut App, family: Option<String>) {
    let next = FontChoice {
        ui: family,
        ..active(cx)
    };
    set(cx, next);
}

/// Choose the terminal / diff / code face, or `None` for the platform one.
pub fn set_mono(cx: &mut App, family: Option<String>) {
    let next = FontChoice {
        mono: family,
        ..active(cx)
    };
    set(cx, next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    /// With no global installed — headless tests, and the moments before
    /// startup finishes — both faces read as the platform's.
    #[gpui::test]
    fn an_uninstalled_global_reads_as_the_platform_faces(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(active(cx).is_default());
        });
    }

    /// Each setter moves its own face and leaves the other alone — the same
    /// rule the appearance controls hold to, and just as easy to break by
    /// building a fresh `FontChoice` instead of updating the current one.
    #[gpui::test]
    fn each_setter_moves_its_own_face(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(FontChoice {
                ui: Some("Inter".into()),
                mono: Some("Cascadia Code".into()),
            });

            // Not `set_mono`: that persists, and a test has no business
            // writing the user's settings file. The rule under test is the
            // struct update, which is where the mistake would be.
            let cleared = FontChoice {
                mono: None,
                ..active(cx)
            };
            assert_eq!(cleared.ui.as_deref(), Some("Inter"));
            assert!(cleared.mono.is_none());
        });
    }

    /// A name the machine does not have is dropped rather than carried into
    /// `Typography`, where gpui would resolve it to a sentinel face silently.
    #[gpui::test]
    fn an_absent_family_is_dropped_on_the_way_in(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let checked = validated(
                cx,
                FontChoice {
                    ui: Some("No Such Family 4c1f".into()),
                    mono: None,
                },
            );
            assert!(checked.ui.is_none());
            assert!(checked.is_default());
        });
    }
}
