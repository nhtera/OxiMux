//! Settings › Agents › Agent delegation: whether chat agents may start helper
//! agents, how far that may go, and whether each chat agent's `oximux` reaches
//! only its own session (and the agents it started).
//!
//! The values live in the shared database's settings keys, the same ones a
//! headless `oximux serve` reads, so both hosts enforce the same caps. A change
//! applies to the next spawn: the host core holds the config in a watch.

use gpui::{AnyElement, AppContext as _, Entity, IntoElement, prelude::FluentBuilder as _, ParentElement, Styled, Subscription, Window, div, px};
use gpui_component::input::{InputEvent, Textarea, TextareaState};
use oximux_remote_host::HostConfig;
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{stepper, toggle_switch};
use super::layout::{SettingEntry, card_surface, entries_card, entry, section_title};
use crate::remote_control::RemoteControl;

/// The host's config right now, or the defaults before the core is installed.
pub(super) fn current(cx: &gpui::App) -> HostConfig {
    cx.try_global::<RemoteControl>()
        .and_then(|rc| rc.host_core())
        .map_or_else(HostConfig::default, |core| core.config())
}

/// Apply one field's `edit` to the host settings and store the result.
///
/// The edit is a closure, not a finished `HostConfig`, so the core can run it
/// on the config in force while it holds its write lock: a struct built from a
/// config this pane read first would write back whatever an `oximux config set`
/// changed in between.
pub(super) fn apply(
    modal: &SettingsModal,
    edit: impl FnOnce(&mut HostConfig),
    cx: &mut gpui::Context<SettingsModal>,
) {
    let switched_on = match cx.try_global::<RemoteControl>().and_then(|rc| rc.host_core()) {
        // Through the core, which holds its settings-write lock across the
        // read, the store and the apply: this pane stores every key at once,
        // and `oximux config set` stores one, so two unlocked writers could
        // leave the store and what the host enforces disagreeing.
        Some(core) => match core.edit_config(edit) {
            Ok(change) => RemoteControl::confinement_switched_on(
                change.before.confine_chat,
                change.after.confine_chat,
            ),
            // Nothing was stored and nothing is in force that was not before,
            // so there is no new posture for open chats to pick up.
            Err(err) => {
                tracing::warn!(?err, "agent delegation settings not saved");
                false
            }
        },
        // No core yet (no runtime at boot): there is nothing to apply and no
        // lock to take, but the keys must still be stored for the next start,
        // and no chat is open to respawn.
        None => {
            let mut next = current(cx);
            edit(&mut next);
            for (key, value) in next.sanitized().entries() {
                if let Err(err) = modal.notify_repo.set(key, &value) {
                    tracing::warn!(%err, key, "agent delegation setting not saved");
                }
            }
            false
        }
    };
    if switched_on {
        // Open chats' agents were spawned without a credential: respawn them
        // (after any running turn) so they pick one up.
        cx.defer(RemoteControl::refresh_chat_credentials);
    }
    cx.notify();
}

/// Why the settings cannot take effect here right now, if they cannot.
fn blocked_reason(cx: &gpui::App) -> Option<&'static str> {
    let rc = cx.try_global::<RemoteControl>()?;
    if !rc.local_enabled() {
        return Some("Needs local access (Settings › Remote). Changes apply once it is on.");
    }
    if !rc.host_core().is_some_and(|core| core.is_owner()) {
        return Some("Another host owns this data directory. Changes apply when this app hosts it again.");
    }
    None
}

/// The verifier's command rules editor, seeded from the host config.
pub(super) fn verify_allow_input(
    window: &mut Window,
    cx: &mut gpui::Context<SettingsModal>,
) -> (Entity<TextareaState>, Subscription) {
    let seed = current(cx).verify_allow.join("\n");
    let input = cx.new(|cx| {
        TextareaState::new(window, cx).auto_grow(4, 10).placeholder("Bash(cargo test:*)").default_value(seed)
    });
    let sub = cx.subscribe_in(&input, window, |this, input, ev: &InputEvent, window, cx| {
        if matches!(ev, InputEvent::Blur) {
            let raw = input.read(cx).value().to_string();
            commit_verify_allow(this, &raw, cx);
            // Show what is in force: rejected lines go, an empty box shows
            // the defaults it restored.
            let shown = current(cx).verify_allow.join("\n");
            if shown != raw.trim() {
                input.update(cx, |s, cx| s.set_value(shown, window, cx));
            }
        }
    });
    (input, sub)
}

/// Store the edited rules, one per line, when they changed. Anything that is
/// not a `Bash(<command>)` rule is dropped by sanitizing; an empty list means
/// the defaults.
pub(super) fn commit_verify_allow(modal: &SettingsModal, raw: &str, cx: &mut gpui::Context<SettingsModal>) {
    let mut rules: Vec<String> = raw.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    if rules.is_empty() {
        rules = HostConfig::default().verify_allow;
    }
    if rules != current(cx).verify_allow {
        apply(modal, move |config| config.verify_allow = rules, cx);
    }
}

pub(super) fn render_section(
    modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> AnyElement {
    let note = blocked_reason(cx).unwrap_or(
        "A guardrail, not isolation: any program running as you can still read the operator's \
         credential. It keeps agents from reaching past their own work by accident.",
    );
    div()
        .flex()
        .flex_col()
        .w_full()
        .gap(px(8.0))
        .child(section_title(
            "Agent delegation",
            "Let chat agents start helper agents, within limits, and keep each one to its own work.",
            theme,
            typography,
        ))
        .child(card_surface(
            theme,
            density,
            entries_card(theme, density, typography, entries(modal, theme, density, typography, cx)),
        ))
        .when_some(modal.verify_allow_input.as_ref(), |d, input| {
            d.child(section_title(
                "Commands a verifier may run",
                "One rule per line, like Bash(cargo test:*). A Claude helper spawned to verify may run these, \
                 and what your own Claude settings allow; anything else is refused without asking. A rule \
                 matches a command's start, so list only tools you trust with any arguments. Empty restores \
                 the defaults.",
                theme,
                typography,
            ))
            .child(Textarea::new(input).text_size(px(typography.t_body_sm)))
        })
        .child(
            div()
                .px(px(2.0))
                .text_size(px(typography.t_sub_label))
                .text_color(theme.fg_subtle)
                .child(note),
        )
        .into_any_element()
}

/// One `−` / `+` row adjusting a cap through `set`, within `1..=max`.
#[allow(clippy::too_many_arguments)]
fn cap_row(
    id: &'static str,
    label: &'static str,
    description: &'static str,
    value: u32,
    max: u32,
    set: fn(&mut HostConfig, u32),
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> SettingEntry {
    let control = stepper(
        id,
        value.to_string(),
        theme,
        density,
        typography,
        move |this: &mut SettingsModal, _w, cx| {
            apply(this, |config| set(config, value.saturating_sub(1).max(1)), cx);
        },
        move |this: &mut SettingsModal, _w, cx| {
            apply(this, |config| set(config, (value + 1).min(max)), cx);
        },
        cx,
    );
    entry(label, description, control)
}

pub(super) fn entries(
    _modal: &SettingsModal,
    theme: Theme,
    density: Density,
    typography: &Typography,
    cx: &mut gpui::Context<SettingsModal>,
) -> Vec<SettingEntry> {
    let config = current(cx);
    let delegation = toggle_switch(
        "delegation-enabled",
        config.delegation_enabled,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            apply(this, |config| config.delegation_enabled = !config.delegation_enabled, cx);
        },
        cx,
    );
    let confine = toggle_switch(
        "delegation-confine-chat",
        config.confine_chat,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            apply(this, |config| config.confine_chat = !config.confine_chat, cx);
        },
        cx,
    );
    let expose = toggle_switch(
        "delegation-expose-tools",
        config.expose_tools,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            apply(this, |config| config.expose_tools = !config.expose_tools, cx);
        },
        cx,
    );
    vec![
        entry(
            "Agent delegation",
            "Chat agents may start helper agents (`oximux delegate spawn`).",
            delegation,
        ),
        entry(
            "Delegation tools for agents",
            "Claude chats get spawn, wait and the rest as tools, beside the `oximux` command. \
             Applies to agents started from now on.",
            expose,
        ),
        entry(
            "Confine chat agents to their own sessions",
            "Each chat agent's `oximux` reaches its own session and the agents it started, \
             nothing else. Applies to agents started from now on.",
            confine,
        ),
        cap_row(
            "delegation-depth",
            "Spawn depth",
            "1: an agent's helpers cannot start their own. 2 is the most allowed.",
            config.caps.depth,
            oximux_agents::lineage::MAX_DEPTH,
            |c, v| c.caps.depth = v,
            theme,
            density,
            typography,
            cx,
        ),
        cap_row(
            "delegation-active-parent",
            "Working helpers per agent",
            "How many of one agent's helpers may be working at once. Finished ones do not count.",
            config.caps.active_per_parent,
            16,
            |c, v| c.caps.active_per_parent = v,
            theme,
            density,
            typography,
            cx,
        ),
        cap_row(
            "delegation-active-root",
            "Working helpers per task",
            "The same limit across a whole tree of helpers started from one chat.",
            config.caps.active_per_root,
            16,
            |c, v| c.caps.active_per_root = v,
            theme,
            density,
            typography,
            cx,
        ),
        cap_row(
            "delegation-open-root",
            "Open helpers per task",
            "Helpers kept open in one tree, finished ones included. Cancel finished helpers to free room.",
            config.caps.open_per_root,
            32,
            |c, v| c.caps.open_per_root = v,
            theme,
            density,
            typography,
            cx,
        ),
    ]
}
