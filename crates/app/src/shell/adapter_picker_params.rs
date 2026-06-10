//! Model + reasoning-effort sub-step of the adapter picker.
//!
//! After the user clicks an adapter that declares model/effort options, the
//! picker swaps to this stage: a compact list of model choices (plus a
//! "Default" that passes no flag) and, when the adapter supports it, effort
//! choices, then a "Start agent" confirm row. The selection flows back to
//! the picker's `on_select` via [`AdapterPicker::confirm_params`].

use gpui::{
    AnyElement, App, Context, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, SharedString, Styled, Window, div, px,
};
use oximux_core::AgentAdapter;
use oximux_settings::{Density, Theme, Typography};

use crate::shell::adapter_picker::{AdapterPicker, RowState, card_container, picker_row};

/// "Default" option label — selecting it passes no flag (model/effort None).
const DEFAULT_LABEL: &str = "Default";
/// Hint shown on the currently-selected option row.
const SELECTED_HINT: &str = "✓";
/// Row padding so section labels align with `picker_row` text.
const LABEL_PADDING_X: f32 = 10.0;

/// State for the model/effort sub-step. Held in `AdapterPicker::stage`.
pub(super) struct ParamsStage {
    pub kind: AgentAdapter,
    pub id: &'static str,
    pub display_name: &'static str,
    pub models: &'static [&'static str],
    pub efforts: &'static [&'static str],
    /// Highlighted model option. 0 = "Default" (None); `i` > 0 = `models[i-1]`.
    pub model_sel: usize,
    /// Highlighted effort option. 0 = "Default" (None); `i` > 0 = `efforts[i-1]`.
    pub effort_sel: usize,
}

impl ParamsStage {
    /// Selected model, or `None` for the "Default" option. Uses `get` so an
    /// out-of-range `model_sel` can never panic.
    pub fn selected_model(&self) -> Option<String> {
        self.model_sel
            .checked_sub(1)
            .and_then(|i| self.models.get(i))
            .map(|m| m.to_string())
    }

    /// Selected effort, or `None` for the "Default" option.
    pub fn selected_effort(&self) -> Option<String> {
        self.effort_sel
            .checked_sub(1)
            .and_then(|i| self.efforts.get(i))
            .map(|e| e.to_string())
    }
}

/// Build the params-stage card. Falls back to an empty card when the picker
/// is somehow not in the params stage (defensive; render only calls this in
/// that stage).
pub(super) fn render_card(picker: &AdapterPicker, cx: &mut Context<AdapterPicker>) -> Div {
    let theme = picker.theme();
    let density = picker.density();
    let typography = picker.typography();
    let Some(p) = picker.params_stage() else {
        return card_container(theme, density);
    };

    let mut card = card_container(theme, density)
        .child(header_row(p.display_name, theme, density, typography.clone(), cx));

    // Model section: "Default" (index 0) then each declared model.
    card = card.child(section_label("Model", theme, typography.clone()));
    card = card.child(model_option(0, DEFAULT_LABEL, p.model_sel == 0, picker, cx));
    for (i, m) in p.models.iter().enumerate() {
        card = card.child(model_option(i + 1, m, p.model_sel == i + 1, picker, cx));
    }

    // Effort section — only when the adapter declares efforts.
    if !p.efforts.is_empty() {
        card = card.child(section_label("Effort", theme, typography.clone()));
        card = card.child(effort_option(0, DEFAULT_LABEL, p.effort_sel == 0, picker, cx));
        for (i, e) in p.efforts.iter().enumerate() {
            card = card.child(effort_option(i + 1, e, p.effort_sel == i + 1, picker, cx));
        }
    }

    // Separator + confirm row.
    card = card.child(
        div()
            .h(px(1.0))
            .bg(theme.border_inactive)
            .mx(px(4.0))
            .my(px(4.0)),
    );
    card.child(picker_row(
        "param-start",
        SharedString::from("Start agent"),
        None,
        RowState::Active,
        theme,
        density,
        typography,
        cx.listener(|this, _: &MouseDownEvent, window, cx| this.confirm_params(window, cx)),
    ))
}

/// Back-chevron + adapter name header. Clicking returns to the adapter list.
fn header_row(
    name: &str,
    theme: Theme,
    density: Density,
    typography: Typography,
    cx: &mut Context<AdapterPicker>,
) -> AnyElement {
    div()
        .id("param-back")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .h(px(density.h_overlay_item))
        .px(px(LABEL_PADDING_X))
        .rounded(px(density.r_xs))
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover_overlay))
        .text_size(px(typography.t_body_md))
        .text_color(theme.fg_base)
        .child(div().text_color(theme.fg_subtle).child("‹"))
        .child(SharedString::from(name.to_string()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _w, cx| this.back_to_list(cx)),
        )
        .into_any_element()
}

/// Small caps section label ("Model" / "Effort").
fn section_label(text: &str, theme: Theme, typography: Typography) -> AnyElement {
    div()
        .px(px(LABEL_PADDING_X))
        .pt(px(4.0))
        .text_size(px(typography.t_label_caps))
        .text_color(theme.fg_subtle)
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// One selectable model option row.
fn model_option(
    idx: usize,
    label: &str,
    selected: bool,
    picker: &AdapterPicker,
    cx: &mut Context<AdapterPicker>,
) -> AnyElement {
    option_row(
        ("param-model", idx),
        label,
        selected,
        picker,
        cx.listener(move |this, _: &MouseDownEvent, _w, cx| this.set_model_sel(idx, cx)),
    )
}

/// One selectable effort option row.
fn effort_option(
    idx: usize,
    label: &str,
    selected: bool,
    picker: &AdapterPicker,
    cx: &mut Context<AdapterPicker>,
) -> AnyElement {
    option_row(
        ("param-effort", idx),
        label,
        selected,
        picker,
        cx.listener(move |this, _: &MouseDownEvent, _w, cx| this.set_effort_sel(idx, cx)),
    )
}

/// Shared option-row builder: reuses `picker_row`, marking the selected row
/// with a checkmark hint.
fn option_row(
    id: impl Into<gpui::ElementId>,
    label: &str,
    selected: bool,
    picker: &AdapterPicker,
    on_click: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hint = selected.then(|| SharedString::from(SELECTED_HINT));
    picker_row(
        id,
        SharedString::from(label.to_string()),
        hint,
        RowState::Active,
        picker.theme(),
        picker.density(),
        picker.typography(),
        on_click,
    )
}
