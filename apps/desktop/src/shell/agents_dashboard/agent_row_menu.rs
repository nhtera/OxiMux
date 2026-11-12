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
    SharedString, Styled, WeakEntity, Window, div, px,
};
use oximux_ui::MenuRow;
use oximux_agents::attention::Attention;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::rail_agent_row::RailAgentTarget;
use crate::workspace_root::WorkspaceRoot;

/// Pushes the card clear of the pointer so the first item is not already
/// hovered when it appears. Scaled with the rest of the interface, since it is
/// measured against a row height that scales.
const ANCHOR_Y_OFFSET: f32 = 4.0;
/// Width of the card. The longest label is "Mark as unread"; fixed so the
/// menu does not resize between a row that offers Dismiss and one that does
/// not, and so the clamp below knows the width before layout.
const MENU_WIDTH: f32 = 176.0;

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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        // The shared row, not a seventh hand-rolled one: `MenuRow` exists
        // because six menus had drifted into four signatures for the same
        // painted control, and it is what agrees on the height, the padding
        // and the type size — all of which scale with interface zoom.
        for (ix, &action) in state.actions.iter().enumerate() {
            let menu = cx.entity();
            card = card.child(
                MenuRow::new(
                    SharedString::from(format!("agent-row-menu-{ix}")),
                    action.label(),
                    theme,
                    density,
                    typography.clone(),
                )
                .build(move |_, _window, cx| {
                    menu.update(cx, |menu, cx| menu.dispatch(action, cx));
                }),
            );
        }

        // Keep the card inside the window. Two rows is a short menu, but the
        // second one is Dismiss — the whole point of the menu — and a
        // right-click near the bottom edge would put it where it cannot be
        // clicked. The height is computed rather than measured, the way the
        // terminal's context menu does it: the row count is known here and
        // the metrics scale together.
        let rows = state.actions.len() as f32;
        let height = 2.0 * density.pad_overlay + rows * density.h_overlay_item;
        let viewport = window.viewport_size();
        let (vp_w, vp_h) = (f32::from(viewport.width), f32::from(viewport.height));
        // Not enough room below: anchor the card's bottom at the pointer, and
        // never let it run off the top either.
        let top = if state.y + height > vp_h { (state.y - height).max(0.0) } else { state.y };
        let left =
            if state.x + MENU_WIDTH > vp_w { (state.x - MENU_WIDTH).max(0.0) } else { state.x };

        // A full-window catcher behind the card closes it on any outside
        // click, the same way the workspace row menu dismisses. It *occludes*:
        // without that, a click passes through to the row underneath, which
        // would run that row's own click — and a row click records a look, so
        // choosing "Mark as unread" would be undone by the very click that
        // chose it.
        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(MENU_WIDTH))
                    // A click on the card is the card's, not the backdrop's:
                    // without this it would also close the menu, and the
                    // close would race the action.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
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
