//! Under a real iPhone's toolbar: whether OxiMux controls it, and the way to
//! turn control on — the signing team the runner is built with (the user's
//! choice, never guessed), what building it does to their account, the
//! build's progress, and what went wrong.

use gpui::{AnyElement, Context, Div, IntoElement, ParentElement as _, SharedString, Styled as _, div, px};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
};
use oximux_simulator::DeviceId;

use super::SimulatorPanel;
use crate::shell::simulator::hub::{ControlState, TargetApp, TeamChoice};

/// What building the runner does, said before the user picks a team.
const DISCLOSURE: &str = "OxiMux builds a small test runner on this Mac and runs it on the iPhone, signed with the team you pick. \
Building registers this iPhone with that team and creates its App IDs (dev.oximux.runner.t<team>, and Xcode's dev.oximux.runner.t<team>.uitests.xctrunner) in the team's account.";
const NO_TEAMS: &str = "No Apple Development certificate on this Mac. Add your Apple ID in Xcode › Settings › Accounts, then check again.";

impl SimulatorPanel {
    /// The control row under a streaming iPhone.
    pub(super) fn render_iphone_control(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) else { return div().into_any_element() };
        let state = hub.read(cx).control_state(&udid);
        if state != ControlState::Off {
            self.control_setup = false;
        }
        let (theme, ty) = (self.theme, self.typography.clone());
        match state {
            ControlState::Off if self.control_setup => self.render_team_picker(cx),
            ControlState::Off => self
                .row()
                .child(self.line("View only", theme.fg_muted, ty.t_body_sm))
                .child(self.action("sim-control-on", "Control from OxiMux…", cx, |this, cx| {
                    this.control_setup = true;
                    cx.notify();
                }))
                .into_any_element(),
            ControlState::Building(line) => self
                .column()
                .child(
                    self.row()
                        .child(self.line("Building the control runner…", theme.fg_base, ty.t_body_sm))
                        .child(self.action("sim-control-cancel", "Cancel", cx, Self::turn_control_off)),
                )
                .child(self.line(line, theme.fg_subtle, ty.t_sub_label))
                .into_any_element(),
            ControlState::Starting => self
                .row()
                .child(self.line("Starting control on the iPhone…", theme.fg_base, ty.t_body_sm))
                .child(self.action("sim-control-cancel", "Cancel", cx, Self::turn_control_off))
                .into_any_element(),
            ControlState::On { busy } => self
                .row()
                .child(self.line(if busy { "Controlling · working…" } else { "Controlling" }, theme.fg_muted, ty.t_body_sm))
                .child(self.target_picker(&udid, cx))
                .child(self.action("sim-control-off", "Turn off", cx, Self::turn_control_off))
                .into_any_element(),
            ControlState::Failed(message) => self
                .column()
                .child(self.line(message, theme.status_warn, ty.t_body_sm))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(self.density.pad_panel))
                        .child(self.action("sim-control-retry", "Retry", cx, |this, cx| {
                            if let (Some(hub), Some(udid)) = (this.hub.clone(), this.device(cx)) {
                                hub.update(cx, |hub, cx| {
                                    if let Some(team) = hub.chosen_team() {
                                        hub.enable_control(&udid, &team, cx);
                                    }
                                });
                            }
                        }))
                        .child(self.action("sim-control-off", "Turn off", cx, Self::turn_control_off)),
                )
                .into_any_element(),
        }
    }

    /// Which app gestures and typing address: the home screen, or one of
    /// the phone's apps (listed when the menu opens).
    fn target_picker(&self, udid: &DeviceId, cx: &mut Context<Self>) -> AnyElement {
        let Some(hub) = self.hub.clone() else { return div().into_any_element() };
        let current = hub.read(cx).control_target(udid);
        let label = current.as_ref().map_or_else(|| "Home screen".to_owned(), |a| a.name.clone());
        let (udid, menu_hub) = (udid.clone(), hub.downgrade());
        Button::new("sim-control-target")
            .ghost()
            .small()
            .label(SharedString::from(label))
            .icon(Icon::default().path("icons/chevron-down.svg"))
            .tooltip("The app taps, typing and the accessibility tree address")
            .dropdown_menu(move |menu, _window, cx| {
                let Some(hub) = menu_hub.upgrade() else { return menu };
                let apps = hub.update(cx, |hub, cx| hub.phone_apps(&udid, cx));
                let pick = |app: Option<TargetApp>| {
                    let (hub, udid) = (hub.downgrade(), udid.clone());
                    move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut gpui::App| {
                        let _ = hub.update(cx, |hub, cx| hub.set_control_target(&udid, app.clone(), cx));
                    }
                };
                let mut menu = menu.item(PopupMenuItem::new("Home screen").checked(current.is_none()).on_click(pick(None)));
                let apps = match apps {
                    None => return menu.label("Listing the phone's apps… (reopen in a moment)"),
                    Some(Err(_)) => return menu.label("Could not list the phone's apps (unlock it, then reopen)"),
                    Some(Ok(apps)) if apps.is_empty() => return menu.label("No apps of yours on the phone"),
                    Some(Ok(apps)) => apps,
                };
                menu = menu.separator();
                for app in apps {
                    let checked = current.as_ref().is_some_and(|c| c.bundle_id == app.bundle_id);
                    menu = menu.item(PopupMenuItem::new(SharedString::from(app.name.clone())).checked(checked).on_click(pick(Some(app))));
                }
                menu
            })
            .into_any_element()
    }

    /// The setup card: what building does, then one button per team.
    fn render_team_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(hub) = self.hub.clone() else { return div().into_any_element() };
        let (theme, ty) = (self.theme, self.typography.clone());
        let teams = hub.update(cx, |hub, cx| hub.signing_teams(cx));
        let chosen = hub.read(cx).chosen_team();
        let mut card = self.column().child(self.line(DISCLOSURE, theme.fg_muted, ty.t_body_sm));
        card = match teams {
            None => card.child(self.line("Looking for signing teams…", theme.fg_subtle, ty.t_body_sm)),
            Some(Err(why)) => card.child(self.line(why, theme.status_warn, ty.t_body_sm)),
            Some(Ok(teams)) if teams.is_empty() => card
                .child(self.line(NO_TEAMS, theme.fg_base, ty.t_body_sm))
                .child(self.action("sim-control-teams-again", "Check again", cx, |this, cx| {
                    if let Some(hub) = this.hub.clone() {
                        hub.update(cx, |hub, cx| hub.load_signing_teams(cx));
                    }
                })),
            Some(Ok(teams)) => card
                .child(self.line("Build and sign it with:", theme.fg_base, ty.t_body_sm))
                .children(teams.into_iter().enumerate().map(|(i, team)| self.team_button(i, team, chosen.as_deref(), cx))),
        };
        card.child(self.action("sim-control-setup-cancel", "Not now", cx, |this, cx| {
            this.control_setup = false;
            cx.notify();
        }))
        .into_any_element()
    }

    fn team_button(&self, index: usize, team: TeamChoice, chosen: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let label = format!("{} ({}){}", team.name, team.id, if team.personal { " · Personal Team" } else { "" });
        let button = Button::new(("sim-control-team", index)).small().label(SharedString::from(label));
        let button = if chosen == Some(team.id.as_str()) { button.primary() } else { button.outline() };
        button
            .on_click(cx.listener(move |this, _, _window, cx| {
                if let (Some(hub), Some(udid)) = (this.hub.clone(), this.device(cx)) {
                    hub.update(cx, |hub, cx| hub.enable_control(&udid, &team.id, cx));
                }
                this.control_setup = false;
                cx.notify();
            }))
            .into_any_element()
    }

    fn turn_control_off(&mut self, cx: &mut Context<Self>) {
        if let (Some(hub), Some(udid)) = (self.hub.clone(), self.device(cx)) {
            hub.update(cx, |hub, cx| hub.disable_control(&udid, cx));
        }
    }

    fn row(&self) -> Div {
        let (theme, density) = (self.theme, self.density);
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(density.pad_panel))
            .w_full()
            .px(px(density.pad_panel))
            .py(px(density.pad_row * 0.5))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel_alt)
    }

    fn column(&self) -> Div {
        let (theme, density) = (self.theme, self.density);
        div()
            .flex()
            .flex_col()
            .gap(px(density.pad_row * 0.5))
            .w_full()
            .px(px(density.pad_panel))
            .py(px(density.pad_row * 0.5))
            .rounded(px(density.r_card))
            .bg(theme.bg_panel_alt)
    }

    fn line(&self, text: impl Into<SharedString>, color: gpui::Hsla, size: f32) -> Div {
        div().min_w_0().flex_1().text_size(px(size)).text_color(color).child(text.into())
    }

    fn action(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        Button::new(id)
            .ghost()
            .small()
            .label(label)
            .on_click(cx.listener(move |this, _, _window, cx| on_click(this, cx)))
            .into_any_element()
    }
}
