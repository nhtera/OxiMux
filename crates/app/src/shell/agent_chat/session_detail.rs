//! Session-detail popover — what this chat's agent was started with.
//!
//! A session advertises its inventory once, at init: the model and working
//! directory, the tools it loaded, the MCP servers it connected (and whether
//! they actually came up), and the subagent types available to it. That answers
//! the questions users otherwise resort to asking the agent itself ("do you have
//! the context7 server?", "which directory are you in?") — so it is surfaced
//! directly instead.
//!
//! Read-only by design: this reports what the session HAS, and none of it can be
//! changed without restarting the agent. Settings live in the composer.
//!
//! Backends differ in how much they advertise — Claude's `system/init` carries
//! all of it, Codex and ACP carry none — so every section is omitted rather than
//! shown empty, and the trigger hides entirely when there is nothing to tell.
//! The data is cached with the transcript, so a restored chat can answer these
//! questions before its resumed process says anything.

use gpui::{
    div, px, AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled,
};
use oximux_agents::thread::SessionMeta;

use super::{AgentChatView, RAIL_W};

/// Fixed width of the open popover.
const PANEL_W: f32 = 300.0;
/// Cap the popover's height; past this it scrolls internally. A session with 30+
/// tools must not grow a panel taller than the transcript behind it.
const PANEL_MAX_H: f32 = 360.0;

impl AgentChatView {
    /// The session-detail trigger, plus the popover when it's open. `None` when
    /// the session advertised nothing worth showing (Codex/ACP today), so the
    /// affordance never opens onto an empty panel.
    pub(super) fn render_session_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let meta = &self.thread.session_meta;
        if meta == &SessionMeta::default() {
            return None;
        }
        let theme = self.theme;
        let typo = &self.typography;
        let open = self.session_detail_open;

        let trigger = div()
            .id("session-detail-trigger")
            .absolute()
            .top(px(4.0))
            .right(px(8.0))
            .px(px(6.0))
            .rounded(px(self.density.r_xs))
            .text_size(px(typo.t_body_sm))
            .text_color(if open { theme.fg_base } else { theme.fg_subtle })
            .cursor_pointer()
            .hover(|s| s.text_color(theme.fg_base))
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.session_detail_open = !this.session_detail_open;
                cx.notify();
            }))
            .child(SharedString::from("ⓘ"));

        if !open {
            return Some(trigger.into_any_element());
        }

        let mut panel = div()
            .id("session-detail-panel")
            .absolute()
            .top(px(26.0))
            .right(px(8.0))
            .w(px(PANEL_W))
            .max_h(px(PANEL_MAX_H))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(self.density.pad_row))
            .rounded(px(self.density.r_card))
            .border_1()
            .border_color(theme.border_inactive)
            .bg(theme.bg_panel_alt)
            .overflow_y_scroll()
            .text_size(px(typo.t_body_sm));

        if let Some(model) = self.thread.model.as_deref().or(self.model.as_deref()) {
            panel = panel.child(row("Model", model.to_string(), theme));
        }
        if let Some(cwd) = meta.cwd.as_deref() {
            panel = panel.child(row("Directory", cwd.to_string(), theme));
        }
        if !meta.tools.is_empty() {
            // A count plus the names: the count is the answer to "is anything
            // loaded", the names to "is X loaded".
            panel = panel.child(section(format!("Tools ({})", meta.tools.len()), theme, typo));
            panel = panel.child(wrapped(meta.tools.join(", "), theme));
        }
        if !meta.mcp_servers.is_empty() {
            panel = panel.child(section(
                format!("MCP servers ({})", meta.mcp_servers.len()),
                theme,
                typo,
            ));
            for s in &meta.mcp_servers {
                // A server that failed to come up is the single most useful fact
                // here, so status gets its own colour rather than blending in.
                let ok = s.status.eq_ignore_ascii_case("connected");
                panel = panel.child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .gap(px(6.0))
                        .w_full()
                        .child(div().flex_1().min_w(px(0.0)).text_color(theme.fg_muted).child(
                            SharedString::from(s.name.clone()),
                        ))
                        .child(
                            div()
                                .flex_none()
                                .text_color(if ok { theme.status_added } else { theme.fg_subtle })
                                .child(SharedString::from(s.status.clone())),
                        ),
                );
            }
        }
        if !meta.agents.is_empty() {
            panel = panel.child(section(format!("Subagents ({})", meta.agents.len()), theme, typo));
            panel = panel.child(wrapped(meta.agents.join(", "), theme));
        }

        Some(
            div()
                .absolute()
                .top(px(0.0))
                .left(px(RAIL_W))
                .right(px(0.0))
                .child(trigger)
                .child(panel)
                .into_any_element(),
        )
    }
}

/// A `label: value` line for a single fact.
fn row(label: &str, value: String, theme: oximux_settings::Theme) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(6.0))
        .w_full()
        .child(
            div()
                .flex_none()
                .text_color(theme.fg_subtle)
                .child(SharedString::from(label.to_string())),
        )
        // `min_w_0` lets a long path wrap instead of forcing the panel wider.
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_color(theme.fg_base)
                .child(SharedString::from(value)),
        )
        .into_any_element()
}

/// A muted section heading.
fn section(
    label: String,
    theme: oximux_settings::Theme,
    typo: &oximux_settings::Typography,
) -> AnyElement {
    div()
        .w_full()
        .pt(px(6.0))
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(label))
        .into_any_element()
}

/// A wrapping run of comma-joined names.
fn wrapped(text: String, theme: oximux_settings::Theme) -> AnyElement {
    div()
        .w_full()
        .min_w(px(0.0))
        .text_color(theme.fg_muted)
        .child(SharedString::from(text))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use oximux_agents::thread::{McpServerStatus, StubConnection};
    use oximux_settings::{Density, Theme, Typography};

    /// The trigger only exists once a session has advertised something — a
    /// backend that advertises nothing (Codex, ACP) must not show an affordance
    /// that opens onto an empty panel.
    #[gpui::test]
    async fn trigger_hidden_until_a_session_advertises_something(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.add_window(|window, cx| {
            AgentChatView::with_connection_for_test(
                Box::new(StubConnection::default()),
                Theme::default(),
                Density::default(),
                Typography::default(),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        window
            .update(cx, |view, _window, cx| {
                assert!(
                    view.render_session_detail(cx).is_none(),
                    "nothing advertised → no trigger"
                );

                view.thread.session_meta = SessionMeta {
                    cwd: Some("/repo".into()),
                    tools: vec!["Bash".into()],
                    mcp_servers: vec![McpServerStatus {
                        name: "ctx7".into(),
                        status: "connected".into(),
                    }],
                    agents: vec!["code-reviewer".into()],
                };
                assert!(view.render_session_detail(cx).is_some(), "advertised → trigger shows");
                // Opening is a pure state flip, and rendering it must not panic
                // on any section (this meta populates all four).
                assert!(!view.session_detail_open);
                view.session_detail_open = true;
                assert!(view.render_session_detail(cx).is_some(), "open panel renders");
            })
            .expect("window update");
    }
}
