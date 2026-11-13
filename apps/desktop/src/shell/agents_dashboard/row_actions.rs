//! The row's own answer line: Allow / Reject / Reply / Dismiss.
//!
//! What a row offers is not decided here — [`oximux_agents::row_actions`] is
//! the one table, and the host has already answered it by the time a row is
//! built. This module is only the painting and the click.
//!
//! **Geometry.** A row with actions grows by one [`ACTION_LINE_HEIGHT`] line
//! and an open Reply box by another, rather than squeezing buttons into the
//! title's flex flow, which would shrink the title it sits beside
//! ([[gpui-a-row-action-cluster-must-leave-the-flex-flow]]). The Reply input
//! gets the full row width on its own line for the same reason the workspace
//! rename field does: a gpui `Input` in a pinned or `flex_none` column
//! collapses to about 24px
//! ([[gpui-pinned-control-column-collapses-a-text-field]]).
//!
//! **Every control stops mouse-down propagation.** The card itself carries an
//! `on_mouse_down` that focuses the agent and records a look, so a click that
//! reached it through a button would open the session the person was trying to
//! answer without opening — and on a row with Dismiss, the look would clear
//! the very alert they were deciding about.

use gpui::{
    App, Div, InteractiveElement, MouseButton, ParentElement, SharedString, Styled, WeakEntity,
    Window, div, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::{Escape as InputEscape, Input, InputState};
use oximux_agents::row_actions::{RowAction, RowActions};
use oximux_settings::{Density, Theme, Typography};

use crate::shell::left_rail::RailAgentTarget;
use crate::ui::pill_button;
use crate::workspace_root::WorkspaceRoot;

/// Height of the control line under a row's three text lines.
pub(crate) const ACTION_LINE_HEIGHT: f32 = 24.0;

/// Height of the Reply line, which holds a `.small()` input (30px) plus the
/// gap above it.
pub(crate) const REPLY_LINE_HEIGHT: f32 = 34.0;

/// Which row's Reply box is open, and the input it uses.
///
/// **One box for the whole list.** Two rows with half-typed replies is two
/// drafts a person has to keep track of, and the rail is 250px wide — so
/// opening a second Reply closes the first, exactly as the workspace rename
/// field works. The input itself is built lazily (it needs a window) and
/// reused, so the state is "which row, if any" rather than a map of inputs.
#[derive(Clone)]
pub struct RowReply {
    /// The attention target whose box is open, if one is.
    pub open_for: Option<String>,
    /// The shared input, present once a Reply has ever been opened.
    pub input: Option<gpui::Entity<InputState>>,
}

impl RowReply {
    /// Whether this row's box is the open one.
    pub fn is_open_for(&self, target: &str) -> bool {
        self.open_for.as_deref() == Some(target)
    }
}

/// Which row a control is addressed to.
///
/// The three travel together everywhere — the attention target the host
/// answers to, the row kind, and the row's own id — and splitting them across
/// a parameter list is how a click ends up addressed to one row while its
/// `mark_viewed` goes to another.
#[derive(Clone, Copy)]
pub(crate) struct RowId<'a> {
    /// What the host knows this row as: `chat:<id>` or `cockpit:<id>`.
    pub target: &'a str,
    pub kind: &'a RailAgentTarget,
    pub db_id: &'a str,
}

/// How much taller than [`super::row_render::ROW_HEIGHT`] this row is.
pub(crate) fn extra_height(actions: &RowActions, reply_open: bool) -> f32 {
    if actions.is_empty() {
        return 0.0;
    }
    ACTION_LINE_HEIGHT + if reply_open { REPLY_LINE_HEIGHT } else { 0.0 }
}

/// The summary line a row shows above its buttons: what is actually being
/// approved or asked.
///
/// Returned separately from the action line because it belongs with the row's
/// other text, under the title — and because a row whose summary the host
/// withheld has no buttons either, so the two always appear together.
pub(crate) fn summary_line(
    actions: &RowActions,
    theme: Theme,
    typography: &Typography,
) -> Option<Div> {
    let summary = actions.summary.clone()?;
    Some(
        div()
            .w_full()
            .min_w_0()
            .text_size(px(typography.t_body_sm))
            // `fg_muted`, not `fg_subtle`: this is the line the decision is
            // made from, so it must not read as the dimmest thing on the row.
            .text_color(theme.fg_muted)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(summary),
    )
}

/// The control line, and the Reply box when it is open for this row.
///
/// Every action is addressed to `row.target`, the attention target — never to
/// the row's database id, which names a different thing for a chat than for a
/// cockpit row.
pub(crate) fn action_line(
    actions: &RowActions,
    row: RowId<'_>,
    reply: &RowReply,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> Option<Div> {
    if actions.is_empty() {
        return None;
    }
    let mut line = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ACTION_LINE_HEIGHT))
        .gap(px(density.gap_inline));

    for &action in &actions.actions {
        let accent = match action {
            RowAction::Allow => theme.status_ok,
            RowAction::Reject => theme.status_error,
            // Reply and Dismiss are not verdicts on the agent's request, so
            // neither takes a verdict's colour: a muted outline reads as the
            // neutral action it is.
            RowAction::Reply | RowAction::Dismiss => theme.fg_muted,
        };
        let weak = weak_root.clone();
        let (target, row_target, db_id) =
            (row.target.to_string(), row.kind.clone(), row.db_id.to_string());
        // The answer travels WITH the click, carrying the digest this row was
        // painted with — not re-read when the button is pressed. A re-read
        // would hand back the digest of whatever is pending now, which would
        // make the host's check compare the current arguments against
        // themselves and always pass. The whole guarantee is that the digest
        // is older than the click.
        let answer = actions.answer.clone();
        line = line.child(
            div()
                // A wrapper, because the pill's own `on_click` fires on
                // mouse-up while the card's focus handler is a mouse-DOWN —
                // so stopping propagation has to happen on the down event or
                // the card has already acted by the time the pill is clicked.
                .id(SharedString::from(format!("row-action-{db_id}-{action:?}")))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(pill_button(
                    SharedString::from(format!("row-action-pill-{db_id}-{action:?}")),
                    label(action),
                    accent,
                    density,
                    typography,
                    move |_, window: &mut Window, cx: &mut App| {
                        let _ = weak.update(cx, |root, cx| {
                            root.act_on_row_action(
                                action, &answer, &target, &row_target, &db_id, window, cx,
                            );
                        });
                    },
                )),
        );
    }

    if !reply.is_open_for(row.target) {
        return Some(line);
    }
    let Some(input) = reply.input.clone() else { return Some(line) };
    let weak_root_for_escape = weak_root;
    Some(
        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(density.gap_inline))
            .child(line)
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    // The input swallows its own clicks, but the row around
                    // it would still focus the agent — which moves the focus
                    // out of the box the person is typing in.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // Escape collapses the box, captured ahead of the
                    // Input's own handling, exactly as the Agents filter
                    // clears itself. Captured rather than bubbled because
                    // the Input would otherwise consume it; the IME's own
                    // Escape still comes first, which is what keeps a
                    // half-composed Telex syllable from closing the box
                    // ([[oximux-escape-swallowed-by-ime]]).
                    .capture_action({
                        let weak = weak_root_for_escape;
                        move |_: &InputEscape, window, cx| {
                            let _ = weak.update(cx, |root, cx| {
                                root.close_row_reply(window, cx);
                            });
                        }
                    })
                    .child(Input::new(&input).small()),
            ),
    )
}

/// The word on the button. Desktop says **Reject**, the same word as the chat
/// permission card; the phone and the push say Deny.
fn label(action: RowAction) -> &'static str {
    match action {
        RowAction::Allow => "Allow",
        RowAction::Reject => "Reject",
        RowAction::Reply => "Reply",
        RowAction::Dismiss => "Dismiss",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oximux_agents::row_actions::InlineAnswer;

    fn offering(actions: Vec<RowAction>) -> RowActions {
        RowActions { actions, answer: InlineAnswer::Prompt, summary: None }
    }

    #[test]
    fn a_row_with_nothing_to_answer_keeps_its_height() {
        assert_eq!(extra_height(&RowActions::nothing(), false), 0.0);
        // Even if a stale "open" flag named it: a row with no Reply button
        // must not reserve a line for a box it cannot show.
        assert_eq!(
            extra_height(&RowActions::nothing(), true),
            0.0,
            "no actions means no control line to put a box under"
        );
    }

    #[test]
    fn actions_add_one_line_and_an_open_reply_adds_another() {
        let actions = offering(vec![RowAction::Reply]);
        assert_eq!(extra_height(&actions, false), ACTION_LINE_HEIGHT);
        assert_eq!(extra_height(&actions, true), ACTION_LINE_HEIGHT + REPLY_LINE_HEIGHT);
    }

    #[test]
    fn only_the_row_whose_box_is_open_reserves_the_reply_line() {
        // One box for the whole list: the open row grows, every other row
        // offering Reply stays at the control line.
        let reply = RowReply { open_for: Some("chat:a".into()), input: None };
        assert!(reply.is_open_for("chat:a"));
        assert!(!reply.is_open_for("chat:b"));
        assert!(!RowReply { open_for: None, input: None }.is_open_for("chat:a"));
    }

    /// Every action set is laid out and painted, at both rail widths, with
    /// the Reply box open.
    ///
    /// `VisualTestContext::draw` runs a real layout, prepaint and paint —
    /// not a render call that is thrown away, which is what a
    /// notify-and-park cycle gives in a test window. It is drawn **through a
    /// view**, not as a bare element tree: a `capture_action` listener and
    /// the `Input` both ask the window for the view currently rendering, and
    /// outside one `current_view()` aborts
    /// ([[gpui-current-view-aborts-in-event-handlers]]).
    ///
    /// What it guards is a panic at paint inside this tree — and it has
    /// already caught one: drawn as a bare element tree, the `capture_action`
    /// listener aborted in `current_view()`, which is how the "draw through a
    /// view" above came to be written. It does **not** check geometry; the
    /// Reply box filling the row at 250px and at the 220px minimum is a
    /// canvas probe in the live drill (step 8), because measuring it from
    /// here would mean a test-only field on `RowReply` and a `cfg` at every
    /// construction site.
    #[gpui::test]
    fn every_action_set_is_laid_out_and_painted_at_both_rail_widths(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use gpui::{AvailableSpace, IntoElement as _, point, size};
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_component::init);
        let sink: Rc<RefCell<Option<gpui::Entity<ActionProbe>>>> = Rc::new(RefCell::new(None));
        let out = sink.clone();
        let w = cx.add_window(move |window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Reply…"));
            let probe = cx.new(move |_cx| ActionProbe { input, width: 250.0, painted: 0 });
            *out.borrow_mut() = Some(probe.clone());
            let any: gpui::AnyView = probe.into();
            gpui_component::Root::new(any, window, cx)
        });
        let probe = sink.borrow().clone().expect("the probe was built");
        let mut vcx = gpui::VisualTestContext::from_window(w.into(), cx);

        // The default rail, then its 220px minimum: a control line that
        // overflows or a Reply box that cannot be laid out has to fail in
        // both, and the narrow one is where it would.
        for width in [250.0_f32, 220.0_f32] {
            probe.update(&mut vcx, |probe, _cx| probe.width = width);
            let view: gpui::AnyView = probe.clone().into();
            vcx.draw(
                point(px(0.0), px(0.0)),
                size(AvailableSpace::Definite(px(width)), AvailableSpace::MinContent),
                move |_window, _cx| view.into_any_element(),
            );
        }
        // Proof it drew rather than merely compiled. Not an exact count: the
        // test window also draws the probe as its own root, so the two
        // explicit draws are a floor.
        probe.update(&mut vcx, |probe, _cx| {
            assert!(probe.painted >= 2, "drew {} times, expected both widths", probe.painted);
        });
    }

    /// Three rows, one with its Reply box open — so one draw covers the
    /// two-verdict row, the Dismiss row and the open-box row together.
    struct ActionProbe {
        input: gpui::Entity<InputState>,
        width: f32,
        painted: usize,
    }

    impl gpui::Render for ActionProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use oximux_agents::row_actions::InlineAnswer;
            self.painted += 1;
            let reply =
                RowReply { open_for: Some("chat:c".to_string()), input: Some(self.input.clone()) };
            let sets = [
                (
                    "chat:a",
                    RowActions {
                        actions: vec![RowAction::Allow, RowAction::Reject],
                        answer: InlineAnswer::Permission {
                            request_id: "r1".into(),
                            args_digest: "d1".into(),
                        },
                        summary: Some("Bash: pnpm db:reset".into()),
                    },
                ),
                (
                    "chat:b",
                    RowActions {
                        actions: vec![RowAction::Dismiss],
                        answer: InlineAnswer::Nothing,
                        summary: None,
                    },
                ),
                (
                    "chat:c",
                    RowActions {
                        actions: vec![RowAction::Reply],
                        answer: InlineAnswer::Question { request_id: "q1".into() },
                        summary: Some("Which README?".into()),
                    },
                ),
            ];
            let mut col = div().flex().flex_col().w(px(self.width));
            for (target, actions) in sets {
                let kind = RailAgentTarget::Chat { session_id: target.into() };
                col = col
                    .children(summary_line(&actions, Theme::default(), &Typography::default()))
                    .children(action_line(
                        &actions,
                        RowId { target, kind: &kind, db_id: target },
                        &reply,
                        // The row must survive a root that is gone; a test
                        // window is one.
                        gpui::WeakEntity::new_invalid(),
                        Theme::default(),
                        Density::default(),
                        &Typography::default(),
                    ));
            }
            col
        }
    }

    #[test]
    fn the_desktop_words_are_the_chat_card_s_words() {
        // The row was written against the card's vocabulary; the phone and
        // the push say Deny, and that difference is deliberate.
        assert_eq!(label(RowAction::Allow), "Allow");
        assert_eq!(label(RowAction::Reject), "Reject");
    }
}
