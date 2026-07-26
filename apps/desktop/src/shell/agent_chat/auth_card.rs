//! The inline ACP authentication card + its view-owned state.
//!
//! When an ACP agent needs login it fails the session open with `AuthRequired`
//! (-32000) and advertises its methods; the worker surfaces them as a
//! [`ThreadEvent::AuthRequired`](oximux_agents::thread::ThreadEvent). This card
//! turns that dead-end into an action: a pill per Agent/Terminal method (click →
//! `authenticate`), an interactive secret form for an EnvVar method (a masked
//! field per variable + a submit that respawns the agent with those values in its
//! env, then authenticates), an optional inline login terminal, and a retry-state
//! error note. The clickable pills + secret fields are built by the view (they
//! need `Context`/`InputState`); this stays a pure assembler like
//! [`super::login_card`].

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::AuthMethodInfo;
use oximux_settings::{Density, Theme, Typography};

/// View-owned state for a pending ACP auth prompt. Ephemeral — never persisted;
/// a restored mid-auth tab fails closed to a fresh [`ThreadEvent::AuthRequired`].
#[derive(Debug, Clone, Default)]
pub(super) struct AuthPrompt {
    /// The methods the agent advertised, in advertised order.
    pub methods: Vec<AuthMethodInfo>,
    /// A prior `authenticate` failure's message, shown as a retry note.
    pub error: Option<String>,
    /// The method id currently being authenticated (spinner state); `None` idle.
    pub pending: Option<String>,
    /// The embedded login terminal's id once a terminal-kind method launched one.
    pub terminal_id: Option<String>,
}

/// Build the auth card: a primary-tinted panel with a heading, an optional retry
/// error note, the caller-built method rows, and an optional inline login
/// terminal element.
pub(super) fn auth_card(
    provider: &str,
    error: Option<&str>,
    method_rows: Vec<AnyElement>,
    terminal: Option<AnyElement>,
    theme: Theme,
    typo: &Typography,
    density: Density,
) -> AnyElement {
    let mut panel = div()
        .flex()
        .flex_col()
        .gap(px(density.pad_row))
        .w_full()
        // Same wrap trap as the markdown bodies: without `min_w_0` a flex ancestor
        // honors the longest unwrapped line and overflows the column.
        .min_w_0()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.status_info.opacity(0.4))
        .bg(theme.status_info.opacity(0.06))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.status_info)
                .child(SharedString::from(format!("{provider} needs you to sign in"))),
        );
    if let Some(err) = error {
        panel = panel.child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.status_error)
                .child(SharedString::from(format!("Sign-in failed: {err}"))),
        );
    }
    for row in method_rows {
        panel = panel.child(row);
    }
    if let Some(term) = terminal {
        panel = panel.child(term);
    }
    panel.into_any_element()
}

/// The interactive form for an EnvVar-kind method (pure assembler): the optional
/// description, a one-line note, the caller-built masked secret fields (one per
/// advertised variable), an optional docs link, and the caller-built submit
/// button. The values the user types are read straight into the respawn's
/// in-flight environment and are NEVER written to the persisted transcript — so
/// this is a genuine sign-in, not a set-and-reopen note. The fields + submit are
/// built by the view (they need `InputState` entities + a `Context` listener);
/// this stays a pure assembler like [`auth_card`].
pub(super) fn env_var_form(
    description: Option<&str>,
    link: Option<&str>,
    field_rows: Vec<AnyElement>,
    submit: AnyElement,
    theme: Theme,
    typo: &Typography,
    density: Density,
) -> AnyElement {
    let mut block = div().flex().flex_col().gap(px(density.pad_row)).w_full().min_w_0();
    if let Some(desc) = description {
        block = block.child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(desc.to_string())),
        );
    }
    block = block.child(
        div()
            .text_size(px(typo.t_body_sm))
            .text_color(theme.fg_muted)
            .child(SharedString::from(
                "Enter your credentials — passed to the agent's environment, never saved:",
            )),
    );
    for row in field_rows {
        block = block.child(row);
    }
    if let Some(url) = link {
        block = block.child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_muted)
                .child(SharedString::from(url.to_string())),
        );
    }
    block = block.child(submit);
    block.into_any_element()
}
