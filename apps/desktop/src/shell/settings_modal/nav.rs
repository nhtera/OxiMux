//! Left navigation for the settings modal: the pane enum + nav list.

use gpui::{
    AnyElement, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement, SharedString,
    Styled, div, prelude::FluentBuilder, px, svg,
};
use gpui_component::{
    Icon, Sizable as _,
    input::{Input, InputState},
};
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;

/// Width of the left nav column.
const NAV_WIDTH: f32 = 184.0;

/// Section a pane belongs to in the nav.
///
/// Ten flat rows is past the point where a list is scanned rather than read —
/// the reference cockpit groups the same surfaces under four headings, and
/// without them the only way to find "Remote" is to read every label. The
/// grouping is by *what you are configuring*, not by which subsystem
/// implements it: Voice sits with Agents because dictation is how you talk to
/// one, not because it shares code with them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SettingsGroup {
    Ai,
    Workspace,
    Interface,
    App,
}

impl SettingsGroup {
    /// Rendered in caps per the section-label convention in
    /// `docs/design-guidelines.md`; written here in its natural case so the
    /// string stays readable in code and in search.
    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsGroup::Ai => "AI",
            SettingsGroup::Workspace => "WORKSPACE",
            SettingsGroup::Interface => "INTERFACE",
            SettingsGroup::App => "APP",
        }
    }
}

/// Which settings pane is shown in the body. `Appearance` + `About` are
/// read-only; `Terminal` + `Agents` + `Notifications` round-trip to disk;
/// `Keybindings` is a read-only reference list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsPane {
    /// Branch naming, worktree location, and default-branch freshness.
    Git,
    Terminal,
    Agents,
    Voice,
    ScreenControl,
    Notifications,
    Schedules,
    Remote,
    /// External CLIs OxiMux shells out to — presence, sign-in, and the button
    /// that fixes whichever is missing.
    Integrations,
    Keybindings,
    Appearance,
    About,
}

impl SettingsPane {
    /// Every pane this build knows about, in nav order.
    ///
    /// **One list, not two.** This used to be a pair of cfg-gated `ALL` arrays
    /// with hardcoded lengths, which meant a Mac compiled only one of them:
    /// adding a pane to the array you can see and forgetting the other
    /// compiled clean locally and shipped a settings modal missing that pane
    /// on Linux, with `nav_groups_are_contiguous` vouching for the arm it was
    /// built for. Filtering one list at runtime removes the second place to
    /// forget, and the length constants with it.
    ///
    /// Panes sharing a [`SettingsGroup`] must stay adjacent here: the nav
    /// emits a heading whenever the group changes between rows, so a pane
    /// filed out of order prints its heading twice. `nav_groups_are_contiguous`
    /// holds that invariant — which matters most for the next person adding a
    /// pane at the end of the list, where the obvious place is the wrong one.
    const EVERY: [SettingsPane; 12] = [
        SettingsPane::Agents,
        SettingsPane::Voice,
        SettingsPane::ScreenControl,
        SettingsPane::Git,
        SettingsPane::Terminal,
        SettingsPane::Schedules,
        SettingsPane::Remote,
        SettingsPane::Integrations,
        SettingsPane::Appearance,
        SettingsPane::Keybindings,
        SettingsPane::Notifications,
        SettingsPane::About,
    ];

    /// Whether this pane is offered on the platform being compiled.
    ///
    /// The variant stays defined everywhere so navigation and the pane match
    /// keep one shape; it is simply not offered where screen control does not
    /// exist, rather than opening a pane that can only explain itself away.
    ///
    /// Offered on Windows too, though what it opens is a different pane.
    /// Screen control is not available there, but the decision the pane exists
    /// for — approving an unsigned driver binary — is real, has to be made
    /// before the feature can ever be turned on, and has nowhere else to live.
    fn is_available(self) -> bool {
        match self {
            SettingsPane::ScreenControl => cfg!(any(target_os = "macos", windows)),
            _ => true,
        }
    }

    /// The panes this platform actually shows, in nav order.
    pub(super) fn offered() -> Vec<SettingsPane> {
        Self::EVERY.into_iter().filter(|p| p.is_available()).collect()
    }

    /// Which section this pane files under.
    pub(super) fn group(self) -> SettingsGroup {
        match self {
            SettingsPane::Agents | SettingsPane::Voice | SettingsPane::ScreenControl => {
                SettingsGroup::Ai
            }
            SettingsPane::Git
            | SettingsPane::Terminal
            | SettingsPane::Schedules
            | SettingsPane::Remote
            | SettingsPane::Integrations => SettingsGroup::Workspace,
            SettingsPane::Appearance
            | SettingsPane::Keybindings
            | SettingsPane::Notifications => SettingsGroup::Interface,
            SettingsPane::About => SettingsGroup::App,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsPane::Git => "Git & Source Control",
            SettingsPane::Terminal => "Terminal",
            SettingsPane::Agents => "Agents / AI",
            SettingsPane::Voice => "Voice",
            // "Computer use" rather than anything about screens: it is the term
            // users arrive already knowing, the code has always spelled it that
            // way (`oximux-computer-use`, `computer_use.toml`), and the sidebar
            // entry directly below this one is Remote — which genuinely *is*
            // controlling this screen from elsewhere. Two adjacent rows both
            // named for screens would be read as two halves of one feature.
            SettingsPane::ScreenControl => "Computer use",
            SettingsPane::Notifications => "Notifications",
            SettingsPane::Schedules => "Schedules",
            SettingsPane::Remote => "Remote",
            SettingsPane::Integrations => "Integrations",
            SettingsPane::Keybindings => "Keybindings",
            SettingsPane::Appearance => "Appearance",
            SettingsPane::About => "About",
        }
    }

    /// Leading nav glyph. Terminal/Notifications/Appearance/About resolve
    /// from the bundled component catalog; Keybindings ships a local
    /// `keyboard.svg`.
    fn icon_path(self) -> &'static str {
        match self {
            SettingsPane::Git => "icons/git-branch.svg",
            SettingsPane::Terminal => "icons/square-terminal.svg",
            SettingsPane::Agents => "icons/sparkles.svg",
            SettingsPane::Voice => "icons/mic.svg",
            SettingsPane::ScreenControl => "icons/crosshair.svg",
            SettingsPane::Notifications => "icons/bell.svg",
            SettingsPane::Schedules => "icons/history.svg",
            SettingsPane::Remote => "icons/globe.svg",
            // A plug would be the obvious glyph and the bundle has none; a
            // wrench is what these rows are actually for — fixing something.
            SettingsPane::Integrations => "icons/wrench.svg",
            SettingsPane::Keybindings => "icons/keyboard.svg",
            SettingsPane::Appearance => "icons/palette.svg",
            SettingsPane::About => "icons/info.svg",
        }
    }
}

/// Build the left nav column with a selected-row tint per the list-row
/// convention in `docs/design-guidelines.md`.
pub(super) fn render_nav(
    selected: SettingsPane,
    search_input: Option<&Entity<InputState>>,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> impl IntoElement {
    let mut col = div()
        .flex()
        .flex_col()
        .w(px(NAV_WIDTH))
        .flex_none()
        .h_full()
        .py(px(density.pad_panel))
        .px(px(density.pad_row))
        .gap(px(2.0))
        .border_r_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel)
        .child(search_field(search_input, theme, typography));

    // A heading is emitted whenever the group changes, which is why `ALL` has
    // to stay group-contiguous. Tracking the previous group rather than
    // grouping into buckets keeps the row indices — and so the element ids —
    // identical to the flat list.
    let mut prev: Option<SettingsGroup> = None;
    for (idx, pane) in SettingsPane::offered().into_iter().enumerate() {
        let group = pane.group();
        if prev != Some(group) {
            col = col.child(group_heading(group, theme, density, typography));
            prev = Some(group);
        }
        col = col.child(nav_row(idx, pane, pane == selected, theme, density, typography, cx));
    }
    col
}

/// Section label above the first row of each group. Matches the caps-label
/// convention already used by the diff file rail: `t_label_caps` at semibold
/// in `fg_subtle`, so it reads as structure rather than as another row.
fn group_heading(
    group: SettingsGroup,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> AnyElement {
    div()
        .w_full()
        .flex_shrink_0()
        // Shares `nav_row`'s inset so the heading and the labels beneath it
        // line up on one left edge — the thing that makes a heading read as
        // belonging to its rows rather than floating above them.
        .px(px(density.pad_row))
        .pt(px(density.pad_panel))
        .pb(px(2.0))
        .text_size(px(typography.t_label_caps))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_subtle)
        .child(group.label())
        .into_any_element()
}

/// Focusable search field at the top of the nav — a real text input with a
/// leading glyph. Its value drives the per-pane row filter (read in `view`).
fn search_field(
    search_input: Option<&Entity<InputState>>,
    theme: Theme,
    typography: &Typography,
) -> AnyElement {
    let Some(state) = search_input else {
        return div().into_any_element();
    };
    div()
        .w_full()
        .mb(px(8.0))
        .child(
            Input::new(state)
                .small()
                .shadow_none()
                .text_size(px(typography.t_body_sm))
                .prefix(
                    Icon::default()
                        .path("icons/search.svg")
                        .small()
                        .text_color(theme.fg_subtle),
                ),
        )
        .into_any_element()
}

fn nav_row(
    idx: usize,
    pane: SettingsPane,
    selected: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    // Selected rows get a subtle panel-tint highlight with bright text/icon
    // (native-prefs style); unselected rows are muted and brighten on hover.
    let fg = if selected { theme.fg_base } else { theme.fg_muted };
    div()
        .id(SharedString::from(format!("settings-nav-{idx}")))
        .flex()
        .items_center()
        .gap(px(density.gap_inline))
        .h(px(density.h_action_row))
        .px(px(density.pad_row))
        .rounded(px(density.r_xs))
        .text_size(px(typography.t_body_sm))
        .text_color(fg)
        .cursor_pointer()
        .when(selected, |s| {
            s.bg(theme.bg_panel_alt).font_weight(typography.w_medium)
        })
        .when(!selected, |s| s.hover(|h| h.text_color(theme.fg_base).bg(theme.hover_overlay)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _ev, _window, cx| this.select_pane(pane, cx)),
        )
        .child(
            svg()
                .path(pane.icon_path())
                .size(px(15.0))
                .flex_none()
                .text_color(fg),
        )
        .child(pane.label())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `render_nav` emits a heading whenever the group changes between
    /// consecutive rows, so a pane filed out of order would print its group's
    /// heading a second time further down the list. Ordering is the invariant
    /// that keeps one heading per group, and it is easy to break by adding a
    /// pane in the "obvious" place at the end of `ALL`.
    /// Nothing but Computer use may be platform-gated, and the Git pane in
    /// particular must appear everywhere.
    ///
    /// The old failure this replaces: two cfg-gated arrays meant a pane could
    /// be added to the one your machine compiles and missed in the other,
    /// shipping a settings modal without it on Linux. There is one list now,
    /// so that specific mistake is unrepresentable — this guards the filter
    /// that survived.
    #[test]
    fn only_the_platform_specific_pane_is_ever_withheld() {
        // Screen control is the only pane that is ever withheld, so on the
        // platforms that offer it the two lists are identical, and on the
        // ones that do not they differ by exactly that pane.
        let offered = SettingsPane::offered();
        let withheld: Vec<SettingsPane> =
            SettingsPane::EVERY.into_iter().filter(|p| !offered.contains(p)).collect();
        assert!(
            withheld.iter().all(|p| matches!(p, SettingsPane::ScreenControl)),
            "a pane other than Computer use is being withheld: {withheld:?}"
        );
        assert!(
            offered.contains(&SettingsPane::Git),
            "the Git pane must be offered on every platform"
        );
    }

    #[test]
    fn nav_groups_are_contiguous() {
        let mut seen: Vec<SettingsGroup> = Vec::new();
        let mut prev: Option<SettingsGroup> = None;
        for pane in SettingsPane::offered() {
            let group = pane.group();
            if prev != Some(group) {
                assert!(
                    !seen.contains(&group),
                    "{group:?} appears in two runs of SettingsPane::ALL — \
                     {:?} is filed out of order, and its heading would render twice",
                    pane.label()
                );
                seen.push(group);
                prev = Some(group);
            }
        }
    }

    /// Every group that names itself must actually own a pane, or the nav
    /// grows a heading with nothing under it.
    #[test]
    fn every_group_has_at_least_one_pane() {
        for group in [
            SettingsGroup::Ai,
            SettingsGroup::Workspace,
            SettingsGroup::Interface,
            SettingsGroup::App,
        ] {
            assert!(
                SettingsPane::offered().iter().any(|p| p.group() == group),
                "{group:?} has no panes"
            );
        }
    }

    /// The nav is the only way into a pane, so a pane missing from `ALL` is
    /// unreachable rather than merely hidden.
    #[test]
    fn every_pane_is_reachable_and_labelled() {
        for pane in SettingsPane::offered() {
            assert!(!pane.label().is_empty());
            assert!(!pane.icon_path().is_empty());
        }
    }
}
