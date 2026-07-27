//! Copy and affordances for a screen-control consent card.
//!
//! Not a fourth card implementation: the ordinary tool card renders this, the
//! same way it renders an MCP elicitation. What lives here is the part that is
//! specific to being asked *"may an agent click on this app for you?"* — naming
//! the target, warning when the target's category makes one click worth much
//! more than one click, and offering a durable "always allow" that a tool
//! approval does not have.
//!
//! # Why the target is resolved once, not per repaint
//!
//! Naming an app costs a `codesign` spawn. A card can be on screen for minutes
//! and repaints many times a second, so the resolution happens when the card
//! goes up and the result is held until it is answered.

use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::PermissionDecision;
use oximux_computer_use::{Sentinel, TargetApp};
use oximux_settings::{ComputerUseSettings, Density, Theme, Typography};

use super::AgentChatView;
use super::computer_use::Decision;
use super::tool_card::pill_button;

/// What a pending screen-control card knows about its target.
///
/// Held by the view against the tool-call id, because [`PermissionRequest`]
/// carries only a description string and this needs structure — the bundle id
/// for the allowlist, the sentinel for the warning.
///
/// [`PermissionRequest`]: oximux_agents::thread::PermissionRequest
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScreenPrompt {
    /// What to call the app on screen.
    pub app: String,
    /// `None` for an ad-hoc signed binary — normal for an agent's own build,
    /// and the reason "always allow" is not always offered.
    pub bundle_id: Option<String>,
    pub sentinel: Option<Sentinel>,
}

impl ScreenPrompt {
    pub(super) fn from_target(target: &TargetApp) -> Self {
        Self {
            app: target.name.clone(),
            bundle_id: target.bundle_id.clone(),
            sentinel: target.sentinel(),
        }
    }

    /// The question the card asks.
    ///
    /// Names the app rather than the tool, because the app is what the user is
    /// deciding about — the difference between `click` and `type_text` matters
    /// far less than the difference between Safari and Terminal.
    pub(super) fn question(&self, provider: &str) -> String {
        format!("Let {provider} control {}?", self.app)
    }
}

/// The warning banner for a high-blast-radius target, or nothing.
///
/// Rendered in the warn accent above the buttons — a user who is about to
/// approve a click into their terminal should not have to infer what that
/// means from the app's name.
pub(super) fn warning_banner(
    prompt: &ScreenPrompt,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> Option<AnyElement> {
    let sentinel = prompt.sentinel?;
    Some(
        div()
            .w_full()
            .px(px(8.0))
            .py(px(6.0))
            .rounded(px(density.r_xs))
            .border_1()
            .border_color(theme.status_warn)
            .bg(theme.status_warn.opacity(0.10))
            .text_size(px(typo.t_body_sm))
            .text_color(theme.status_warn)
            .child(SharedString::from(sentinel.warning()))
            .into_any_element(),
    )
}

/// The "Always allow <App>" pill, when the target has a stable identity to
/// remember it by.
///
/// Absent for an unsigned binary on purpose: a persisted grant is keyed on
/// bundle id, and an agent's freshly built app has none. Offering the button
/// anyway would produce a grant that silently matched nothing — worse than not
/// offering it, because the user would believe they had answered once and for
/// all.
///
/// Also absent for a sentinel target. "Always" and "this is equivalent to
/// giving the agent a shell" do not belong on the same card; that approval
/// should be a deliberate trip to settings, not a button next to Allow.
#[allow(clippy::too_many_arguments)]
pub(super) fn always_allow_pill(
    prompt: &ScreenPrompt,
    tool_id: &str,
    request_id: &str,
    input: &serde_json::Value,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> Option<AnyElement> {
    if prompt.sentinel.is_some() {
        return None;
    }
    let bundle_id = prompt.bundle_id.clone()?;
    let app = prompt.app.clone();
    let (tool_id, request_id, input) = (
        tool_id.to_string(),
        request_id.to_string(),
        input.clone(),
    );
    Some(pill_button(
        format!("screen-always-{tool_id}"),
        format!("Always allow {app}"),
        theme.status_info,
        density,
        typo,
        cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
            this.always_allow_screen_app(
                &bundle_id,
                &app,
                tool_id.clone(),
                request_id.clone(),
                input.clone(),
                cx,
            );
        }),
    ))
}

/// The view's half of the consent flow.
///
/// Lives here rather than in the view's own module for two reasons: it is all
/// one concern, and `agent_chat/mod.rs` is already over its size budget. A child
/// module can still reach the view's private state, so nothing had to be widened
/// to make the move.
impl AgentChatView {
    /// Answer a screen-control call from policy, leaving everything else alone.
    ///
    /// Allow and Refuse resolve without troubling the user. `Ask` is where the
    /// consent UX lives: the target is resolved to something nameable, a
    /// pre-approved app is allowed on the spot, and anything else leaves the
    /// card up carrying the app's name. A refusal's reason goes back to the
    /// agent as the denial message, so it can explain itself rather than
    /// retrying blindly.
    pub(super) fn enforce_screen_control(
        &mut self,
        tool_name: String,
        input: serde_json::Value,
        request_id: String,
        tool_id: String,
        cx: &mut Context<Self>,
    ) {
        let decision = match self.screen_control.decide(&tool_name, &input) {
            Decision::NotApplicable => return,
            Decision::Ask { pid } => {
                if self.prepare_screen_card(&tool_id, pid, cx) {
                    return;
                }
                // Pre-approved in settings: allowed without a card, but still
                // through `resolve_permission`, which re-runs the policy and so
                // records the grant (or refuses a target another chat took).
                PermissionDecision::Allow { updated_input: input }
            }
            Decision::Allow => PermissionDecision::Allow { updated_input: input },
            Decision::Refuse { reason } => PermissionDecision::Deny { message: reason },
        };
        self.resolve_permission(tool_id, request_id, decision, cx);
    }

    /// Work out who an `Ask` is about. Returns whether the card should stay up.
    ///
    /// `false` means the app is on the user's allowlist and the caller should
    /// allow it outright. `true` means a human has to decide, and the prompt has
    /// been stored so the card can name what it is asking about.
    fn prepare_screen_card(
        &mut self,
        tool_id: &str,
        pid: Option<u32>,
        cx: &mut Context<Self>,
    ) -> bool {
        // A `Consent`-class tool carries no pid (`launch_app` names a bundle,
        // not a process). Nothing to resolve; the generic card is honest.
        let Some(target) = pid.and_then(TargetApp::describe) else {
            return true;
        };
        let allowed = target.bundle_id.as_deref().is_some_and(|bundle_id| {
            cx.try_global::<ComputerUseSettings>()
                .is_some_and(|settings| settings.is_allowed(bundle_id))
        });
        if allowed {
            return false;
        }
        self.screen_prompts
            .insert(tool_id.to_string(), ScreenPrompt::from_target(&target));
        true
    }

    /// Add the card's app to the persisted allowlist, then allow this call.
    ///
    /// The write goes through the settings file rather than the global: the
    /// watcher owns the global and reloads it, so setting both here would race
    /// the debouncer and could lose the grant.
    fn always_allow_screen_app(
        &mut self,
        bundle_id: &str,
        app: &str,
        tool_id: String,
        request_id: String,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let mut settings = cx
            .try_global::<ComputerUseSettings>()
            .cloned()
            .unwrap_or_default();
        settings.allow(bundle_id, app);
        if let Err(err) = crate::app_settings::computer_use_settings::save(&settings) {
            tracing::warn!(%err, "could not persist the screen-control allowlist");
            crate::shell::toast::toast(
                cx,
                crate::shell::toast::ToastKind::Error,
                "Could not save the allowlist",
            );
        }
        self.resolve_permission(
            tool_id,
            request_id,
            PermissionDecision::Allow { updated_input: input },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(app: &str, bundle_id: Option<&str>, sentinel: Option<Sentinel>) -> ScreenPrompt {
        ScreenPrompt {
            app: app.to_string(),
            bundle_id: bundle_id.map(str::to_string),
            sentinel,
        }
    }

    #[test]
    fn the_question_names_the_app_not_the_tool() {
        // Which app is the decision; which input action is nearly irrelevant to
        // it.
        let q = prompt("Safari", Some("com.apple.Safari"), None).question("Claude");
        assert_eq!(q, "Let Claude control Safari?");
    }

    #[test]
    fn a_sentinel_target_carries_its_warning() {
        let p = prompt("Terminal", Some("com.apple.Terminal"), Some(Sentinel::Shell));
        assert!(p.sentinel.expect("a warning").warning().contains("shell"));
    }

    #[test]
    fn a_target_resolves_straight_from_the_policy_layers_view_of_it() {
        // The card must not re-derive identity by its own rules; a second
        // opinion about which app this is would be a second policy.
        let target = TargetApp {
            pid: 42,
            executable: std::path::PathBuf::from("/Applications/Terminal.app/Contents/MacOS/Terminal"),
            bundle_id: Some("com.apple.Terminal".into()),
            name: "Terminal".into(),
        };
        let p = ScreenPrompt::from_target(&target);
        assert_eq!(p.app, "Terminal");
        assert_eq!(p.bundle_id.as_deref(), Some("com.apple.Terminal"));
        assert_eq!(p.sentinel, Some(Sentinel::Shell));
    }
}
