//! Right-click menu for a dashboard agent row: the two things a person can
//! say back to an alert without opening it.
//!
//! **Dismiss is why this exists.** A failure stands in NEEDS YOU until it is
//! viewed or dismissed, and the review of PR B found the gap that makes the
//! second half necessary: a row with nothing to open — a history row, a chat
//! another agent spawned — cannot be *viewed*, because clicking it shows the
//! person nothing and so is not allowed to count as reading it. Without an
//! explicit Dismiss such a row would ask forever.
//!
//! **What the menu offers is decided once, by a pure function.**
//! [`menu_actions`] takes the row's attention and returns the list, so the
//! gating is a table of cases testable without a window — and so Dismiss is
//! offered exactly where the evaluator says an alert can be dealt with by
//! looking at it, rather than from a second list of kinds that could drift.
//!
//! Mounted at `WorkspaceRoot` beside the workspace row menu (the
//! primitive-picking fork: a floating entity, not a child of the row, so it
//! paints above the dashboard and survives the row being rebuilt).

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_agents::attention::Attention;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::rail_agent_row::RailAgentTarget;
use crate::workspace_root::WorkspaceRoot;

/// Height of one menu row, matching the workspace row menu so the two read as
/// the same control.
const ITEM_H: f32 = 26.0;
const PADDING_X: f32 = 8.0;
/// Pushes the card clear of the pointer so the first item is not already
/// hovered when it appears.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// What a row's menu can offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRowAction {
    /// Keep standing out although it has been read.
    MarkUnread,
    /// This has been dealt with: stop asking.
    Dismiss,
}

impl AgentRowAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::MarkUnread => "Mark as unread",
            Self::Dismiss => "Dismiss",
        }
    }
}

/// What this row's menu offers.
///
/// Mark as unread is always there: it is about the session, not about any
/// alert, and a person may want a finished turn to keep standing out.
///
/// Dismiss appears only where the evaluator says a look can deal with the
/// alert — a failure or a stall ([`AttentionKind::is_dismissible`]). A
/// permission cannot be dismissed, because the agent is still waiting on an
/// answer and hiding the row would not give it one; and a row that wants
/// nothing has nothing to dismiss.
///
/// [`AttentionKind::is_dismissible`]: oximux_agents::attention::AttentionKind::is_dismissible
pub fn menu_actions(attention: Option<&Attention>) -> Vec<AgentRowAction> {
    let mut out = vec![AgentRowAction::MarkUnread];
    if attention.is_some_and(|a| a.kind.is_dismissible()) {
        out.push(AgentRowAction::Dismiss);
    }
    out
}

/// The row the menu is open for.
#[derive(Clone)]
struct OpenState {
    /// The row's id and kind, which together name its attention target.
    db_id: String,
    target: RailAgentTarget,
    actions: Vec<AgentRowAction>,
    x: f32,
    y: f32,
}

pub struct AgentRowMenu {
    /// `None` when closed.
    open_for: Option<OpenState>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl AgentRowMenu {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        weak_root: WeakEntity<WorkspaceRoot>,
    ) -> Self {
        Self { open_for: None, weak_root, theme, density, typography }
    }

    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// Open the menu for one row, anchored at the pointer.
    ///
    /// Closed again when the row has nothing to offer, which cannot happen
    /// today — Mark as unread is always offered — but keeps an empty card from
    /// ever appearing if that changes.
    pub fn open(
        &mut self,
        db_id: String,
        target: RailAgentTarget,
        attention: Option<&Attention>,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        let actions = menu_actions(attention);
        self.open_for = (!actions.is_empty())
            .then_some(OpenState { db_id, target, actions, x, y: y + ANCHOR_Y_OFFSET });
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    /// Run `action` against the open row, then close.
    fn dispatch(&mut self, action: AgentRowAction, cx: &mut Context<Self>) {
        if let Some(state) = self.open_for.clone() {
            let _ = self.weak_root.update(cx, |root, cx| {
                root.act_on_agent_row(&state.db_id, &state.target, action, cx);
            });
        }
        self.close(cx);
    }
}

impl Render for AgentRowMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        oximux_settings::appearance::sync(
            &mut self.theme,
            &mut self.density,
            &mut self.typography,
            cx,
        );
        let Some(state) = self.open_for.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        for (ix, &action) in state.actions.iter().enumerate() {
            card = card.child(
                div()
                    .id(("agent-row-menu-item", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(ITEM_H))
                    .px(px(PADDING_X))
                    .rounded(px(density.r_xs))
                    .cursor_pointer()
                    .whitespace_nowrap()
                    .hover(move |s| s.bg(theme.hover_overlay))
                    .text_size(px(typography.t_body_md))
                    .text_color(theme.fg_base)
                    .child(action.label())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                            this.dispatch(action, cx);
                        }),
                    ),
            );
        }

        // A full-screen catcher behind the card closes it on any outside
        // click, the same way the workspace row menu dismisses.
        div()
            .absolute()
            .left_0()
            .top_0()
            .size_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(div().absolute().left(px(state.x)).top(px(state.y)).child(card))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::attention::{AttentionKind, AttentionSource};

    fn attention(kind: AttentionKind) -> Attention {
        Attention { kind, since: 0, reason: None, source: AttentionSource::Health }
    }

    #[test]
    fn a_failure_and_a_stall_can_be_dismissed() {
        // The two the evaluator calls dismissible, and the reason this menu
        // exists: a failed row with nothing to open has no other way out of
        // NEEDS YOU.
        for kind in [AttentionKind::Failed, AttentionKind::Stalled] {
            assert_eq!(
                menu_actions(Some(&attention(kind))),
                vec![AgentRowAction::MarkUnread, AgentRowAction::Dismiss],
                "{kind:?} should offer Dismiss"
            );
        }
    }

    #[test]
    fn nothing_else_can_be_dismissed() {
        // A permission is not dismissible because the agent is still waiting
        // on an answer: hiding the row would not give it one. A finished turn
        // is read by reading it. A rate limit clears itself.
        for kind in [
            AttentionKind::NeedsPermission,
            AttentionKind::NeedsInput,
            AttentionKind::FinishedUnread,
            AttentionKind::Limited,
        ] {
            assert_eq!(
                menu_actions(Some(&attention(kind))),
                vec![AgentRowAction::MarkUnread],
                "{kind:?} should not offer Dismiss"
            );
        }
    }

    #[test]
    fn a_quiet_row_can_still_be_marked_unread() {
        // Mark as unread is about the session, not about an alert: a person
        // may want a finished turn to keep standing out until they act on it.
        assert_eq!(menu_actions(None), vec![AgentRowAction::MarkUnread]);
    }
}
