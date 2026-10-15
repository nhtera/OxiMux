//! Status bar — 24px fixed-height bottom strip.
//!
//! Layout: `left | center | right`. Left zone shows brand + version. Center
//! shows the git branch chip + dirty count when a repository is mounted,
//! plus a compact primary-action button when an SCM panel is active. Right
//! zone shows a metric strip: `N ports | N TTY | N agents | N panes`.
//!
//! The ports segment is the one metric that comes and goes. It is a *signal*,
//! not a count of something always present — "something you started is
//! serving" is worth a permanent place in the eye's path only while it is
//! true, and the Ports tab in the activity bar is the affordance that is
//! always there. Unlike its neighbours it is clickable, because a metric that
//! only appears when there is something to look at should take you to it.
//!
//! The TTY count is clickable too: it opens the terminal daemon's card
//! (status, Restart, Kill all) — the daemon is what those terminals run in.
//!
//! The keep-awake chip leads the right zone — coffee icon, mode, state dot —
//! and opens its card to change the mode or see what else holds the machine
//! awake. Absent only on a platform with no sleep backend behind it.
//!
//! Pure helpers (`tty_label`, `agent_label`, `pane_label`, `metric_color`,
//! `primary_button_visible`, `ports_segment_visible`) drive the visible
//! labels; tested without GPUI.

use gpui::{
    App, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::Icon;
use oximux_agents::session_log::usage::ProviderUsage;
use oximux_core::GitState;
use oximux_git::PollState;
use oximux_settings::{Density, Theme, Typography, UsageDetail};

use super::awake_card;
use crate::agent_awake::AwakeStatus;
use crate::shell::source_control::primary_action::PrimaryAction;
use crate::shell::usage_meter;

/// The phone-viewing chip's icon.
const PHONE_VIEW_ICON: &str = "icons/eye.svg";

/// Which card a status-bar chip opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPopoverKind {
    /// The usage meter's accounts.
    Usage,
    /// The terminal daemon: status, Restart, Kill all.
    Daemon,
    /// Keep computer awake: mode, state, and what else holds it.
    Awake,
}

/// Pure helper for the git zone text. Returns:
///   - `"<branch>  •  N changed"` (or `0 changed`) when Ready
///   - `"<branch>  •  ↑A ↓B  •  N changed"` when ahead/behind
///   - `"loading git…"` when Loading
///   - `"git: <err>"` when Failed
///   - `""` when no repo
pub fn git_zone_label(state: Option<&PollState>) -> String {
    match state {
        None => String::new(),
        Some(PollState::Loading) => "loading git…".to_string(),
        Some(PollState::Failed(e)) => format!("git: {e}"),
        Some(PollState::Ready(g)) => format_ready(g),
    }
}

fn format_ready(g: &GitState) -> String {
    let branch = g.branch.as_deref().unwrap_or("(detached)");
    let dirty = g.files.iter().filter(|f| !is_ignored(f)).count();
    let mut s = String::from(branch);
    if g.ahead != 0 || g.behind != 0 {
        s.push_str(&format!("  •  ↑{} ↓{}", g.ahead, g.behind));
    }
    s.push_str(&format!("  •  {dirty} changed"));
    s
}

fn is_ignored(f: &oximux_core::FileStatus) -> bool {
    matches!(f.index, oximux_core::IndexStatus::Ignored)
        || matches!(f.worktree, oximux_core::WorktreeStatus::Ignored)
}

/// `"{n} TTY"` — always plural-stable since "TTY" is an abbreviation.
pub fn tty_label(count: usize) -> String {
    format!("{count} TTY")
}

/// `"1 agent"` / `"N agents"`. Plural-aware.
pub fn agent_label(count: usize) -> String {
    match count {
        1 => "1 agent".to_string(),
        n => format!("{n} agents"),
    }
}

/// `"1 pane"` / `"N panes"`. Plural-aware.
pub fn pane_label(count: usize) -> String {
    match count {
        1 => "1 pane".to_string(),
        n => format!("{n} panes"),
    }
}

/// Recessive `fg_subtle` when below threshold; `fg_muted` when active.
/// Used to dim "0 agents" while keeping live counts readable.
pub fn metric_color(count: usize, active_threshold: usize, theme: Theme) -> Hsla {
    if count >= active_threshold {
        theme.fg_muted
    } else {
        theme.fg_subtle
    }
}

fn separator(theme: Theme, typography: &Typography) -> impl IntoElement {
    div()
        .text_size(px(typography.t_body_sm))
        .text_color(theme.fg_subtle)
        .child(" | ")
}

/// Whether the clickable ports segment belongs in the metric strip.
///
/// Zero is hidden rather than dimmed. The neighbouring metrics describe things
/// the window always has some number of; a listening port is an event, and a
/// permanent "0 ports" would be the only segment in the strip that is usually
/// reporting nothing happened.
pub fn ports_segment_visible(count: usize) -> bool {
    count > 0
}

/// Returns `true` when the primary-action button should be rendered in the
/// git zone: requires both a mounted repo (git_state present) and a resolved
/// primary action from the SCM panel.
pub fn primary_button_visible(
    git_state: Option<&PollState>,
    primary: Option<&PrimaryAction>,
) -> bool {
    git_state.is_some() && primary.is_some()
}

#[allow(clippy::too_many_arguments)]
pub fn view<F, G, H, P, T, A>(
    theme: Theme,
    density: Density,
    typography: &Typography,
    pane_count: usize,
    tty_count: usize,
    agent_count: usize,
    // Listening ports attributed to this window's terminals. Hidden at zero —
    // see `ports_segment_visible`.
    port_count: usize,
    git_state: Option<&PollState>,
    primary: Option<PrimaryAction>,
    usage: &[ProviderUsage],
    // How much each account's segment spells out — the user's Appearance choice.
    usage_detail: UsageDetail,
    // Passed in rather than read here so the whole bar renders from one instant
    // and the pure formatting stays testable.
    now_ms: i64,
    // Version of a staged update awaiting restart, if any.
    update_ready: Option<String>,
    // `None` where holds never reach the OS — the chip is then absent.
    awake: Option<AwakeStatus>,
    // "Viewed from …" while a paired phone watches a device; a click stops
    // sharing. Absent otherwise.
    phone_view: Option<String>,
    on_primary_click: F,
    on_usage_click: G,
    on_update_click: H,
    on_ports_click: P,
    on_tty_click: T,
    on_awake_click: A,
) -> impl IntoElement
where
    F: Fn(&mut Window, &mut App) + 'static,
    G: Fn(&mut Window, &mut App) + 'static,
    H: Fn(&mut Window, &mut App) + 'static,
    P: Fn(&mut Window, &mut App) + 'static,
    T: Fn(&mut Window, &mut App) + 'static,
    A: Fn(&mut Window, &mut App) + 'static,
{
    let git_label = git_zone_label(git_state);
    let show_primary = primary_button_visible(git_state, primary.as_ref());

    // Build the center git zone: branch label + optional primary-action button.
    let git_zone = {
        let mut zone = div()
            .flex()
            .flex_1()
            .justify_center()
            .items_center()
            .gap(px(6.))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .child(git_label);

        if show_primary
            && let Some(action) = primary
        {
            let label = action.label.clone();
            let title = action.title.clone();
            let disabled = action.disabled;
            let fg = if disabled { theme.fg_subtle } else { theme.fg_muted };
            let hover_bg = theme.hover_overlay;
            let btn = div()
                .id("status-bar-git-primary")
                .flex()
                .items_center()
                .h(px(16.))
                .px(px(6.))
                .rounded(px(density.r_chip))
                .text_size(px(typography.t_body_sm))
                .text_color(fg)
                .border_1()
                .border_color(if disabled {
                    theme.border_inactive
                } else {
                    theme.border_active
                })
                .child(label)
                // Surface the resolver's context (commit counts, or the
                // reason the next step is unavailable) on hover.
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
                });

            // Wire click only when enabled.
            let btn = if disabled {
                btn
            } else {
                btn.cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .on_mouse_down(
                        MouseButton::Left,
                        move |_: &MouseDownEvent, window, cx| {
                            on_primary_click(window, cx);
                        },
                    )
            };
            zone = zone.child(btn);
        }
        zone
    };

    // Usage meter — one segment per configured account, each its own icon and
    // numbers, sharing a single click target so they open one card rather than
    // several. Absent entirely until the first sample lands, and on a machine
    // with no agent account set up.
    let segments = usage_meter::meter_segments(usage, usage_detail, now_ms, theme);
    let usage_chip = (!segments.is_empty()).then(|| {
        let hover_bg = theme.hover_overlay;
        let mut chip = div()
            .id("status-bar-usage-meter")
            .flex()
            .items_center()
            .gap(px(8.))
            .h(px(16.))
            .px(px(4.))
            .rounded(px(density.r_chip))
            .text_size(px(typography.t_body_sm))
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            // No hover tooltip: an in-window element can't composite above the
            // inline browser's native surface, so a tooltip here is occluded by
            // an active page. The chip is clearly clickable and opens a floating
            // detail card (its own higher-level window), which carries the info.
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_usage_click(window, cx);
            });
        for segment in segments {
            chip = chip.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .text_color(segment.color)
                    // The icon carries the account's identity so the numbers
                    // don't have to spell out whose they are.
                    .child(
                        Icon::default()
                            .path(segment.icon_path)
                            .size(px(11.))
                            .text_color(segment.color),
                    )
                    .child(segment.label),
            );
        }
        chip
    });

    // The only place a pending update touches the main window. Passive on
    // purpose: it states a fact and opens the pane that explains it, rather
    // than restarting anything on a stray click — an accidental restart would
    // interrupt whatever the user's agents are mid-way through.
    let update_pill = update_ready.map(|version| {
        let hover_bg = theme.hover_overlay;
        div()
            .id("status-bar-update-ready")
            .flex()
            .items_center()
            .h(px(16.))
            .px(px(6.))
            .rounded(px(density.r_chip))
            .bg(theme.status_ok.alpha(0.12))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.status_ok)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_update_click(window, cx);
            })
            .child(format!("v{version} ready — restart to update"))
    });

    // Clickable, unlike its neighbours: it appears exactly when there is
    // something to go and look at, so it opens the panel that shows it.
    let ports_chip = ports_segment_visible(port_count).then(|| {
        let hover_bg = theme.hover_overlay;
        div()
            .id("status-bar-ports")
            .flex()
            .items_center()
            .h(px(16.))
            .px(px(4.))
            .rounded(px(density.r_chip))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_ports_click(window, cx);
            })
            .child(crate::shell::ports_panel::labels::port_metric_label(
                port_count,
            ))
    });

    // Leads the right zone, and never shrinks: a mode that is partly clipped
    // reads as a different mode.
    let awake_chip = awake.map(|status| {
        let hover_bg = theme.hover_overlay;
        let (icon_color, dot_color) = if status.asserted {
            (theme.fg_base, theme.fg_base)
        } else {
            (theme.fg_muted, theme.fg_subtle)
        };
        let tip = awake_card::tooltip_text(&status);
        div()
            .id("status-bar-awake")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(16.))
            .px(px(4.))
            .rounded(px(density.r_chip))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_muted)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_awake_click(window, cx);
            })
            .child(Icon::default().path(awake_card::ICON).size(px(11.)).text_color(icon_color))
            .child(crate::awake_settings::label(status.mode))
            .child(div().size(px(6.)).rounded_full().bg(dot_color))
    });

    // Leads everything while it lasts: a device's screen leaving the
    // machine is the one state here the user must never miss.
    let phone_chip = phone_view.map(|label| {
        let hover_bg = theme.hover_overlay;
        div()
            .id("status-bar-phone-view")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.))
            .h(px(16.))
            .px(px(6.))
            .rounded(px(density.r_chip))
            .bg(theme.status_warn.alpha(0.14))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.status_warn)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new("A paired phone is watching a device. Click to stop sharing.").build(window, cx)
            })
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                crate::remote_control::sim_view::stop_sharing(cx);
            })
            .child(Icon::default().path(PHONE_VIEW_ICON).size(px(11.)).text_color(theme.status_warn))
            .child(label)
            .child(div().text_color(theme.fg_muted).child("· Stop"))
    });

    div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_status_bar))
        .px(px(density.pad_panel))
        .bg(theme.bg_panel)
        .border_t_1()
        .border_color(theme.border_inactive)
        .child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .gap(px(6.))
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_subtle)
                .child(format!("OxiMux v{}", env!("CARGO_PKG_VERSION")))
                .children(update_pill),
        )
        .child(git_zone)
        .child(
            div()
                .flex()
                .flex_1()
                .justify_end()
                .items_center()
                .gap(px(2.))
                .children(phone_chip.map(|chip| {
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .child(chip)
                        .child(separator(theme, typography))
                }))
                .children(awake_chip.map(|chip| {
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .child(chip)
                        .child(separator(theme, typography))
                }))
                .children(usage_chip.map(|chip| {
                    div()
                        .flex()
                        .items_center()
                        .child(chip)
                        .child(separator(theme, typography))
                }))
                .children(ports_chip.map(|chip| {
                    div()
                        .flex()
                        .items_center()
                        .child(chip)
                        .child(separator(theme, typography))
                }))
                .child({
                    let hover_bg = theme.hover_overlay;
                    div()
                        .id("status-bar-tty")
                        .flex()
                        .items_center()
                        .h(px(16.))
                        .px(px(4.))
                        .rounded(px(density.r_chip))
                        .text_size(px(typography.t_body_sm))
                        .text_color(metric_color(tty_count, 1, theme))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover_bg))
                        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                            on_tty_click(window, cx);
                        })
                        .child(tty_label(tty_count))
                })
                .child(separator(theme, typography))
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(metric_color(agent_count, 1, theme))
                        .child(agent_label(agent_count)),
                )
                .child(separator(theme, typography))
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(theme.fg_muted)
                        .child(pane_label(pane_count)),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ports_segment_appears_only_when_something_is_serving() {
        assert!(!ports_segment_visible(0), "an idle window keeps a quiet strip");
        assert!(ports_segment_visible(1));
        assert!(ports_segment_visible(9));
    }

    #[test]
    fn tty_label_singular() {
        assert_eq!(tty_label(1), "1 TTY");
    }

    #[test]
    fn tty_label_plural() {
        assert_eq!(tty_label(3), "3 TTY");
    }

    #[test]
    fn tty_label_zero() {
        assert_eq!(tty_label(0), "0 TTY");
    }

    #[test]
    fn agent_label_zero() {
        assert_eq!(agent_label(0), "0 agents");
    }

    #[test]
    fn agent_label_singular() {
        assert_eq!(agent_label(1), "1 agent");
    }

    #[test]
    fn agent_label_plural() {
        assert_eq!(agent_label(2), "2 agents");
    }

    #[test]
    fn pane_label_singular() {
        assert_eq!(pane_label(1), "1 pane");
    }

    #[test]
    fn pane_label_plural() {
        assert_eq!(pane_label(4), "4 panes");
    }

    #[test]
    fn metric_color_active_returns_fg_muted() {
        let t = Theme::charcoal();
        assert_eq!(metric_color(1, 1, t), t.fg_muted);
    }

    #[test]
    fn metric_color_inactive_returns_fg_subtle() {
        let t = Theme::charcoal();
        assert_eq!(metric_color(0, 1, t), t.fg_subtle);
    }

    #[test]
    fn primary_button_hidden_when_no_repo() {
        use crate::shell::source_control::primary_action::{
            PrimaryAction, PrimaryActionKind,
        };
        let action = PrimaryAction {
            kind: PrimaryActionKind::Stage,
            label: "Stage All".into(),
            title: "Stage all changes".into(),
            disabled: false,
        };
        assert!(!primary_button_visible(None, Some(&action)));
    }

    #[test]
    fn primary_button_hidden_when_no_action() {
        let state = oximux_git::PollState::Loading;
        assert!(!primary_button_visible(Some(&state), None));
    }

    #[test]
    fn primary_button_shown_when_repo_and_action_present() {
        use crate::shell::source_control::primary_action::{
            PrimaryAction, PrimaryActionKind,
        };
        let action = PrimaryAction {
            kind: PrimaryActionKind::Push,
            label: "Push".into(),
            title: "Push 1 commit".into(),
            disabled: false,
        };
        let state = oximux_git::PollState::Loading;
        assert!(primary_button_visible(Some(&state), Some(&action)));
    }
}
