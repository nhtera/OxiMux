//! CommitDialog — modal that gathers subject + body + conventional prefix,
//! assembles the full commit message, and dispatches `Repository::commit()`.
//!
//! Composition (vertical):
//! ```text
//!  Subject  [prefix▾]  [text input ...........]  (✱ over 50 if applicable)
//!  Body     [multi-line text input ............]
//!                                  [Cancel]  [Commit]
//! ```
//!
//! State machine:
//!   Editing                       ← user typing
//!   Committing                    ← git commit in flight
//!   Failed { error }              ← last commit failed (stays open)
//!   Done   { sha }                ← commit landed; caller closes
//!
//! Runtime: `submit()` follows the same `Handle::try_current` + log+no-op
//! fallback pattern as `git_panel::spawn_repo_op`. Result delivery uses a
//! `oneshot` channel awaited via `cx.spawn`, mirroring `DiffView::load`.

pub mod prefix;

use crate::shell::commit_dialog::prefix::{
    assemble_message, next_prefix_index, prefix_label, subject_over_warn,
};
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Render, Styled, Task, Window, div, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::oneshot;

#[derive(Debug)]
pub enum CommitDialogState {
    Editing,
    Committing,
    Failed { error: String },
    Done { sha: String },
}

pub struct CommitDialog {
    repo: Repository,
    state: CommitDialogState,
    subject_state: Entity<InputState>,
    body_state: Entity<InputState>,
    prefix_idx: usize,
    focus_handle: FocusHandle,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Drop cancels the in-flight commit task (mirrors DiffView pattern).
    _commit_task: Option<Task<()>>,
}

impl CommitDialog {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subject_state =
            cx.new(|cx| InputState::new(window, cx).placeholder("Subject (50 char soft limit)"));
        let body_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Body (optional)")
        });
        Self {
            repo,
            state: CommitDialogState::Editing,
            subject_state,
            body_state,
            prefix_idx: 0,
            focus_handle: cx.focus_handle(),
            theme,
            density,
            typography,
            _commit_task: None,
        }
    }

    pub fn state(&self) -> &CommitDialogState {
        &self.state
    }

    pub fn prefix_idx(&self) -> usize {
        self.prefix_idx
    }

    pub fn cycle_prefix(&mut self) {
        self.prefix_idx = next_prefix_index(self.prefix_idx);
    }

    pub fn submit(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, CommitDialogState::Committing) {
            return;
        }
        let subject = self.subject_state.read(cx).value().to_string();
        let body = self.body_state.read(cx).value().to_string();
        let Some(message) = assemble_message(self.prefix_idx, &subject, &body) else {
            self.state = CommitDialogState::Failed {
                error: "subject is empty".to_string(),
            };
            return;
        };
        self.state = CommitDialogState::Committing;
        let repo = self.repo.clone();
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let r = repo.commit(&message).await.map_err(|e| e.to_string());
                    let _ = tx.send(r);
                });
            }
            Err(_) => {
                tracing::warn!(
                    target: "oximux_app::commit_dialog",
                    "no tokio runtime entered; commit skipped (step 14 wires runtime)"
                );
                self.state = CommitDialogState::Failed {
                    error: "no tokio runtime".to_string(),
                };
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |dlg, cx| {
                dlg.apply_commit_result(result);
                cx.notify();
            });
        });
        self._commit_task = Some(task);
    }

    fn apply_commit_result(&mut self, result: Result<String, String>) {
        match result {
            Ok(sha) => self.state = CommitDialogState::Done { sha },
            Err(error) => self.state = CommitDialogState::Failed { error },
        }
    }
}

impl Focusable for CommitDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommitDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let subject_text = self.subject_state.read(cx).value().to_string();
        let over_warn = subject_over_warn(&subject_text);
        let prefix_button = self.render_prefix_button(cx);
        let warn_chip = self.render_warn_chip(over_warn);
        let status_row = self.render_status_row();
        let submit_disabled = matches!(self.state, CommitDialogState::Committing);

        div()
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .w(px(560.0))
            .p(px(density.pad_panel * 2.0))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .gap(px(density.gap_inline))
            .child(label_row(
                "Commit",
                "Stage your changes then assemble the message below.",
                theme,
                typography,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(density.gap_inline))
                    .items_center()
                    .child(prefix_button)
                    .child(div().flex_1().child(Input::new(&self.subject_state)))
                    .child(warn_chip),
            )
            .child(Input::new(&self.body_state).h(px(160.0)))
            .child(status_row)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(density.gap_inline))
                    .child(
                        Button::new("commit-button")
                            .primary()
                            .label(if submit_disabled {
                                "Committing…"
                            } else {
                                "Commit"
                            })
                            .disabled(submit_disabled)
                            .on_click(cx.listener(|dlg, _: &ClickEvent, _window, cx| {
                                dlg.submit(cx);
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl CommitDialog {
    fn render_prefix_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = prefix_label(self.prefix_idx);
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();
        div()
            .flex()
            .items_center()
            .justify_center()
            .h(px(density.h_tab))
            .px(px(density.pad_panel))
            .min_w(px(110.0))
            .bg(theme.bg_panel_alt)
            .border_1()
            .border_color(theme.border_inactive)
            .rounded(px(density.r_xs))
            .text_size(px(typography.t_body_sm))
            .text_color(theme.fg_base)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|dlg, _: &MouseDownEvent, _window, cx| {
                    dlg.cycle_prefix();
                    cx.notify();
                }),
            )
            .child(format!("{label} ▾"))
    }

    fn render_warn_chip(&self, over_warn: bool) -> impl IntoElement {
        let theme = self.theme;
        let typography = &self.typography;
        div()
            .min_w(px(20.0))
            .text_size(px(typography.t_label_caps))
            .text_color(if over_warn {
                theme.status_warn
            } else {
                theme.fg_subtle
            })
            .child(if over_warn { "✱ >50" } else { "" })
    }

    fn render_status_row(&self) -> impl IntoElement {
        let (color, msg) = match &self.state {
            CommitDialogState::Editing => (self.theme.fg_subtle, String::new()),
            CommitDialogState::Committing => (self.theme.fg_muted, "Committing…".to_string()),
            CommitDialogState::Failed { error } => {
                (self.theme.status_error, format!("Commit failed: {error}"))
            }
            CommitDialogState::Done { sha } => (
                self.theme.status_ok,
                format!("Committed {}", &sha[..sha.len().min(7)]),
            ),
        };
        div()
            .text_size(px(self.typography.t_body_sm))
            .text_color(color)
            .child(msg)
    }
}

fn label_row(
    title: &str,
    sub: &str,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(px(typography.t_body_md))
                .font_weight(typography.w_semibold)
                .text_color(theme.fg_base)
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(sub.to_string()),
        )
}

