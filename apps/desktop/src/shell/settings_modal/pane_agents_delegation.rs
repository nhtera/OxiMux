//! Settings › Agents › Agent delegation: whether chat agents may start helper
//! agents, how far that may go, and whether each chat agent's `oximux` reaches
//! only its own session (and the agents it started).
//!
//! The values live in the shared database's settings keys, the same ones a
//! headless `oximux serve` reads, so both hosts enforce the same caps. A change
//! applies to the next spawn: the host core holds the config in a watch.

use gpui::{AnyElement, IntoElement, ParentElement, Styled, div, px};
use oximux_remote_host::HostConfig;
use oximux_settings::{Density, Theme, Typography};

use super::SettingsModal;
use super::controls::{stepper, toggle_switch};
use super::layout::{SettingEntry, card_surface, entries_card, entry, section_title};
use crate::remote_control::RemoteControl;

/// The host's config right now, or the defaults before the core is installed.
fn current(cx: &gpui::App) -> HostConfig {
    cx.try_global::<RemoteControl>()
        .and_then(|rc| rc.host_core())
        .map_or_else(HostConfig::default, |core| core.config())
}

/// Store `next` in the settings keys and hand it to the running core.
fn apply(modal: &SettingsModal, next: HostConfig, cx: &mut gpui::Context<SettingsModal>) {
    let next = next.sanitized();
    let confinement_switched_on = next.confine_chat && !current(cx).confine_chat;
    for (key, value) in next.entries() {
        if let Err(err) = modal.notify_repo.set(key, &value) {
            tracing::warn!(%err, key, "agent delegation setting not saved");
        }
    }
    if let Some(core) = cx.try_global::<RemoteControl>().and_then(|rc| rc.host_core()) {
        core.set_config(next);
    }
    if confinement_switched_on {
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
            let mut next = current(cx);
            set(&mut next, value.saturating_sub(1).max(1));
            apply(this, next, cx);
        },
        move |this: &mut SettingsModal, _w, cx| {
            let mut next = current(cx);
            set(&mut next, (value + 1).min(max));
            apply(this, next, cx);
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
            let mut next = current(cx);
            next.delegation_enabled = !next.delegation_enabled;
            apply(this, next, cx);
        },
        cx,
    );
    let confine = toggle_switch(
        "delegation-confine-chat",
        config.confine_chat,
        theme,
        |this: &mut SettingsModal, _w, cx| {
            let mut next = current(cx);
            next.confine_chat = !next.confine_chat;
            apply(this, next, cx);
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
