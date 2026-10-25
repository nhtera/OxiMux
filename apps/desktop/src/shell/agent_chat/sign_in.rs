//! Sign-in surfaces of the chat view: the signed-out banner (detect a CLI
//! auth failure, offer "Open terminal to sign in") and the inline ACP / Codex
//! auth card's interactive parts (method pills, browser OAuth, the EnvVar secret
//! form and its respawn-with-env submit). The card's layout itself is the pure
//! assembler in [`super::auth_card`]; this file owns the `Context`-bound halves.
//!
//! Moved verbatim out of `agent_chat/mod.rs` (no behaviour change) to give the
//! view file headroom under the file-size ratchet.

use gpui::{
    AnyElement, AppContext, Context, Focusable, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::Icon;
use gpui_component::input::{Input, InputEvent, InputState};
use oximux_agents::thread::{AuthMethodKind, ThreadEntry, Transport};

use super::{AUTH_TERMINAL_KEY, AgentChatEvent, AgentChatView, auth_card, tool_card};

impl AgentChatView {
    /// True when the latest turn looks like an auth failure the user can fix by
    /// signing in from a terminal. Some CLIs answer with a plain "Not logged in
    /// · Please run /login" reply that settles as an ordinary assistant turn
    /// (no error), so the last assistant text is scanned alongside `last_error`.
    /// Matched case-insensitively and kept broad on purpose across providers.
    pub(super) fn is_signed_out(&self) -> bool {
        const SIGNATURES: &[&str] = &[
            "please run /login",
            "not logged in",
            "logged out",
            "signed out",
            "invalid api key",
            "authentication_error",
        ];
        let hit = |s: &str| {
            let l = s.to_ascii_lowercase();
            SIGNATURES.iter().any(|sig| l.contains(sig))
        };
        if self.thread.last_error.as_deref().is_some_and(hit) {
            return true;
        }
        // Only the most recent assistant reply counts — an old sign-in prompt
        // must not keep the banner up after a later turn succeeds.
        self.thread
            .entries
            .iter()
            .rev()
            .find_map(|e| match e {
                ThreadEntry::Assistant(m) => Some(hit(&m.text)),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// The CLI adapter id for this chat's transport when it has an interactive
    /// binary the user can sign into in a terminal. `None` for ACP presets,
    /// whose sign-in flow isn't a bundled CLI. Gates the signed-out banner.
    pub(super) fn login_adapter_id(&self) -> Option<&'static str> {
        match self.backend.transport {
            Transport::StreamJson => Some("claude-code"),
            Transport::AppServer => Some("codex"),
            Transport::Acp => None,
            // Pi's protocol exposes no sign-in command — authentication happens
            // in the `pi` CLI itself, which is exactly what this banner opens.
            Transport::Rpc => Some("pi"),
            // Same story for omp: `/login` lives in the omp TUI.
            Transport::OmpRpc => Some("omp"),
        }
    }

    /// The signed-out banner's action: a link-styled control that asks the host
    /// to open a terminal running this agent's CLI so `/login` is reachable.
    pub(super) fn open_login_terminal_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let typo = &self.typography;
        div()
            .id("chat-open-login-terminal")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .bg(theme.status_info.opacity(0.15))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_info)
            .hover(|s| s.bg(theme.status_info.opacity(0.28)))
            .child(
                Icon::default()
                    .path("icons/square-terminal.svg")
                    .size(px(13.0))
                    .text_color(theme.status_info),
            )
            .child(SharedString::from("Open terminal to sign in"))
            .on_click(cx.listener(|this, _e, _window, cx| this.request_open_login_terminal(cx)))
    }

    /// Ask the host to spawn a terminal tab running this agent's CLI at the
    /// chat's cwd. No-op for transports with no interactive login binary (ACP).
    fn request_open_login_terminal(&mut self, cx: &mut Context<Self>) {
        if let Some(adapter_id) = self.login_adapter_id() {
            cx.emit(AgentChatEvent::OpenLoginTerminalRequested {
                adapter_id,
                cwd: self.cwd.clone(),
            });
        }
    }

    /// Authenticate with the ACP method the user picked from the auth card: mark
    /// it pending (spinner), then call the connection's `authenticate`, which runs
    /// on the worker and retries the session open on the same connection. A
    /// terminal-kind method mounts its login terminal via a follow-up
    /// `AuthTerminal` event; success clears the card on `SessionInit`.
    fn request_authenticate(&mut self, method_id: String, cx: &mut Context<Self>) {
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id.clone());
            auth.error = None;
        }
        if let Some(conn) = self.connection.as_ref()
            && let Err(e) = conn.authenticate(&method_id)
            && let Some(auth) = self.auth.as_mut()
        {
            auth.pending = None;
            auth.error = Some(e.to_string());
        }
        cx.notify();
    }

    /// Begin a browser OAuth sign-in (Codex "Sign in with ChatGPT"): mark the
    /// method pending and kick the connection's login (fire-and-forget — the RPC
    /// runs on the worker, so this click never blocks the UI). The browser URL
    /// arrives asynchronously as [`ThreadEvent::AuthUrl`] and is opened there; the
    /// card stays pending until `account/login/completed` → [`ThreadEvent::AuthOutcome`]
    /// resolves it. A failure to even start surfaces on the card immediately.
    fn request_browser_login(&mut self, method_id: String, cx: &mut Context<Self>) {
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id);
            auth.error = None;
        }
        if let Some(Err(e)) = self.connection.as_ref().map(|c| c.begin_browser_login())
            && let Some(auth) = self.auth.as_mut()
        {
            auth.pending = None;
            auth.error = Some(e.to_string());
        }
        cx.notify();
    }

    /// A browser-OAuth sign-in pill (Codex). Distinct from [`Self::auth_pill`]
    /// (which runs ACP `authenticate`): clicking this opens a browser via
    /// [`Self::request_browser_login`]. Renders a muted "Opening browser…" while
    /// pending.
    fn browser_login_pill(
        &self,
        method_id: &str,
        label: &str,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Opening browser…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.request_browser_login(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("codex-oauth-{method_id}"),
            label.to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// One clickable auth pill (Agent/Terminal name, or an EnvVar "Retry"). While
    /// its method is authenticating it renders a muted "Authenticating…" label
    /// instead of a button.
    fn auth_pill(
        &self,
        method_id: &str,
        label: &str,
        pending: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Authenticating…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.request_authenticate(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("acp-auth-{method_id}"),
            label.to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// Build the ACP auth card from the pending prompt: a pill per Agent/Terminal
    /// method, an instructions block + Retry for an EnvVar method, an optional
    /// inline login terminal, and a retry-state error note.
    pub(super) fn render_auth_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        let provider = self.provider_label();
        let Some(auth) = self.auth.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        // `reconcile_env_inputs` builds the masked fields for the FIRST EnvVar
        // method only, so render the interactive form for that one and skip any
        // further EnvVar methods — otherwise a second would reuse the first's
        // fields and submit the wrong variable names. (An agent advertising two
        // simultaneous EnvVar methods is unheard of; this just fails safe.)
        let mut env_form_done = false;
        for m in &auth.methods {
            let is_pending = auth.pending.as_deref() == Some(m.id.as_str());
            match &m.kind {
                // EnvVar: an interactive secret form — a masked field per advertised
                // variable (built in `reconcile_env_inputs`, so the values never
                // touch the transcript) + a submit pill that respawns the agent WITH
                // those values in its env, then authenticates.
                AuthMethodKind::EnvVar { .. } if env_form_done => continue,
                AuthMethodKind::EnvVar { link, .. } => {
                    env_form_done = true;
                    let field_rows: Vec<AnyElement> = self
                        .env_inputs
                        .iter()
                        .map(|(name, input)| {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(3.0))
                                .w_full()
                                .min_w_0()
                                .child(
                                    div()
                                        .font_family("monospace")
                                        .text_size(px(typo.t_label_xs))
                                        .text_color(theme.fg_base)
                                        .child(SharedString::from(name.clone())),
                                )
                                .child(Input::new(input))
                                .into_any_element()
                        })
                        .collect();
                    let submit = self.env_submit_pill(&m.id, is_pending, cx);
                    rows.push(auth_card::env_var_form(
                        m.description.as_deref(),
                        link.as_deref(),
                        field_rows,
                        submit,
                        theme,
                        &typo,
                        density,
                    ));
                }
                // BrowserOauth (Codex ChatGPT) → a pill that opens the browser,
                // not the ACP `authenticate` path.
                AuthMethodKind::BrowserOauth => {
                    let pill = self.browser_login_pill(&m.id, &m.name, is_pending, cx);
                    let row = match m.description.as_deref() {
                        Some(desc) => div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(pill)
                            .child(
                                div()
                                    .text_size(px(typo.t_label_xs))
                                    .text_color(theme.fg_muted)
                                    .child(SharedString::from(desc.to_string())),
                            )
                            .into_any_element(),
                        None => pill,
                    };
                    rows.push(row);
                }
                // Agent / Terminal → a single labeled pill (+ its description).
                _ => {
                    let pill = self.auth_pill(&m.id, &m.name, is_pending, cx);
                    let row = match m.description.as_deref() {
                        Some(desc) => div()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(pill)
                            .child(
                                div()
                                    .text_size(px(typo.t_label_xs))
                                    .text_color(theme.fg_muted)
                                    .child(SharedString::from(desc.to_string())),
                            )
                            .into_any_element(),
                        None => pill,
                    };
                    rows.push(row);
                }
            }
        }
        let terminal =
            auth.terminal_id.as_ref().and_then(|_| self.render_embedded_terminal(AUTH_TERMINAL_KEY));
        auth_card::auth_card(provider, auth.error.as_deref(), rows, terminal, theme, &typo, density)
    }

    /// The EnvVar-auth submit button: visually a [`Self::auth_pill`], but on click
    /// it reads the typed secrets and respawns the agent WITH them in its env
    /// (rather than authenticating the current, env-less process). Renders a muted
    /// "Connecting…" while that respawn is in flight.
    fn env_submit_pill(&self, method_id: &str, pending: bool, cx: &mut Context<Self>) -> AnyElement {
        let (theme, typo, density) = (self.theme, self.typography.clone(), self.density);
        if pending {
            return div()
                .px(px(10.0))
                .py(px(3.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from("Connecting…"))
                .into_any_element();
        }
        let id = method_id.to_string();
        let on_click = cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.submit_env_auth(id.clone(), cx);
        });
        tool_card::pill_button(
            format!("acp-env-auth-{method_id}"),
            "Sign in".to_string(),
            theme.status_info,
            density,
            &typo,
            on_click,
        )
    }

    /// Ensure the masked secret fields match the current EnvVar-auth prompt: one
    /// [`InputState`] per advertised variable while an EnvVar method is up, torn
    /// down once the card clears or turns non-EnvVar. Lives here (not the event
    /// fold) because `InputState::new` needs the `Window`. Rebuilds only when the
    /// variable set actually changes, so a half-typed secret survives repaints.
    pub(super) fn reconcile_env_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Variables the current card wants, in advertised order (first EnvVar method).
        let wanted: Vec<String> = self
            .auth
            .as_ref()
            .and_then(|a| {
                a.methods.iter().find_map(|m| match &m.kind {
                    AuthMethodKind::EnvVar { vars, .. } => Some(vars.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        // Already in sync (same vars, same order) → keep the live fields untouched.
        if self.env_inputs.len() == wanted.len()
            && self.env_inputs.iter().zip(&wanted).all(|((name, _), w)| name == w)
        {
            return;
        }
        self.env_inputs.clear();
        self.env_input_subs.clear();
        for (i, name) in wanted.iter().enumerate() {
            let input =
                cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(name.clone()));
            // Repaint the chat view on edits so the masked dots appear live (an
            // embedded `Input` doesn't self-repaint its owner).
            let sub = cx.subscribe(&input, |_this, _input, _ev: &InputEvent, cx| cx.notify());
            // Focus the first field so the user can type without clicking first. The
            // render-time composer-focus fallback only fires when the ROOT holds
            // focus, so this stays put.
            if i == 0 {
                input.read(cx).focus_handle(cx).focus(window, cx);
            }
            self.env_inputs.push((name.clone(), input));
            self.env_input_subs.push(sub);
        }
    }

    /// Collect the typed EnvVar-auth secrets and respawn the agent WITH them in its
    /// environment (which then authenticates) — the only way an env-credentialed
    /// agent can sign in, since a running process can't gain env after it spawned.
    /// The values are read straight into the respawn's in-flight `ConnectSpec.env`
    /// and never persisted. Blank fields are skipped (the agent re-prompts if it
    /// actually needed them); an all-blank submit keeps the card up with a nudge.
    fn submit_env_auth(&mut self, method_id: String, cx: &mut Context<Self>) {
        let env: Vec<(String, String)> = self
            .env_inputs
            .iter()
            .filter_map(|(name, input)| {
                let value = input.read(cx).value().to_string();
                (!value.is_empty()).then(|| (name.clone(), value))
            })
            .collect();
        if env.is_empty() {
            if let Some(auth) = self.auth.as_mut() {
                auth.error = Some("enter the required value(s) to continue".to_string());
            }
            cx.notify();
            return;
        }
        if let Some(auth) = self.auth.as_mut() {
            auth.pending = Some(method_id.clone());
            auth.error = None;
        }
        // The fresh connection emits SessionInit (→ card clears) on success, or
        // AuthRequired again (→ card re-shows) if the credential was wrong.
        self.respawn_with_env(env, Some(method_id), cx);
        cx.notify();
    }
}
