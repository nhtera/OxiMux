//! Tool-call rendering for the chat transcript: a status header, an
//! expand/collapse disclosure for the raw input + result, and — while the call
//! is gated on the user — Allow / Reject buttons that resolve the permission
//! through the connection (`request_id`-keyed, per this crate's transport).
//!
//! The inline **diff** card for Edit/Write is a follow-up slice; expanded input
//! shows the raw JSON payload until then. Interactive pieces live here (not in
//! `bubble`) because they need a `Context<AgentChatView>` for click listeners.

use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use oximux_agents::thread::{PermissionDecision, PermissionRequest, ToolCall, ToolCallStatus};
use oximux_settings::{Density, Theme, Typography};

use super::AgentChatView;
use super::bubble;
use super::diff_card;
use super::tool_bodies;

/// Cap on the rendered tool-result body so a chatty tool can't blow up a row.
const RESULT_CHARS: usize = 4000;

/// Render one tool call. A compact status row by default; framed as a card with
/// Allow/Reject buttons while awaiting confirmation, and with raw-input/result
/// blocks when expanded.
pub(super) fn render_tool_card(
    tc: &ToolCall,
    expanded: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let awaiting = matches!(tc.status, ToolCallStatus::WaitingForConfirmation(_));
    let has_detail = !tc.input.is_null() || tc.result.is_some();
    let framed = awaiting || expanded;

    let mut card = div().flex().flex_col().gap(px(4.0)).w_full();
    if framed {
        card = card
            .rounded(px(density.r_card))
            .border_1()
            .border_color(if awaiting {
                theme.status_warn
            } else {
                theme.border_inactive
            })
            .bg(theme.bg_panel_alt)
            .p(px(density.pad_panel));
    } else {
        card = card.py(px(2.0));
    }

    card = card.child(header_row(tc, expanded, has_detail, theme, density, typo, cx));

    // Inline diff for Edit/Write: shown while awaiting (so the user sees what
    // they're approving) and whenever the card is expanded. Only build the
    // (Myers) diff when it will actually render — a collapsed, settled tool
    // call (the bulk of a long transcript) skips the recompute every frame.
    let diff = if awaiting || expanded {
        diff_card::build_edit_diff(tc)
    } else {
        None
    };
    if let Some(lines) = diff.as_ref() {
        // A whole-file Write shows only additions (its payload carries no prior
        // content), so an all-green diff must not read as "purely additive /
        // safe" — call out that it replaces the file.
        if tc.name == "Write" {
            card = card.child(write_replace_hint(theme, typo));
        }
        card = card.child(diff_card::render_diff(lines, theme, density, typo));
    }

    if let ToolCallStatus::WaitingForConfirmation(req) = &tc.status {
        card = card.child(approval_row(tc, req, theme, density, typo, cx));
    }

    if expanded {
        if diff.is_some() {
            // Edit/Write/MultiEdit: the diff is shown above; add any textual
            // result too (e.g. an error message).
            if let Some(result) = &tc.result {
                card = card.child(result_block(result, theme, density, typo));
            }
        } else if let Some(body) = tool_bodies::render_tool_body(tc, theme, density, typo) {
            // Bash/Read/Grep/Glob: a legible, tool-specific body (command +
            // output, file slice, or match list) instead of raw JSON.
            card = card.child(body);
        } else {
            // Unknown tool: the generic raw input + result disclosure.
            card = card.child(raw_input_block(tc, theme, density, typo));
            if let Some(result) = &tc.result {
                card = card.child(result_block(result, theme, density, typo));
            }
        }
    }

    card.into_any_element()
}

/// Status glyph + tool label; the whole row toggles the disclosure when there's
/// expandable content.
fn header_row(
    tc: &ToolCall,
    expanded: bool,
    has_detail: bool,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let (glyph, glyph_color) = bubble::status_glyph(&tc.status, theme);
    let mut label = tc.name.clone();
    if let Some(target) = bubble::tool_target(tc) {
        label.push(' ');
        label.push_str(&bubble::elide(&target, 80));
    }
    let id = tc.id.clone();
    div()
        .id(SharedString::from(format!("tool-hdr-{}", tc.id)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .w_full()
        .text_size(px(typo.t_body_sm))
        .text_color(theme.fg_muted)
        .when(has_detail, |s| {
            s.cursor_pointer()
                .hover(|s| s.text_color(theme.fg_base))
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    this.toggle_tool_expanded(id.clone(), cx)
                }))
        })
        .child(
            div()
                .text_color(glyph_color)
                .child(SharedString::from(glyph.to_string())),
        )
        .child(SharedString::from(label))
        .when(has_detail, |s| {
            s.child(
                div()
                    .text_color(theme.fg_subtle)
                    .child(SharedString::from(if expanded { "▾" } else { "▸" })),
            )
        })
        .into_any_element()
}

/// The Allow / Reject row shown while a tool waits on the user. Clicking routes
/// the decision to the connection by `request_id` and transitions the local
/// status (Allow → InProgress so the tool proceeds; Reject → Rejected). Allow
/// echoes the tool input as `updatedInput` (required by the transport).
fn approval_row(
    tc: &ToolCall,
    req: &PermissionRequest,
    theme: Theme,
    density: Density,
    typo: &Typography,
    cx: &mut Context<AgentChatView>,
) -> AnyElement {
    let tool_id = tc.id.clone();
    let request_id = req.request_id.clone();
    // Echo the tool's own input as `updatedInput` (required by the transport).
    // For a matched call this is the `tool_use` block's input; the CLI sends the
    // same input in its `can_use_tool` request, so the two are identical for the
    // same tool call.
    let input = tc.input.clone();

    let prompt = if req.description.trim().is_empty() {
        format!("Allow Claude to run {}?", tc.name)
    } else {
        format!("Allow {}?", req.description.trim())
    };

    let allow = {
        let (tool_id, request_id, input) = (tool_id.clone(), request_id.clone(), input.clone());
        pill_button(
            format!("tool-allow-{}", tc.id),
            "Allow",
            theme.status_ok,
            density,
            typo,
            cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                this.resolve_permission(
                    tool_id.clone(),
                    request_id.clone(),
                    PermissionDecision::Allow { updated_input: input.clone() },
                    cx,
                )
            }),
        )
    };
    let reject = {
        let (tool_id, request_id) = (tool_id.clone(), request_id.clone());
        pill_button(
            format!("tool-reject-{}", tc.id),
            "Reject",
            theme.status_error,
            density,
            typo,
            cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                this.resolve_permission(
                    tool_id.clone(),
                    request_id.clone(),
                    PermissionDecision::Deny { message: "Denied by user".into() },
                    cx,
                )
            }),
        )
    };

    // One pill per agent-offered suggestion (e.g. "Always (acceptEdits)",
    // "Always allow this pattern"). Choosing one allows this call AND applies
    // the suggestion so the CLI stops re-prompting for that tool/scope. Rendered
    // in a distinct accent from Allow/Reject.
    let suggestion_pills: Vec<AnyElement> = req
        .suggestions
        .iter()
        .enumerate()
        .map(|(i, sugg)| {
            let (tool_id, request_id, input, suggestion) =
                (tool_id.clone(), request_id.clone(), input.clone(), sugg.clone());
            pill_button(
                format!("tool-always-{}-{}", tc.id, i),
                sugg.label.clone(),
                theme.status_info,
                density,
                typo,
                cx.listener(move |this, _e: &gpui::ClickEvent, _w, cx| {
                    this.resolve_permission(
                        tool_id.clone(),
                        request_id.clone(),
                        PermissionDecision::AllowWithSuggestion {
                            updated_input: input.clone(),
                            suggestion: suggestion.clone(),
                        },
                        cx,
                    )
                }),
            )
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .w_full()
        .child(
            div()
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_base)
                .child(SharedString::from(prompt)),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .items_center()
                .gap(px(density.gap_inline))
                .child(allow)
                .child(reject)
                .children(suggestion_pills),
        )
        .into_any_element()
}

/// A small accent-tinted action pill (Allow/Reject/Always/Submit/Skip). `accent`
/// colors the label and border; the fill lights up on hover. `on_click` is an
/// entity-bound listener (the output of `cx.listener`), passed straight to the
/// element. Shared with the question card.
pub(super) fn pill_button(
    id: String,
    label: impl Into<SharedString>,
    accent: gpui::Hsla,
    density: Density,
    typo: &Typography,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> AnyElement {
    div()
        .id(SharedString::from(id))
        .px(px(10.0))
        .py(px(3.0))
        .rounded(px(density.r_chip))
        .border_1()
        .border_color(accent)
        .text_size(px(typo.t_body_sm))
        .text_color(accent)
        .cursor_pointer()
        .hover(|s| s.bg(accent.opacity(0.12)))
        .on_click(on_click)
        .child(label.into())
        .into_any_element()
}

/// A one-line safety note under a `Write` diff, since its all-additions diff
/// omits whatever content is being overwritten.
fn write_replace_hint(theme: Theme, typo: &Typography) -> AnyElement {
    div()
        .w_full()
        .text_size(px(typo.t_label_xs))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(
            "Write replaces the entire file — prior content isn't shown.",
        ))
        .into_any_element()
}

/// The raw tool input as pretty JSON, framed as a muted block.
fn raw_input_block(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let json = serde_json::to_string_pretty(&tc.input).unwrap_or_default();
    labeled_block("Input", &json, theme, density, typo)
}

/// The tool result/output, capped, framed as a muted block.
fn result_block(
    result: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    labeled_block("Output", &bubble::elide(result, RESULT_CHARS), theme, density, typo)
}

fn labeled_block(
    label: &'static str,
    body: &str,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .w_full()
        .child(
            div()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .w_full()
                .rounded(px(density.r_xs))
                .bg(theme.bg_base)
                .px(px(density.pad_row))
                .py(px(4.0))
                .text_size(px(typo.t_body_sm))
                .text_color(theme.fg_muted)
                .child(SharedString::from(body.to_string())),
        )
        .into_any_element()
}
