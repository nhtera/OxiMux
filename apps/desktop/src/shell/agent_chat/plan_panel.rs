//! Renders a `TodoWrite` tool call as a read-only plan checklist instead of a
//! generic raw-JSON tool card. The item shape (`content` + a three-state status)
//! mirrors the ACP `Plan`/`PlanEntry` model, so a future ACP `Plan` session
//! update can reuse this same renderer with no changes.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use oximux_agents::thread::{PlanEntryLite, ToolCall};
use oximux_settings::{Density, Theme, Typography};
use serde_json::Value;

/// One plan step. `content` is the step text; `status` is its lifecycle.
struct PlanItem {
    content: String,
    status: PlanStatus,
}

#[derive(Debug, PartialEq)]
enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStatus {
    fn from_wire(s: &str) -> Self {
        match s {
            "in_progress" => PlanStatus::InProgress,
            "completed" => PlanStatus::Completed,
            _ => PlanStatus::Pending,
        }
    }

    /// Status glyph + its accent color source (resolved against the theme).
    fn glyph(&self) -> &'static str {
        match self {
            PlanStatus::Pending => "○",
            PlanStatus::InProgress => "◐",
            PlanStatus::Completed => "●",
        }
    }
}

/// Parse the `TodoWrite` input (`{todos: [{content, status, activeForm}]}`) into
/// plan items. Unknown/missing statuses degrade to `Pending`; a non-array or
/// absent `todos` yields an empty list (caller falls back to a generic card).
fn parse_todos(input: &Value) -> Vec<PlanItem> {
    let Some(arr) = input.get("todos").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let content = t.get("content").and_then(Value::as_str)?.to_string();
            let status = PlanStatus::from_wire(t.get("status").and_then(Value::as_str).unwrap_or(""));
            Some(PlanItem { content, status })
        })
        .collect()
}

/// True when this tool call is a `TodoWrite` carrying at least one parseable
/// todo — the signal for `mod.rs` to route it here instead of the generic card.
pub(super) fn is_plan(tc: &ToolCall) -> bool {
    tc.name == "TodoWrite" && !parse_todos(&tc.input).is_empty()
}

/// Render an ACP `Plan` (its `PlanEntry` list) as the same checklist card the
/// `TodoWrite` path uses. The entry shape is identical (`content` + a three-state
/// status), so both feed one renderer — no ACP-specific widget.
pub(super) fn render_plan_entries(
    entries: &[PlanEntryLite],
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let items = entries
        .iter()
        .map(|e| PlanItem {
            content: e.content.clone(),
            status: PlanStatus::from_wire(&e.status),
        })
        .collect();
    render_items(items, theme, density, typo)
}

/// Render the plan as a framed checklist: a "Plan · N/M done" header, then one
/// row per step with a status glyph. Read-only — no approve/edit affordances.
pub(super) fn render_plan_card(
    tc: &ToolCall,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    render_items(parse_todos(&tc.input), theme, density, typo)
}

/// Shared checklist renderer for both the `TodoWrite` and ACP `Plan` paths.
fn render_items(
    items: Vec<PlanItem>,
    theme: Theme,
    density: Density,
    typo: &Typography,
) -> AnyElement {
    let total = items.len();
    let done = items.iter().filter(|i| i.status == PlanStatus::Completed).count();

    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .w_full()
        .rounded(px(density.r_card))
        .border_1()
        .border_color(theme.border_inactive)
        .bg(theme.bg_panel_alt)
        .p(px(density.pad_panel))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(density.gap_inline))
                .w_full()
                .text_size(px(typo.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(SharedString::from(format!("Plan · {done}/{total} done"))),
        );

    for item in items {
        let color = match item.status {
            PlanStatus::Completed => theme.status_ok,
            PlanStatus::InProgress => theme.status_info,
            PlanStatus::Pending => theme.fg_subtle,
        };
        let text_color = if item.status == PlanStatus::Completed {
            theme.fg_subtle // done steps recede
        } else {
            theme.fg_muted
        };
        card = card.child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(density.gap_inline))
                .w_full()
                .child(
                    div()
                        .flex_none()
                        .text_size(px(typo.t_body_sm))
                        .text_color(color)
                        .child(SharedString::from(item.status.glyph())),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(typo.t_body_sm))
                        .text_color(text_color)
                        .child(SharedString::from(item.content)),
                ),
        );
    }

    card.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_todowrite_statuses() {
        let items = parse_todos(&json!({"todos":[
            {"content":"first","status":"completed","activeForm":"doing first"},
            {"content":"second","status":"in_progress"},
            {"content":"third","status":"pending"},
            {"content":"fourth"},
        ]}));
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].status, PlanStatus::Completed);
        assert_eq!(items[1].status, PlanStatus::InProgress);
        assert_eq!(items[2].status, PlanStatus::Pending);
        assert_eq!(items[3].status, PlanStatus::Pending, "missing status → pending");
        assert_eq!(items[0].content, "first");
    }

    #[test]
    fn is_plan_only_for_nonempty_todowrite() {
        let tw = ToolCall::new("t1", "TodoWrite", json!({"todos":[{"content":"x","status":"pending"}]}));
        assert!(is_plan(&tw));
        assert!(!is_plan(&ToolCall::new("t2", "TodoWrite", json!({"todos":[]}))));
        assert!(!is_plan(&ToolCall::new("t3", "Bash", json!({"command":"ls"}))));
    }
}
