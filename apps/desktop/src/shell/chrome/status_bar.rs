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
//! Pure helpers (`tty_label`, `agent_label`, `pane_label`, `metric_color`,
//! `primary_button_visible`, `ports_segment_visible`) drive the visible
//! labels; tested without GPUI.

use gpui::{
    App, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use oximux_agents::session_log::usage::UsageState;
use oximux_core::GitState;
use oximux_git::PollState;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::source_control::primary_action::PrimaryAction;
use crate::shell::usage_meter;

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
pub fn view<F, G, H, P>(
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
    usage: Option<&UsageState>,
    // Version of a staged update awaiting restart, if any.
    update_ready: Option<String>,
    on_primary_click: F,
    on_usage_click: G,
    on_update_click: H,
    on_ports_click: P,
) -> impl IntoElement
where
    F: Fn(&mut Window, &mut App) + 'static,
    G: Fn(&mut Window, &mut App) + 'static,
    H: Fn(&mut Window, &mut App) + 'static,
    P: Fn(&mut Window, &mut App) + 'static,
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

    // Usage meter — present once the probe has produced a state. An available
    // reading shows exact `NN% 5h · NN% wk`; an unavailable one shows a
    // warn-colored "Usage unavailable" chip. The click popover carries the
    // window detail or the failure reason.
    let usage_chip = usage.map(|state| {
        let (label, color) = match state {
            UsageState::Available(snapshot) => (
                usage_meter::meter_label(snapshot),
                usage_meter::meter_color(snapshot, theme),
            ),
            UsageState::Unavailable { .. } => {
                (usage_meter::UNAVAILABLE_LABEL.to_string(), theme.status_warn)
            }
        };
        let hover_bg = theme.hover_overlay;
        div()
            .id("status-bar-usage-meter")
            .flex()
            .items_center()
            .h(px(16.))
            .px(px(4.))
            .rounded(px(density.r_chip))
            .text_size(px(typography.t_body_sm))
            .text_color(color)
            .cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            // No hover tooltip: an in-window element can't composite above the
            // inline browser's native surface, so a tooltip here is occluded by
            // an active page. The chip is clearly clickable and opens a floating
            // detail card (its own higher-level window), which carries the info.
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_usage_click(window, cx);
            })
            .child(label)
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
                .child(
                    div()
                        .text_size(px(typography.t_body_sm))
                        .text_color(metric_color(tty_count, 1, theme))
                        .child(tty_label(tty_count)),
                )
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
