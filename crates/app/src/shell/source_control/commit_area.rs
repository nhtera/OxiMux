//! Inline commit composer mounted inside the Source Control panel.
//!
//! Single multi-line `Message` textarea plus a full-width primary action
//! button. The primary's label/icon adapts via the resolved `PrimaryAction`
//! (Commit / Stage Files / Push / Pull / Sync / Publish Branch).
//!
//! Single-flight: an `AtomicBool` guards re-entry so a fast double-click only
//! triggers one commit.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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

use crate::shell::source_control::dropdown_items::{
    self, DropdownActionKind, DropdownEntry, DropdownInputs,
};
use crate::shell::source_control::primary_action::{PrimaryAction, PrimaryActionKind};
use crate::shell::source_control::style as sc_style;

/// Last status surfaced from a commit / remote operation. Mirrors the
/// primary-action lifecycle in the panel header so the user sees the
/// op-in-flight even when the dropdown chevron triggered it (not just the
/// main split button).
///
/// `Failed(label, message)` includes the failing op's verb so the toast
/// reads "Push failed: …" rather than the generic "Commit failed". Reset
/// to `Idle` on the next successful op.
#[derive(Debug, Clone, Default)]
pub enum CommitStatus {
    #[default]
    Idle,
    Committing,
    Pushing,
    Pulling,
    Syncing,
    Fetching,
    Failed(String, String),
}

pub struct CommitArea {
    // `pub(in crate::shell::source_control)` on the fields the sibling
    // `commit_ops` module pokes. Restricting visibility this way (rather
    // than `pub(crate)`) keeps the fields out of the wider app surface
    // while still letting the in-module helper module reach them.
    pub(in crate::shell::source_control) repo: Repository,
    pub message_state: Entity<InputState>,
    pub status: CommitStatus,
    pub(in crate::shell::source_control) in_flight: Arc<AtomicBool>,
    theme: Theme,
    density: Density,
    typography: Typography,
    /// Drop cancels the in-flight commit observer task.
    pub(in crate::shell::source_control) _commit_task: Option<Task<()>>,
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
        super::commit_ops::run_commit(self, false, false, cx);
    }

    /// Commit-then-push convenience used by the dropdown's "Commit & Push"
    /// item. Push only fires on a successful commit; partial failure
    /// surfaces via `CommitStatus::Failed("commit"/"push", …)`.
    pub fn commit_and_push(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_commit(self, true, false, cx);
    }

    /// Commit-then-sync (pull + push). Same single-flight + status surface
    /// as `commit_and_push`.
    pub fn commit_and_sync(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_commit(self, false, true, cx);
    }

    /// Standalone `git push` — used by the dropdown's Push item and the
    /// primary action when `PrimaryActionKind::Push` resolves.
    pub fn push(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Push, cx);
    }

    /// Standalone `git pull --ff-only`.
    pub fn pull(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Pull, cx);
    }

    /// Standalone `git pull --ff-only && git push`.
    pub fn sync(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Sync, cx);
    }

    /// `git fetch --all --prune`. Low-risk — never mutates the working
    /// tree, only updates remote-tracking refs.
    pub fn fetch(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Fetch, cx);
    }

    /// Force-push (with lease). Stubbed: the backend lands alongside the
    /// upstream-rewrite-detection feature; until then the user sees a
    /// "Not yet implemented" message in the status row rather than the
    /// menu item silently doing nothing.
    pub fn force_push(&mut self, cx: &mut Context<Self>) {
        self.status =
            CommitStatus::Failed("force push".to_string(), "Not yet implemented".to_string());
        cx.notify();
    }

    /// Commit-then-force-push convenience used by the dropdown's
    /// "Commit & Force Push" item when an upstream rewrite is needed.
    /// Stubbed for the same reason as `force_push`.
    pub fn commit_and_force_push(&mut self, cx: &mut Context<Self>) {
        self.status = CommitStatus::Failed(
            "commit & force push".to_string(),
            "Not yet implemented".to_string(),
        );
        cx.notify();
    }

    /// Rebase the current branch onto the configured base ref. Stubbed:
    /// rebase backend wiring lands in a later phase; the dropdown row
    /// exists now so the menu shape stays stable across versions.
    pub fn rebase_from_base(&mut self, cx: &mut Context<Self>) {
        self.status =
            CommitStatus::Failed("rebase".to_string(), "Not yet implemented".to_string());
        cx.notify();
    }

    /// Publish the current branch by pushing it with `-u origin <branch>`.
    /// Currently routed through the `Publish` remote verb in `commit_ops`,
    /// which uses the existing `Repository::publish_branch` backend.
    pub fn publish(&mut self, cx: &mut Context<Self>) {
        super::commit_ops::run_remote(self, super::commit_ops::RemoteVerb::Publish, cx);
    }

    /// Apply a completed op result to the status surface. Called from the
    /// commit-ops completion task; pub(super) so the helper module can
    /// reach it without re-exposing the field.
    pub(super) fn apply_result(&mut self, result: Result<&'static str, (&'static str, String)>) {
        match result {
            Ok(_label) => {
                self.status = CommitStatus::Idle;
                // `InputState::set_value` requires `&mut Window`, which isn't
                // available in this oneshot result callback. v1 trade-off:
                // leave the message in place after commit; the user clears
                // manually.
            }
            Err((label, error)) => self.status = CommitStatus::Failed(label.to_string(), error),
        }
    }

    pub fn render(
        &self,
        action: &PrimaryAction,
        dropdown_inputs: DropdownInputs,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

        // Chevron opens a dropdown built from `dropdown_items::resolve`.
        // Each row carries its own label / tooltip / disabled state; the
        // render layer just walks the resolved Vec and emits one
        // `PopupMenuItem` per `Item` + a separator line per `Separator`.
        let action_view = cx.entity();
        // Resolve once up front so the menu builder closure doesn't need to
        // re-resolve on each open (and so the resolution sees the same
        // inputs as the rest of this render call).
        let resolved_entries = dropdown_items::resolve(&dropdown_inputs);
        // DropdownButton handles the shared borders + unified variant so the
        // chevron half can't render brighter than the disabled main half.
        // `.small()` brings the chunky default-size pill down to ~32px tall
        // so it sits proportional to the textarea above.
        //
        // Variant switch: a `.primary()` button retains its bright fill even
        // when disabled, which reads as "active" to the eye. Fall back to
        // `.secondary()` when disabled so the button matches the dimmed
        // visual treatment users expect (a gray disabled-Commit
        // state). Enabled state keeps `.primary()` so the affirmative action
        // still pops.
        let mut primary = DropdownButton::new("sc-commit-split")
            .button(inner_button)
            .small()
            .disabled(submit_disabled)
            .w_full();
        primary = if submit_disabled {
            primary.secondary()
        } else {
            primary.primary()
        };
        let primary =
            primary.dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, _cx| {
                build_commit_menu(menu, window, &action_view, &resolved_entries)
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

/// Chevron dropdown items: every entry comes from the resolved
/// `Vec<DropdownEntry>`. Per-row label, tooltip, and disabled state are
/// owned by `dropdown_items::resolve`; this function only wires each
/// `DropdownActionKind` to the matching `CommitArea` method and turns
/// `Separator` into a divider line.
///
/// Items whose backend hasn't landed yet (force-push variants, rebase)
/// dispatch to stub methods on `CommitArea` that surface a "Not yet
/// implemented" message in the status row.
fn build_commit_menu(
    mut menu: gpui_component::menu::PopupMenu,
    window: &mut Window,
    view: &Entity<CommitArea>,
    entries: &[DropdownEntry],
) -> gpui_component::menu::PopupMenu {
    menu = menu.min_w(px(224.0));
    for entry in entries {
        match entry {
            DropdownEntry::Separator => {
                menu = menu.separator();
            }
            DropdownEntry::Item {
                kind,
                label,
                title,
                disabled,
            } => {
                menu = menu.item(build_menu_item(window, view, *kind, label, title, *disabled));
            }
        }
    }
    menu
}

/// Map one resolved entry into a `PopupMenuItem`.
///
/// The menu component has no per-row hover-tooltip API, so we encode the
/// disabled-reason copy inline in the label (`"Push — Nothing to push"`).
/// Surfaces the "why not" without hover and keeps the menu shape stable.
/// Enabled rows render the bare label.
fn build_menu_item(
    window: &mut Window,
    view: &Entity<CommitArea>,
    kind: DropdownActionKind,
    label: &str,
    title: &str,
    disabled: bool,
) -> PopupMenuItem {
    let composed = if disabled && !title.is_empty() {
        format!("{label} — {title}")
    } else {
        label.to_string()
    };
    let item = PopupMenuItem::new(composed).disabled(disabled);
    // Disabled rows still need an on_click slot for the menu's click
    // dispatcher; the gate happens at `PopupMenuItem.disabled` and the
    // dispatch table no-ops on PR kinds so a wired click never escapes.
    let view_for_click = view.clone();
    item.on_click(window.listener_for(&view_for_click, move |area, _, _, cx| {
        dispatch_dropdown(area, kind, cx);
        cx.notify();
    }))
}

/// Single dispatch table — kind → `CommitArea` method. Keeping the match
/// here means the resolver and the action surface only meet at this one
/// site; adding a new dropdown verb is "add a kind + add a stub method +
/// add an arm here".
fn dispatch_dropdown(area: &mut CommitArea, kind: DropdownActionKind, cx: &mut Context<CommitArea>) {
    match kind {
        // Compound commit-with-followup verbs need a synthesised
        // `PrimaryAction { kind: Commit, disabled: false }` to satisfy
        // `submit`'s gate. The resolver already gated the row so we
        // know the user can commit; if it's disabled the menu blocked
        // the click before we got here.
        DropdownActionKind::Commit => {
            let synthetic = PrimaryAction {
                kind: PrimaryActionKind::Commit,
                label: "Commit".to_string(),
                title: String::new(),
                disabled: false,
            };
            area.submit(&synthetic, cx);
        }
        DropdownActionKind::CommitPush => area.commit_and_push(cx),
        DropdownActionKind::CommitForcePush => area.commit_and_force_push(cx),
        DropdownActionKind::CommitSync => area.commit_and_sync(cx),
        DropdownActionKind::Push => area.push(cx),
        DropdownActionKind::ForcePush => area.force_push(cx),
        DropdownActionKind::Pull => area.pull(cx),
        DropdownActionKind::Sync => area.sync(cx),
        DropdownActionKind::Rebase => area.rebase_from_base(cx),
        DropdownActionKind::Fetch => area.fetch(cx),
        DropdownActionKind::Publish => area.publish(cx),
        // PR rows ship disabled — dispatch should never be reached.
        DropdownActionKind::CreatePr | DropdownActionKind::PushBeforePr => {}
    }
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
        CommitStatus::Pushing => (theme.fg_muted, "Pushing…".to_string()),
        CommitStatus::Pulling => (theme.fg_muted, "Pulling…".to_string()),
        CommitStatus::Syncing => (theme.fg_muted, "Syncing…".to_string()),
        CommitStatus::Fetching => (theme.fg_muted, "Fetching…".to_string()),
        CommitStatus::Failed(label, error) => (
            theme.status_error,
            // Title-case the verb so "Push failed: …" reads naturally;
            // labels are always short ASCII so the manual capitalize is fine.
            format!("{} failed: {error}", title_case_first(label)),
        ),
    };
    div()
        .text_size(px(sc_style::META_TEXT))
        .text_color(color)
        .child(msg)
}

fn title_case_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

// `RemoteVerb` moved to `commit_ops.rs` — the helper module owns the
// op-execution machinery and the verb enum it dispatches on.
