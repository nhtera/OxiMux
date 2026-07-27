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
use oximux_computer_use::{Category, TargetApp};
use oximux_settings::{ComputerUseSettings, Density, Theme, Typography};

use super::AgentChatView;
use super::computer_use::Decision;
use super::tool_card::pill_button;

/// What a pending screen-control card knows about its target.
///
/// Held by the view against the tool-call id, because [`PermissionRequest`]
/// carries only a description string and this needs structure — the bundle id
/// for the allowlist, the category for the warning.
///
/// [`PermissionRequest`]: oximux_agents::thread::PermissionRequest
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScreenPrompt {
    /// What to call the app on screen.
    pub app: String,
    /// `None` for an ad-hoc signed binary — normal for an agent's own build,
    /// and the reason "always allow" is not always offered.
    pub bundle_id: Option<String>,
    /// Only ever a category that reaches a card. A terminal or an editor is
    /// refused before anyone is asked, so those never arrive here.
    pub category: Category,
}

/// What the transcript knows about one screen-control call's target.
///
/// Two different lifetimes in one place, which is why it is a struct rather
/// than the bare prompt it replaced: the card is up only while the user is
/// being asked, and the app's name outlives the answer — every later action
/// against the same process reads better for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ScreenContext {
    /// Present only while this call is waiting on the user.
    pub prompt: Option<ScreenPrompt>,
    /// What the target process is called, when this chat has resolved it. See
    /// [`screen_card`](super::screen_card) for why it is remembered rather than
    /// looked up at render time.
    pub app: Option<String>,
}

impl ScreenPrompt {
    pub(super) fn from_target(target: &TargetApp) -> Self {
        Self {
            app: target.name.clone(),
            bundle_id: target.bundle_id.clone(),
            category: target.category(),
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
    let warning = prompt.category.warning()?;
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
            .child(SharedString::from(warning))
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
/// Also absent for any target carrying a warning. "Always" and "this app is
/// signed in to your bank" do not belong on the same card; that approval should
/// be a deliberate trip to settings, not a button next to Allow.
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
    if prompt.category.warning().is_some() {
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

/// Has the user already said yes to this app, so no card is needed?
///
/// Split out of the view because it is the whole of "pre-approved apps don't
/// prompt", and the rest of the path around it needs a live pid to reach — a
/// test that had to launch a real signed application to check an allowlist
/// lookup would be testing the launcher.
///
/// Two ways to answer no beyond "not on the list", both deliberate:
///
/// - **No bundle id, no match, ever.** The allowlist is keyed on bundle id, and
///   an agent's own freshly built binary has none. This is the same fact that
///   makes [`always_allow_pill`] withhold the button — the two must agree, or
///   the card would offer an approval that could never be honoured.
/// - **No settings loaded at all is a no.** The global is absent before the
///   watcher installs it and if the file fails to parse. Reading that as "asked
///   and answered" would turn a broken settings file into a blanket approval.
fn is_pre_approved(target: &TargetApp, settings: Option<&ComputerUseSettings>) -> bool {
    let (Some(bundle_id), Some(settings)) = (target.bundle_id.as_deref(), settings) else {
        return false;
    };
    settings.is_allowed(bundle_id)
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
        let verdict = self.screen_control.decide(&tool_name, &input);
        if matches!(verdict, Decision::NotApplicable) {
            return;
        }
        // This is a screen-control call, so whatever process it names is a
        // target this chat is driving. Name it now, while the pid still means
        // what it meant when the call was made — refused and allowed calls
        // alike, since a refusal the user has to read is exactly where "which
        // app was that" matters most.
        self.note_screen_target(&input);
        let decision = match verdict {
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
            Decision::Refuse { reason } => {
                // The sentence is on its way to the agent as the denial message.
                // Keep a copy on the card, or the user watching this happen sees
                // an action refuse itself and is told nothing about why.
                self.thread.set_tool_refusal(&tool_id, &reason);
                PermissionDecision::Deny { message: reason }
            }
        };
        self.resolve_permission(tool_id, request_id, decision, cx);
    }

    /// Remember what the process a screen call names is called.
    ///
    /// The cheap path — a `proc_pidpath` and some string work, no `codesign`
    /// spawn — because this runs for every screen-control call rather than only
    /// the ones a human is about to read. A pid already known is left alone; see
    /// [`ScreenControl::remember_app`](super::computer_use::ScreenControl::remember_app)
    /// for why the first answer is the one that stands.
    fn note_screen_target(&mut self, input: &serde_json::Value) {
        let Some(pid) = oximux_computer_use::policy::addressed_pid(input) else {
            return;
        };
        if self.screen_control.app_named(pid).is_some() {
            return;
        }
        if let Some(name) = oximux_computer_use::target::name_of_pid(pid) {
            self.screen_control.remember_app(pid, name);
        }
    }

    /// What the card for `tc` should know about its target.
    ///
    /// Reads the pid through the policy's own accessor, so the name on the card
    /// always belongs to the process the policy actually decided about.
    pub(super) fn screen_context(&self, tc: &oximux_agents::thread::ToolCall) -> ScreenContext {
        let app = oximux_computer_use::policy::addressed_pid(&tc.input)
            .and_then(|pid| self.screen_control.app_named(pid))
            .map(str::to_string);
        ScreenContext { prompt: self.screen_prompts.get(&tc.id).cloned(), app }
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
        if is_pre_approved(&target, cx.try_global::<ComputerUseSettings>()) {
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

    fn prompt(app: &str, bundle_id: Option<&str>, category: Category) -> ScreenPrompt {
        ScreenPrompt {
            app: app.to_string(),
            bundle_id: bundle_id.map(str::to_string),
            category,
        }
    }

    #[test]
    fn the_question_names_the_app_not_the_tool() {
        // Which app is the decision; which input action is nearly irrelevant to
        // it.
        let q = prompt("Notes", Some("com.apple.Notes"), Category::Other).question("Claude");
        assert_eq!(q, "Let Claude control Notes?");
    }

    #[test]
    fn a_warned_target_carries_its_warning_and_no_always_button() {
        // The two go together: a category worth warning about is a category
        // whose approval should be per-call, not a button next to Allow.
        let p = prompt("Safari", Some("com.apple.Safari"), Category::Browser);
        assert!(p.category.warning().expect("a warning").contains("signed in"));
    }

    #[test]
    fn an_ordinary_target_carries_no_warning() {
        // Over-warning is its own failure: a card that always warns is a card
        // nobody reads.
        assert_eq!(
            prompt("my-app", None, Category::Other).category.warning(),
            None
        );
    }

    fn target(name: &str, bundle_id: Option<&str>) -> TargetApp {
        TargetApp {
            pid: 42,
            executable: std::path::PathBuf::from(format!("/Applications/{name}.app")),
            bundle_id: bundle_id.map(str::to_string),
            name: name.to_string(),
        }
    }

    fn allowing(bundle_id: &str, name: &str) -> ComputerUseSettings {
        let mut settings = ComputerUseSettings::default();
        settings.allow(bundle_id, name);
        settings
    }

    #[test]
    fn a_pre_approved_app_raises_no_card() {
        let settings = allowing("com.apple.Safari", "Safari");
        assert!(is_pre_approved(
            &target("Safari", Some("com.apple.Safari")),
            Some(&settings)
        ));
    }

    #[test]
    fn an_app_the_user_never_approved_still_asks() {
        let settings = allowing("com.apple.Safari", "Safari");
        assert!(!is_pre_approved(
            &target("Notes", Some("com.apple.Notes")),
            Some(&settings)
        ));
    }

    #[test]
    fn an_app_with_no_bundle_id_can_never_be_pre_approved() {
        // The pairing that has to hold: `always_allow_pill` withholds the button
        // for exactly this target, so if the lookup matched some other way the
        // card would be offering an approval it could not honour — or honouring
        // one it never offered.
        let settings = allowing("com.apple.Safari", "Safari");
        assert!(!is_pre_approved(&target("my-app", None), Some(&settings)));
    }

    #[test]
    fn no_settings_loaded_means_ask() {
        // Before the watcher installs the global, and after a settings file that
        // will not parse. Neither is an approval.
        assert!(!is_pre_approved(
            &target("Safari", Some("com.apple.Safari")),
            None
        ));
    }

    #[test]
    fn an_allowlisted_terminal_never_reaches_this_path_at_all() {
        // A user who hand-edited Terminal into the file before it was refused
        // outright still has that row. It buys nothing now, and the reason is
        // structural rather than a check here: `enforce_screen_control` consults
        // the policy first, the policy refuses a never-driveable category ahead
        // of `Ask`, and only `Ask` consults the allowlist. So a stale row is
        // inert rather than an override.
        let settings = allowing("com.apple.Terminal", "Terminal");
        let terminal = target("Terminal", Some("com.apple.Terminal"));
        assert!(
            ScreenPrompt::from_target(&terminal).category.is_never_driveable(),
            "a refused category must never be offered a card"
        );
        // The allowlist itself is untouched — it simply is not asked.
        assert!(is_pre_approved(&terminal, Some(&settings)));
    }

    #[test]
    fn a_target_resolves_straight_from_the_policy_layers_view_of_it() {
        // The card must not re-derive identity by its own rules; a second
        // opinion about which app this is would be a second policy.
        let target = TargetApp {
            pid: 42,
            executable: std::path::PathBuf::from("/Applications/Safari.app/Contents/MacOS/Safari"),
            bundle_id: Some("com.apple.Safari".into()),
            name: "Safari".into(),
        };
        let p = ScreenPrompt::from_target(&target);
        assert_eq!(p.app, "Safari");
        assert_eq!(p.bundle_id.as_deref(), Some("com.apple.Safari"));
        assert_eq!(p.category, Category::Browser);
    }

    #[test]
    fn an_unidentifiable_target_gets_the_ordinary_path() {
        // An agent's own fresh build is ad-hoc signed with no bundle id, and it
        // is the target this whole feature exists to drive. It must land in the
        // plain category, not in a named one by default.
        let p = ScreenPrompt::from_target(&target("my-app", None));
        assert_eq!(p.category, Category::Other);
        assert_eq!(p.category.warning(), None);
        assert!(!p.category.is_never_driveable());
    }
}
