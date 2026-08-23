//! Onboarding step 2: the Chat UI vs Terminal choice. Two radio cards writing
//! `AgentLaunchSettings::default_open_mode` on Finish.

use gpui::{
    Context, InteractiveElement as _, MouseButton, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px, svg,
};
use oximux_settings::OpenMode;

use super::OnboardingWizard;

impl OnboardingWizard {
    /// The two open-mode cards plus the terminal-only capability footnote.
    pub(super) fn render_view_body(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let typography = self.typography.clone();
        // The footnote fires only for a terminal-only pick (e.g. Aider) with
        // Chat selected — Chat UI simply won't apply to that agent.
        let footnote = self
            .selected_row()
            .filter(|row| !row.chat_capable && self.open_mode == OpenMode::Chat)
            .map(|row| row.display_name.clone());

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .gap(px(14.0))
                    .child(self.render_mode_card(
                        OpenMode::Chat,
                        "onboarding-mode-chat",
                        "icons/file-text.svg",
                        "Chat UI",
                        "Rendered messages with inline diffs and markdown",
                        cx,
                    ))
                    .child(self.render_mode_card(
                        OpenMode::Terminal,
                        "onboarding-mode-terminal",
                        "icons/square-terminal.svg",
                        "Terminal",
                        "Run the agent's CLI in a terminal pane",
                        cx,
                    )),
            )
            .when_some(footnote, |col, agent_name| {
                col.child(
                    div()
                        .pt(px(14.0))
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.status_warn)
                        .child(format!(
                            "⚠ {agent_name} runs in terminal only; Chat UI applies to chat-capable agents."
                        )),
                )
            })
    }

    fn render_mode_card(
        &self,
        mode: OpenMode,
        id: &'static str,
        icon: &'static str,
        title: &'static str,
        blurb: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        let selected = self.open_mode == mode;

        let radio = div()
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .size(px(15.0))
            .rounded_full()
            .border_1()
            .border_color(if selected { theme.focus_ring } else { theme.fg_subtle })
            .when(selected, |dot| {
                dot.child(
                    div()
                        .absolute()
                        .inset(px(3.0))
                        .rounded_full()
                        .bg(theme.focus_ring),
                )
            });

        let icon_tile = div()
            .flex()
            .items_center()
            .justify_center()
            .size(px(38.0))
            .rounded(px(density.r_card))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(if selected { theme.border_active } else { theme.border_inactive })
            .child(
                svg()
                    .path(icon)
                    .size(px(19.0))
                    .text_color(if selected { theme.fg_base } else { theme.fg_muted }),
            );

        div()
            .id(id)
            .relative()
            .flex_1()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(18.0))
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(if selected { theme.focus_ring } else { theme.border_inactive })
            .rounded(px(density.r_card))
            .cursor_pointer()
            .hover(|s| s.bg(theme.bg_overlay))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _window, cx| {
                    this.open_mode = mode;
                    cx.notify();
                }),
            )
            .child(radio)
            .child(icon_tile.mb(px(22.0)))
            .child(
                div()
                    .text_size(px(typography.t_body_lg))
                    .font_weight(typography.w_semibold)
                    .text_color(theme.fg_base)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_muted)
                    .child(blurb),
            )
    }
}
