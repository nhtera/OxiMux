//! App-side loader and writer for `appearance.toml` — the palette, the density
//! preset, the whole-UI zoom, and the two font families.
//!
//! The first three are the [`Appearance`] global; the faces are a
//! [`FontChoice`] beside it, because they cannot live in a `Copy` stamp (see
//! [`oximux_settings::fonts`]). This module owns the file both are written to;
//! `font_settings` owns the half that needs a text system to validate.
//!
//! Startup reads `appearance.toml` from the app data dir (seeding a default so
//! the file is there to look at), sanitizes it, and installs it as a GPUI
//! global. Every view then resolves its own tokens from that global on render
//! — see [`oximux_settings::appearance::sync`] for why the refresh is a pull
//! rather than a push.
//!
//! # No file watch, unlike `terminal.toml`
//!
//! The terminal settings are file-only, so a watcher is the *only* way an edit
//! can reach the app. Appearance has controls in the Settings modal, which
//! makes the app the writer — and a debounced watch between a click and its
//! effect would put a quarter-second of lag on a control whose entire job is
//! immediate visual feedback. So [`set`] writes the global first and the file
//! second. The cost is that hand-editing `appearance.toml` needs a relaunch,
//! which is the right trade for the surface that has a UI.

use std::path::PathBuf;

use gpui::App;
use oximux_settings::{Appearance, FontChoice};

fn settings_path() -> Option<PathBuf> {
    crate::app_paths::data_dir().map(|d| d.join(Appearance::FILE_NAME))
}

/// Read + sanitize from disk, falling back to the shipped default on a missing
/// file or a parse error (logged, so a typo is visible without a crash).
///
/// One file, two readers: the font names cannot live in the `Copy` stamp the
/// token scales compare on, so they parse separately out of the same text. Each
/// half ignores the keys it does not own — see [`oximux_settings::fonts`].
fn load() -> (Appearance, FontChoice) {
    let Some(path) = settings_path() else {
        return (Appearance::default(), FontChoice::default());
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Absent is the common case (fresh install) — silent default.
        return (Appearance::default(), FontChoice::default());
    };

    let appearance = match Appearance::from_toml_str(&text) {
        Ok(parsed) => {
            let clean = parsed.sanitized();
            if clean != parsed {
                // Say so rather than clamping in silence: the number on disk
                // and the number in use would otherwise disagree with no way
                // to tell from inside the app.
                tracing::warn!(
                    ?path,
                    "appearance.toml was out of range; clamped to the supported zoom"
                );
            }
            clean
        }
        Err(err) => {
            tracing::warn!(?path, %err, "appearance.toml parse failed; using defaults");
            Appearance::default()
        }
    };

    // A malformed font key must not cost the density and zoom beside it, and
    // vice versa: two parses means one typo takes out one setting.
    let fonts = FontChoice::from_toml_str(&text).unwrap_or_else(|err| {
        tracing::warn!(?path, %err, "appearance.toml font keys unreadable; using the platform faces");
        FontChoice::default()
    });

    (appearance, fonts)
}

/// Write a default `appearance.toml` if none exists, so the keys are visible
/// to anyone who goes looking. Best-effort; a failure only costs the template.
fn seed_default_if_absent() {
    let Some(path) = settings_path() else { return };
    if path.exists() {
        return;
    }
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_err()
    {
        return;
    }
    let body = oximux_settings::appearance::to_toml_string(
        &Appearance::default(),
        &FontChoice::default(),
    );
    if let Err(err) = std::fs::write(&path, body) {
        tracing::warn!(?path, %err, "could not seed default appearance.toml");
    }
}

/// Load `appearance.toml` and install the global. Call once from the app's
/// `run` closure, before any window opens — a window that opens first would
/// paint one frame at the default size before correcting itself.
pub fn install(cx: &mut App) {
    seed_default_if_absent();
    let (loaded, fonts) = load();
    if loaded != Appearance::default() {
        tracing::info!(
            theme = ?loaded.theme,
            density = ?loaded.density,
            scale = loaded.scale.percent(),
            "appearance loaded"
        );
    }
    // Validated on every load, not only when the pane writes one: the two ways
    // to hold a name for a font that is not installed are carrying the file
    // between machines and uninstalling the font afterwards, and neither goes
    // through the picker. See `font_settings`.
    let fonts = crate::font_settings::validated(cx, fonts);
    if !fonts.is_default() {
        tracing::info!(ui = ?fonts.ui, mono = ?fonts.mono, "font choices loaded");
    }
    publish_terminal_polarity(loaded);
    cx.set_global(fonts);
    cx.set_global(loaded);
}

/// Tell the terminal emulator which way the window reads.
///
/// A child program asks twice and picks a whole palette from the answer:
/// `COLORFGBG` when it starts, OSC 11 over the TTY while it runs. Both used to
/// say "dark" unconditionally, so a Paper window got CLIs — Claude Code, fzf,
/// delta — emitting truecolor tuned for near-black onto white. Truecolor
/// carries explicit r/g/b, so the renderer's own palette never sees it; the
/// only place to fix it is the answer.
///
/// What this cannot do is recolor a program that is already running: it chose
/// its palette at startup and has no reason to ask again. New panes, and
/// anything that re-queries, follow immediately.
fn publish_terminal_polarity(appearance: Appearance) {
    oximux_pty::set_background_polarity(if appearance.theme.is_light() {
        oximux_pty::BackgroundPolarity::Light
    } else {
        oximux_pty::BackgroundPolarity::Dark
    });
}

/// The appearance in force. Falls back to the shipped default when the global
/// was never installed, so headless tests and early startup stay total.
pub fn active(cx: &App) -> Appearance {
    oximux_settings::appearance::active(cx)
}

/// Adopt `next`, repaint everything, and persist.
///
/// Order matters. The global and the repaint come first so the control the
/// user just clicked answers within the frame; the write follows, and a failed
/// write costs the setting at next launch rather than the response now.
///
/// [`gpui::App::refresh_windows`] is what makes one assignment reach ~50 views:
/// it marks every window dirty, each view re-renders, and each pulls its fresh
/// tokens on the way through.
pub fn set(cx: &mut App, next: Appearance) {
    let next = next.sanitized();
    if active(cx) == next {
        return;
    }
    cx.set_global(next);
    bridge_component_theme(cx);
    publish_terminal_polarity(next);
    cx.refresh_windows();
    if let Err(err) = save(&next, oximux_settings::fonts::active(cx)) {
        tracing::warn!(%err, "could not persist appearance.toml");
    }
}

/// The four transitions the controls make, as pure functions.
///
/// Split out from the `cx`-taking wrappers below so the rule that binds them —
/// each control moves its own field and leaves the other alone — can be tested
/// without a global or a settings file. It is an easy rule to break by
/// building a fresh `Appearance` instead of updating the current one, and the
/// break is invisible until someone who set a preset presses zoom-reset and
/// watches their preset go with it.
mod step {
    use oximux_settings::{Appearance, DensityPreset, ThemeChoice, UiScale, UsageDetail};

    pub(super) fn zoom_in(current: Appearance) -> Appearance {
        Appearance {
            scale: current.scale.zoomed_in(),
            ..current
        }
    }

    pub(super) fn zoom_out(current: Appearance) -> Appearance {
        Appearance {
            scale: current.scale.zoomed_out(),
            ..current
        }
    }

    pub(super) fn zoom_reset(current: Appearance) -> Appearance {
        Appearance {
            scale: UiScale::default(),
            ..current
        }
    }

    pub(super) fn density(current: Appearance, density: DensityPreset) -> Appearance {
        Appearance { density, ..current }
    }

    pub(super) fn theme(current: Appearance, theme: ThemeChoice) -> Appearance {
        Appearance { theme, ..current }
    }

    pub(super) fn usage_detail(current: Appearance, usage_detail: UsageDetail) -> Appearance {
        Appearance {
            usage_detail,
            ..current
        }
    }
}

/// One step larger, up to the supported maximum.
pub fn zoom_in(cx: &mut App) {
    let next = step::zoom_in(active(cx));
    set(cx, next);
}

/// One step smaller, down to the supported minimum.
pub fn zoom_out(cx: &mut App) {
    let next = step::zoom_out(active(cx));
    set(cx, next);
}

/// Back to 100%, leaving the density preset alone.
pub fn zoom_reset(cx: &mut App) {
    let next = step::zoom_reset(active(cx));
    set(cx, next);
}

/// Switch density preset, leaving the palette and the zoom alone.
pub fn set_density(cx: &mut App, density: oximux_settings::DensityPreset) {
    let next = step::density(active(cx), density);
    set(cx, next);
}

/// Switch palette, leaving the density and the zoom alone.
pub fn set_theme(cx: &mut App, theme: oximux_settings::ThemeChoice) {
    let next = step::theme(active(cx), theme);
    set(cx, next);
}

/// Switch how much the usage meter spells out, leaving the rest alone.
pub fn set_usage_detail(cx: &mut App, usage_detail: oximux_settings::UsageDetail) {
    let next = step::usage_detail(active(cx), usage_detail);
    set(cx, next);
}

/// Bring gpui-component's own theme into line with ours.
///
/// The library paints its `Input`s, `Button`s and `TabBar`s from a theme it
/// owns and cannot see ours, so anything it draws has to be pushed across:
/// the light/dark mode it resolves its own defaults from, the two colours
/// whose defaults are tuned for a light shadcn page and read as
/// "always focused" against a deep panel fill, the corner radii, and the two
/// font families.
///
/// The families matter more than they look. `gpui_component::Root` wraps the
/// whole window and sets `font_family` on it, so the library's idea of the UI
/// face is the *ambient* face for everything that does not name its own —
/// tooltips and its search inputs among them. Its default is `.SystemUIFont`,
/// which is why nobody noticed while there was nothing to choose.
///
/// Called at startup and again on every change. Skipping the second call is
/// the bug this exists to prevent — it leaves every text field in the
/// previous theme while the chrome around it moves.
///
/// A no-op when the library's theme was never installed, which is the case in
/// headless tests.
pub fn bridge_component_theme(cx: &mut App) {
    if !cx.has_global::<gpui_component::Theme>() {
        return;
    }
    let appearance = active(cx);
    let palette = oximux_settings::Theme::for_appearance(appearance);
    let density = oximux_settings::Density::for_appearance(appearance);
    // Resolved before `global_mut` takes the mutable borrow.
    let (ui_face, mono_face) = {
        let fonts = oximux_settings::fonts::active(cx);
        (
            fonts.resolved_ui().to_string(),
            fonts.resolved_mono().to_string(),
        )
    };
    // Mode first: `change` rebuilds the library's colour set from its own
    // defaults, so the overrides below have to land after it or they are
    // discarded.
    let mode = if appearance.theme.is_light() {
        gpui_component::ThemeMode::Light
    } else {
        gpui_component::ThemeMode::Dark
    };
    gpui_component::Theme::change(mode, None, cx);

    let component_theme = gpui_component::Theme::global_mut(cx);
    // Inputs rest on `border_input` (alpha over the surface, stronger than the
    // hairline dividers so the type-here affordance reads), and `focus_ring`
    // is the dedicated focus accent — same tokens, single source of truth.
    component_theme.colors.input = palette.border_input;
    component_theme.colors.ring = palette.focus_ring;
    // Radius, bridged for the same reason: the library cannot see our scale.
    // It happens to default to 6/8 — the same scale — so at 100% this changes
    // nothing. It is here so the two cannot drift apart silently, which is
    // precisely what had happened to the radii it does not own: hand-rolled
    // chrome sat at 4 while every `Input` and `Button` beside it was already 6.
    component_theme.radius = gpui::px(density.r_xs);
    component_theme.radius_lg = gpui::px(density.r_card);
    component_theme.font_family = ui_face.into();
    component_theme.mono_font_family = mono_face.into();
}

/// Persist to `appearance.toml`. Public for tests; production goes through
/// [`set`] or `font_settings::set`, which also update the globals everything
/// reads.
///
/// Takes both halves because the file holds both and a write replaces it: a
/// writer that knew only about the density would erase a font choice every time
/// someone pressed zoom.
pub fn save(appearance: &Appearance, fonts: &FontChoice) -> std::io::Result<()> {
    let path =
        settings_path().ok_or_else(|| std::io::Error::other("no app data dir for appearance"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        &path,
        oximux_settings::appearance::to_toml_string(appearance, fonts),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_settings::{
        Density, DensityPreset, FontChoice, Theme, ThemeChoice, Typography, UiScale, UsageDetail,
    };

    /// A preset choice must survive a zoom, and a zoom must survive a preset
    /// choice. They are independent controls; a user who set Comfortable and
    /// then pressed zoom-reset should still be on Comfortable.
    #[test]
    fn each_control_moves_its_own_field_and_leaves_the_other() {
        let start = Appearance {
            theme: ThemeChoice::Paper,
            density: DensityPreset::Comfortable,
            scale: UiScale::from_percent(130),
            usage_detail: UsageDetail::Compact,
        };

        let zoomed = step::zoom_in(start);
        assert_eq!(zoomed.density, DensityPreset::Comfortable);
        assert_eq!(zoomed.scale.percent(), 140);

        let out = step::zoom_out(start);
        assert_eq!(out.density, DensityPreset::Comfortable);
        assert_eq!(out.scale.percent(), 120);

        let reset = step::zoom_reset(start);
        assert_eq!(reset.density, DensityPreset::Comfortable, "reset kept the preset");
        assert!(reset.scale.is_default());

        let tightened = step::density(start, DensityPreset::Cockpit);
        assert_eq!(tightened.density, DensityPreset::Cockpit);
        assert_eq!(tightened.scale.percent(), 130, "preset kept the zoom");

        // And the palette is a third independent axis: none of the four moved
        // it, and switching it leaves the other two where they were.
        for moved in [zoomed, out, reset, tightened] {
            assert_eq!(moved.theme, ThemeChoice::Paper, "palette survived");
        }
        let relit = step::theme(start, ThemeChoice::Charcoal);
        assert_eq!(relit.theme, ThemeChoice::Charcoal);
        assert_eq!(relit.density, DensityPreset::Comfortable);
        assert_eq!(relit.scale.percent(), 130);

        // And a fourth: the meter's detail level is nobody else's business.
        for moved in [zoomed, out, reset, tightened, relit] {
            assert_eq!(moved.usage_detail, UsageDetail::Compact, "meter detail survived");
        }
        let spelled_out = step::usage_detail(start, UsageDetail::Verbose);
        assert_eq!(spelled_out.usage_detail, UsageDetail::Verbose);
        assert_eq!(spelled_out.theme, ThemeChoice::Paper);
        assert_eq!(spelled_out.density, DensityPreset::Comfortable);
        assert_eq!(spelled_out.scale.percent(), 130);
    }

    /// The pull: a view holding tokens from before a change picks up the new
    /// ones on its next render. This is what stands in for the push we do not
    /// do — see `oximux_settings::appearance::sync`.
    #[gpui::test]
    fn a_stale_snapshot_is_refreshed_by_the_pull(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut theme = Theme::charcoal();
            let mut density = Density::cockpit();
            let mut typography = Typography::cockpit();

            cx.set_global(Appearance {
                theme: ThemeChoice::Paper,
                density: DensityPreset::Comfortable,
                scale: UiScale::from_percent(120),
                ..Appearance::default()
            });
            oximux_settings::appearance::sync(&mut theme, &mut density, &mut typography, cx);

            assert_eq!(density.h_row, Density::comfortable().h_row * 1.2);
            assert_eq!(typography.t_body_sm, Typography::cockpit().t_body_sm * 1.2);
            // Pinned against both controls — the macOS traffic lights cannot move.
            assert_eq!(density.h_top_bar, Density::cockpit().h_top_bar);
            // The palette rides the same pull -- a view that refreshed its
            // sizes but kept the old colours would be the worse half-update.
            assert!(theme.is_light());
        });
    }

    /// A face change carries no stamp — `Appearance` is `Copy` and the names
    /// cannot ride in it — so the pull has to notice it some other way. This is
    /// the case a stamp-only comparison misses entirely.
    #[gpui::test]
    fn a_font_change_alone_reaches_a_stale_snapshot(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut theme = Theme::charcoal();
            let mut density = Density::for_appearance(Appearance::default());
            let mut typography = Typography::cockpit();

            cx.set_global(FontChoice {
                ui: None,
                mono: Some("Cascadia Code".into()),
            });
            oximux_settings::appearance::sync(&mut theme, &mut density, &mut typography, cx);

            assert_eq!(typography.family_mono.as_ref(), "Cascadia Code");
            assert_eq!(
                density.appearance,
                Appearance::default(),
                "nothing about the density or zoom moved — only the face did"
            );
        });
    }

    /// And the way back: clearing a choice has to repaint too. Comparing the
    /// `Option` rather than the resolved name would leave every view in the
    /// typeface the user had just removed, with the pane showing the default.
    #[gpui::test]
    fn clearing_a_font_choice_reaches_the_snapshot_too(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut theme = Theme::charcoal();
            let mut density = Density::for_appearance(Appearance::default());
            let mut typography = Typography::cockpit().with_fonts(&FontChoice {
                ui: None,
                mono: Some("Cascadia Code".into()),
            });

            cx.set_global(FontChoice::default());
            oximux_settings::appearance::sync(&mut theme, &mut density, &mut typography, cx);

            assert_eq!(
                typography.family_mono,
                Typography::cockpit().family_mono,
                "back to the platform face"
            );
        });
    }

    /// The common case has to cost nothing: with nothing changed, the pull
    /// must leave the snapshot exactly as it found it rather than rebuilding
    /// a `Typography` (and its fallback list) on every view, every frame.
    #[gpui::test]
    fn the_pull_is_inert_when_nothing_changed(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut theme = Theme::charcoal();
            let mut density = Density::for_appearance(Appearance::default());
            let mut typography = Typography::cockpit();
            let before = typography.mono_fallbacks.as_ptr();

            oximux_settings::appearance::sync(&mut theme, &mut density, &mut typography, cx);

            assert_eq!(density.h_row, Density::cockpit().h_row);
            assert_eq!(
                typography.mono_fallbacks.as_ptr(),
                before,
                "an unchanged appearance must not reallocate the fallback list"
            );
        });
    }

    /// With no global installed at all — headless tests, and the moments
    /// before startup finishes — everything must still resolve to the shipped
    /// default rather than panicking.
    #[gpui::test]
    fn an_uninstalled_global_reads_as_the_default(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert_eq!(active(cx), Appearance::default());
        });
    }
}
