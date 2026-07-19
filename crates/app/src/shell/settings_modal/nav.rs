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

/// Which settings pane is shown in the body. `Appearance` + `About` are
/// read-only; `Terminal` + `Agents` + `Notifications` round-trip to disk;
/// `Keybindings` is a read-only reference list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPane {
    Terminal,
    Agents,
    Voice,
    Notifications,
    Remote,
    Keybindings,
    Appearance,
    About,
}

impl SettingsPane {
    pub(super) const ALL: [SettingsPane; 8] = [
        SettingsPane::Terminal,
        SettingsPane::Agents,
        SettingsPane::Voice,
        SettingsPane::Notifications,
        SettingsPane::Remote,
        SettingsPane::Keybindings,
        SettingsPane::Appearance,
        SettingsPane::About,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            SettingsPane::Terminal => "Terminal",
            SettingsPane::Agents => "Agents / AI",
            SettingsPane::Voice => "Voice",
            SettingsPane::Notifications => "Notifications",
            SettingsPane::Remote => "Remote",
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
            SettingsPane::Terminal => "icons/square-terminal.svg",
            SettingsPane::Agents => "icons/sparkles.svg",
            SettingsPane::Voice => "icons/mic.svg",
            SettingsPane::Notifications => "icons/bell.svg",
            SettingsPane::Remote => "icons/globe.svg",
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

    for (idx, pane) in SettingsPane::ALL.into_iter().enumerate() {
        col = col.child(nav_row(idx, pane, pane == selected, theme, density, typography, cx));
    }
    col
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
