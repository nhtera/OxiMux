//! The plan-approval card — Claude's `ExitPlanMode` request rendered as a
//! dedicated card (plan markdown + the CLI's own three choices) instead of the
//! generic key:value permission card.
//!
//! `ExitPlanMode` rides the same `can_use_tool` control channel as any tool, so
//! the decoder tags it `PermissionKind::Plan` (its `description` carries the plan
//! markdown). Here the tool card routes that kind to `render_plan_card`. Approve
//! echoes the request input plus a `setMode` suggestion so the CLI flips the
//! session's permission mode and continues the turn (verified on the wire); the
//! composer's mode chip updates optimistically since Claude sends no mode echo.

use gpui::{
    AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, px,
};
use oximux_agents::thread::{PermissionRequest, ToolCall};
use oximux_settings::{Density, Theme, Typography};

use super::bubble;
use super::tool_card::pill_button;
use super::AgentChatView;

/// The permission mode a plan approval switches the session into. Mirrors the
/// CLI's ExitPlanMode choices: auto-accept edits, or ask before each edit.
pub(super) const MODE_ACCEPT_EDITS: &str = "acceptEdits";
pub(super) const MODE_DEFAULT: &str = "default";

/// Render an `ExitPlanMode` request as a plan-approval card: the plan rendered as
/// markdown, then three actions — approve into auto-accept-edits mode, approve
/// into ask-each-time mode, or keep planning (deny). Reuses the assistant
/// markdown renderer for the body (`min_w_0` wrap-trap handled there).
pub(super) fn render_plan_card(
    md: &super::markdown_state::Markdown,
    tc: &ToolCall,
    req: &PermissionRequest,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let tool_id = tc.id.clone();
    let request_id = req.request_id.clone();
    // Echo the request's own input (`{plan, planFilePath}`) as `updatedInput` —
    // the CLI treats a bare allow as malformed.
    let input = tc.input.clone();
    // A stable, per-card key for the markdown renderer's internal state. Keyed
    // on the tool call rather than on a transcript position: the card must stay
    // addressable as entries above it come and go.
    let md_key = super::markdown_state::MdKey::Plan(
        tc.id.as_bytes().iter().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(*b as u64)),
    );

    let heading = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .text_size(px(typo.t_body_sm))
        .text_color(theme.status_info)
        .child(div().child(SharedString::from("◆")))
        .child(SharedString::from("Plan ready for review"));

    let plan_body = div()
        .w_full()
        .min_w_0()
        .child(bubble::assistant_body(
            md,
            md_key,
            req.description.trim(),
            None,
            theme,
            density,
            typo,
        ));

    let accept_edits = {
        let (tool_id, request_id, input) = (tool_id.clone(), request_id.clone(), input.clone());
        pill_button(
            format!("plan-accept-edits-{}", tc.id),
            "Approve, auto-accept edits",
            theme.status_ok,
            density,
            typo,
            cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                this.approve_plan(tool_id.clone(), request_id.clone(), input.clone(), MODE_ACCEPT_EDITS, cx)
            }),
        )
    };
    let ask_each = {
        let (tool_id, request_id, input) = (tool_id.clone(), request_id.clone(), input.clone());
        pill_button(
            format!("plan-ask-each-{}", tc.id),
            "Approve, ask each time",
            theme.status_info,
            density,
            typo,
            cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                this.approve_plan(tool_id.clone(), request_id.clone(), input.clone(), MODE_DEFAULT, cx)
            }),
        )
    };
    let keep_planning = {
        let (tool_id, request_id) = (tool_id.clone(), request_id.clone());
        pill_button(
            format!("plan-keep-{}", tc.id),
            "Keep planning",
            theme.status_error,
            density,
            typo,
            cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                this.reject_plan(tool_id.clone(), request_id.clone(), cx)
            }),
        )
    };

    div()
        .flex()
        .flex_col()
        .gap(px(density.gap_inline))
        .w_full()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.status_info)
        .bg(theme.bg_panel_alt)
        .p(px(density.pad_panel))
        .child(heading)
        .child(plan_body)
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(density.gap_inline))
                .child(accept_edits)
                .child(ask_each)
                .child(keep_planning),
        )
        .into_any_element()
}
