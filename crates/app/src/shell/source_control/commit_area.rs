//! Inline commit composer mounted inside the Source Control panel.
//!
//! Single multi-line `Message` textarea plus a full-width primary action
//! button. The primary's label/icon adapts via the resolved `PrimaryAction`
//! (Commit / Stage Files / Push / Pull / Sync / Publish Branch).
//!
//! Single-flight: an `AtomicBool` guards re-entry so a fast double-click only
//! triggers one commit.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    Anchor, AppContext, ClickEvent, Context, Entity, FocusHandle, IntoElement, ParentElement,
    Styled, Task, Window, div, px,
};
use gpui_component::{
    Disableable, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants, DropdownButton},
    input::{Input, InputState},
    menu::PopupMenuItem,
};
use oximux_git::Repository;
use oximux_settings::{Density, Theme, Typography};
use tokio::sync::oneshot;

use crate::shell::source_control::primary_action::{PrimaryAction, PrimaryActionKind};
use crate::shell::source_control::style as sc_style;

/// Last error surfaced from a commit attempt. Reset on a fresh submit.
#[derive(Debug, Clone, Default)]
pub enum CommitStatus {
    #[default]
    Idle,
    Committing,
    Failed(String),
}

pub struct CommitArea {
    repo: Repository,
    pub message_state: Entity<InputState>,
    pub status: CommitStatus,
    in_flight: Arc<AtomicBool>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Drop cancels the in-flight commit observer task.
    _commit_task: Option<Task<()>>,
}

impl CommitArea {
    pub fn new(
        repo: Repository,
        theme: Theme,
        density: Density,
        typography: Typography,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .placeholder("Message")
        });
        Self {
            repo,
            message_state,
            status: CommitStatus::Idle,
            in_flight: Arc::new(AtomicBool::new(false)),
            theme,
            density,
            typography,
            _commit_task: None,
        }
    }

    /// True when the message field currently holds non-whitespace text.
    pub fn has_message(&self, cx: &gpui::App) -> bool {
        !self.message_state.read(cx).value().trim().is_empty()
    }

    /// Focus the message input. Called by Cmd+K routing.
    pub fn focus_subject(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.message_state.update(cx, |s, cx| s.focus(window, cx));
    }

    /// Sibling-callable focus handle for the message input.
    pub fn subject_focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        use gpui::Focusable;
        self.message_state.read(cx).focus_handle(cx)
    }

    /// Submit if the primary action says we can. Caller passes the resolved
    /// `PrimaryAction` so we only run when the rendered button would be
    /// enabled — keeps the keyboard path (Cmd+Enter, future binding) honest.
    pub fn submit(&mut self, action: &PrimaryAction, cx: &mut Context<Self>) {
        if action.disabled || action.kind != PrimaryActionKind::Commit {
            return;
        }
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return;
        }
        let message = self.message_state.read(cx).value().to_string();
        let trimmed = message.trim();
        if trimmed.is_empty() {
            self.in_flight.store(false, Ordering::SeqCst);
            self.status = CommitStatus::Failed("Message is empty".to_string());
            return;
        }
        let message = trimmed.to_string();
        self.status = CommitStatus::Committing;
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
                    target: "oximux_app::source_control",
                    "no tokio runtime entered; commit skipped"
                );
                self.status = CommitStatus::Failed("no tokio runtime".to_string());
                self.in_flight.store(false, Ordering::SeqCst);
                return;
            }
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |area, cx| {
                area.in_flight.store(false, Ordering::SeqCst);
                area.apply_result(result, cx);
                cx.notify();
            });
        });
        self._commit_task = Some(task);
    }

    fn apply_result(&mut self, result: Result<String, String>, _cx: &mut Context<Self>) {
        match result {
            Ok(_sha) => {
                self.status = CommitStatus::Idle;
                // `InputState::set_value` requires `&mut Window`, which isn't
                // available in this oneshot result callback. v1 trade-off:
                // leave the message in place after commit; the user clears
                // manually.
            }
            Err(error) => self.status = CommitStatus::Failed(error),
        }
    }

    pub fn render(&self, action: &PrimaryAction, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let density = self.density;
        let typography = &self.typography;
        let status_row = render_status_row(theme, typography, &self.status);
        let action_for_click = action.clone();
        let submit_label = action.label.clone();
        let submit_title = action.title.clone();
        let submit_disabled = action.disabled;
        let primary_icon = primary_icon_for(action.kind);

        // Relative wrapper anchors the sparkles overlay in the top-right;
        // backend lands later, so the button is disabled. Textarea sits on
        // `bg_base` (darker than the surrounding `bg_panel`) so it reads as
        // an inset field rather than a floating chip.
        //
        // Height is enforced on BOTH the wrapper (`.h(COMMIT_H)`) and the
        // inner Input (`.h_full()`); without the wrapper bound, the multi-
        // line Input would expand to the available column slack and the
        // composer would balloon to fill the panel.
        let textarea = div()
            .relative()
            .w_full()
            .h(px(sc_style::COMMIT_H))
            .flex_shrink_0()
            .border_1()
            .border_color(theme.border_inactive)
            .rounded(px(density.r_xs))
            .bg(theme.bg_base)
            .text_size(px(sc_style::TEXT))
            .child(Input::new(&self.message_state).h_full())
            .child(
                div().absolute().top(px(6.0)).right(px(6.0)).child(
                    Button::new("sc-ai-message")
                        .ghost()
                        .xsmall()
                        .icon(
                            Icon::default()
                                .path("icons/sparkles.svg")
                                .size(px(sc_style::ICON)),
                        )
                        .tooltip("Generate commit message (coming soon)")
                        .disabled(true),
                ),
            );

        let mut inner_button = Button::new("source-control-primary-inner")
            .label(submit_label)
            .tooltip(submit_title)
            .on_click(cx.listener(move |area, _: &ClickEvent, _window, cx| {
                area.submit(&action_for_click, cx);
                cx.notify();
            }))
            .flex_1();
        if let Some(icon) = primary_icon {
            inner_button = inner_button.icon(icon);
        }

        // Chevron opens a dropdown with every available remote/commit verb.
        // Today only "Commit" actually wires through to repo.commit; remote
        // verbs are present for visual completeness and ship disabled until
        // their backends land.
        let action_view = cx.entity();
        let action_for_menu = action.clone();
        // DropdownButton handles the shared borders + unified variant so the
        // chevron half can't render brighter than the disabled main half.
        // `.small()` brings the chunky default-size pill down to ~32px tall
        // so it sits proportional to the textarea above.
        let primary = DropdownButton::new("sc-commit-split")
            .button(inner_button)
            .primary()
            .small()
            .disabled(submit_disabled)
            .w_full()
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, _cx| {
                build_commit_menu(menu, window, &action_view, &action_for_menu)
            });

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w_full()
            .px(px(sc_style::PAD_H))
            .pt(px(sc_style::PAD_V))
            .pb(px(sc_style::PAD_V_TIGHT))
            .gap(px(sc_style::PAD_V_TIGHT))
            .child(textarea)
            .child(primary)
            .child(status_row)
    }
}

/// Chevron dropdown items: commit-with-followups, standalone remote verbs,
/// host-integrations. Only "Commit" actually dispatches today — `action.kind`
/// already encodes whether commit is viable (resolver checks has_message +
/// staged_count). Remote/PR verbs ship disabled until their backends land.
fn build_commit_menu(
    menu: gpui_component::menu::PopupMenu,
    window: &mut Window,
    view: &Entity<CommitArea>,
    action: &PrimaryAction,
) -> gpui_component::menu::PopupMenu {
    let can_commit = matches!(action.kind, PrimaryActionKind::Commit) && !action.disabled;
    let view_commit = view.clone();
    let action_commit = action.clone();

    menu.min_w(px(224.0))
        .item(
            PopupMenuItem::new("Commit")
                .disabled(!can_commit)
                .on_click(window.listener_for(&view_commit, move |area, _, _, cx| {
                    area.submit(&action_commit, cx);
                    cx.notify();
                })),
        )
        .item(PopupMenuItem::new("Commit & Push").disabled(true))
        .item(PopupMenuItem::new("Commit & Sync").disabled(true))
        .separator()
        .item(PopupMenuItem::new("Push").disabled(true))
        .item(PopupMenuItem::new("Create PR").disabled(true))
        .item(PopupMenuItem::new("Push & Create PR").disabled(true))
        .item(PopupMenuItem::new("Pull").disabled(true))
        .item(PopupMenuItem::new("Sync").disabled(true))
        .item(PopupMenuItem::new("Fetch").disabled(true))
}

fn primary_icon_for(kind: PrimaryActionKind) -> Option<IconName> {
    match kind {
        PrimaryActionKind::Commit => Some(IconName::Check),
        PrimaryActionKind::Stage => Some(IconName::Plus),
        PrimaryActionKind::Push | PrimaryActionKind::Publish => Some(IconName::ArrowUp),
        PrimaryActionKind::Pull => Some(IconName::ArrowDown),
        PrimaryActionKind::Sync => Some(IconName::ChevronsUpDown),
    }
}

fn render_status_row(
    theme: Theme,
    _typography: &Typography,
    status: &CommitStatus,
) -> impl IntoElement {
    let (color, msg) = match status {
        CommitStatus::Idle => (theme.fg_subtle, String::new()),
        CommitStatus::Committing => (theme.fg_muted, "Committing…".to_string()),
        CommitStatus::Failed(error) => (theme.status_error, format!("Commit failed: {error}")),
    };
    div()
        .text_size(px(sc_style::META_TEXT))
        .text_color(color)
        .child(msg)
}
