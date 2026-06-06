//! Rendering for [`SettingsModal`]: the full-window overlay, the modal
//! card (left nav + active pane body), and pane-body dispatch. State +
//! persistence live in the parent module.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window, div, px,
};
use oximux_settings::{Density, Typography};

use super::{
    CARD_HEIGHT, CARD_WIDTH, MODAL_TOP_OFFSET, SettingsModal, SettingsPane, nav, pane_about,
    pane_agents, pane_keybindings, pane_terminal,
};

impl SettingsModal {
    fn render_body(
        &self,
        density: Density,
        typography: &Typography,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        match self.selected {
            SettingsPane::Terminal => pane_terminal::render(self, theme, density, typography, cx),
            SettingsPane::Agents => pane_agents::render(self, theme, density, typography, cx),
            SettingsPane::Keybindings => pane_keybindings::render(theme, density, typography),
            SettingsPane::Appearance => pane_about::render_appearance(theme, typography),
            SettingsPane::About => pane_about::render_about(theme, typography),
        }
    }
}

impl Render for SettingsModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let selected = self.selected;

        let header = div()
            .flex()
            .items_center()
            .h(px(40.0))
            .px(px(density.pad_panel))
            .text_size(px(typography.t_body_md))
            .font_weight(typography.w_semibold)
            .text_color(theme.fg_base)
            .child(selected.label());

        let body = div()
            .id("settings-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(density.pad_panel))
            .pb(px(density.pad_panel))
            .child(self.render_body(density, &typography, cx));

        let body_col = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(header)
            .child(div().w_full().h(px(1.0)).bg(theme.border_inactive))
            .child(body);

        let card = div()
            .flex()
            .flex_row()
            .w(px(CARD_WIDTH))
            .h(px(CARD_HEIGHT))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            // Clip the edge-to-edge nav/header backgrounds to the rounded
            // corners (otherwise the nav column's square corners poke out).
            .overflow_hidden()
            // Lift the card off the workspace, matching the other dialogs.
            .shadow_lg()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.close(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_ev, _window, cx| {
                // Stop the click from bubbling to the overlay's click-outside
                // dismiss. An empty handler does NOT stop propagation, so
                // without this every control click would close the modal.
                cx.stop_propagation();
            })
            .child(nav::render_nav(selected, theme, density, &typography, cx))
            .child(body_col);

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(MODAL_TOP_OFFSET))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| this.close(cx)),
            )
            .child(card)
            .into_any_element()
    }
}
