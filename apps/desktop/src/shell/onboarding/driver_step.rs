//! Onboarding driver step: offer the one-click computer-use driver install.
//!
//! Shown only when the driver is missing or stale at wizard open (see
//! `driver_step_needed`). Reuses the shared install state machine
//! (`crate::shell::driver_install`) — the settings pane drives the identical
//! states, and the backend's process-wide lock keeps both surfaces honest if
//! they ever run at once.

use gpui::{Context, InteractiveElement as _, ParentElement as _, Styled as _, div, px};
use gpui_component::button::{Button, ButtonVariants as _};

use crate::shell::driver_install::{self, DriverInstallUi};
use crate::shell::settings_modal::DriverStatus;

use super::OnboardingWizard;

impl OnboardingWizard {
    /// Step body: heading + current status + the install affordance. Purely
    /// optional — Finish works regardless of what happens here.
    pub(super) fn render_driver_step(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = self.theme;
        let typography = self.typography.clone();

        let status: gpui::Div = match &self.driver_install_ui {
            DriverInstallUi::Running { stage } => div()
                .text_color(theme.fg_muted)
                .child(driver_install::stage_label(stage)),
            // Windows only in practice — macOS gates on the signature and
            // never parks here. Same wording as the settings pane's card.
            DriverInstallUi::AwaitingApproval { version, .. } => div()
                .text_color(theme.fg_muted)
                .child(format!("Driver {version} downloaded — publisher unverified")),
            DriverInstallUi::Failed { message } => {
                div().text_color(theme.status_warn).child(message.clone())
            }
            DriverInstallUi::Idle => {
                let colour = match self.driver_status {
                    DriverStatus::Ready { .. } => theme.status_ok,
                    _ => theme.fg_muted,
                };
                let mut line = div().text_color(colour).child(self.driver_status.summary());
                // Post-upgrade: the shared daemon is left running on purpose,
                // so old behavior can persist until it respawns.
                if self.driver_upgraded
                    && matches!(self.driver_status, DriverStatus::Ready { .. })
                {
                    line = line.child(
                        div()
                            .text_size(px(typography.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .child(driver_install::UPGRADE_NOTE),
                    );
                }
                line
            }
        };

        let mut controls = div().flex().items_center().justify_center().gap(px(10.0));
        match &self.driver_install_ui {
            // Cancel only when this wizard started the install — an observed
            // install (from settings) gets progress but no inert button.
            DriverInstallUi::Running { .. } if self.driver_install.is_some() => {
                controls = controls.child(
                    Button::new("onboarding-driver-cancel").ghost().label("Cancel").on_click(
                        cx.listener(|this, _, _window, cx| {
                            if let Some(handle) = &this.driver_install {
                                handle.cancel();
                            }
                            cx.notify();
                        }),
                    ),
                );
            }
            DriverInstallUi::Running { .. } => {}
            // The parked install needs the person's verdict; the poll loop is
            // still alive (`is_running`), so a decision resumes it in place.
            DriverInstallUi::AwaitingApproval { .. } => {
                controls = controls
                    .child(
                        Button::new("onboarding-driver-approve")
                            .primary()
                            .label("Approve and install")
                            .on_click(|_, _, _| driver_install::approve()),
                    )
                    .child(
                        Button::new("onboarding-driver-decline")
                            .ghost()
                            .label("Don't install")
                            .on_click(|_, _, _| driver_install::decline()),
                    );
            }
            _ => {
                if let Some(label) = self.driver_status.install_label() {
                    controls = controls.child(
                        Button::new("onboarding-driver-install").primary().label(label).on_click(
                            cx.listener(|this, _, _window, cx| this.start_driver_install(cx)),
                        ),
                    );
                }
                if matches!(self.driver_install_ui, DriverInstallUi::Failed { .. }) {
                    controls = controls.child(
                        Button::new("onboarding-driver-manual")
                            .ghost()
                            .label("Download manually")
                            .on_click(|_, _, cx| {
                                crate::shell::open_url::open_url(
                                    driver_install::MANUAL_DOWNLOAD_URL,
                                    cx,
                                );
                            }),
                    );
                }
            }
        }

        div()
            .flex()
            .flex_col()
            .child(div().h(px(26.0)))
            .child(super::view::step_heading(
                "Computer use (optional)",
                "Agents can drive other apps through a separate signed helper",
                theme,
                &typography,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(14.0))
                    .child(
                        div()
                            .text_size(px(typography.t_body_md))
                            .max_w(px(440.0))
                            .child(status),
                    )
                    .child(controls)
                    .child(
                        div()
                            .id("onboarding-driver-later")
                            .text_size(px(typography.t_body_sm))
                            .text_color(theme.fg_subtle)
                            .child("Downloaded from the official release and verified (publisher + notarization) before install."),
                    ),
            )
    }

    /// Kick off (or attach to) the install and start this wizard's poll loop.
    pub(super) fn start_driver_install(&mut self, cx: &mut Context<Self>) {
        if self.driver_install_ui.is_running() {
            return;
        }
        // Remembered before the status flips, so the stale-daemon note shows
        // exactly when an existing install was replaced.
        self.driver_upgraded = matches!(
            self.driver_status,
            DriverStatus::Ready { .. } | DriverStatus::Outdated { .. }
        );
        let (handle, ui) =
            driver_install::begin(crate::shell::agent_chat::computer_use::install_anchor());
        self.driver_install = handle;
        self.driver_install_ui = ui;
        self.spawn_driver_install_poll(cx);
        cx.notify();
    }

    /// Same pump-on-a-timer loop as the settings pane; terminal state
    /// re-resolves the status so the step (and `driver_step_needed`) reflect
    /// what is actually on disk.
    fn spawn_driver_install_poll(&mut self, cx: &mut Context<Self>) {
        if self.driver_poll_running || !self.driver_install_ui.is_running() {
            return;
        }
        self.driver_poll_running = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let done = this.update(cx, |this, cx| {
                    let done =
                        driver_install::pump(&mut this.driver_install, &mut this.driver_install_ui);
                    if done {
                        this.driver_poll_running = false;
                        this.driver_status = DriverStatus::resolve();
                    }
                    cx.notify();
                    done
                });
                if done.unwrap_or(true) {
                    break;
                }
            }
        })
        .detach();
    }
}
