//! Per-workspace-row action menu — small popover anchored under the "…"
//! trigger button. Routes the user's selection back to `WorkspaceRoot`
//! via a `WeakEntity` callback, mirroring the pattern established by the
//! adapter / project pickers.
//!
//! Closed state is `workspace: None`. Open state pins the carried
//! `Workspace` + screen-relative `(x, y)` anchor and renders the popover.

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use oximux_core::Workspace;
use oximux_settings::{Density, ScriptKind, Theme, Typography};

use crate::shell::pane_group::TabColor;
use crate::workspace_root::WorkspaceRoot;

/// Which per-project lifecycle scripts are defined for the workspace under
/// the menu — drives which Run-* rows appear. Computed at menu-open time by
/// loading `.oximux/scripts.toml` so undefined scripts surface no row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScriptAvail {
    pub setup: bool,
    pub run: bool,
    pub cleanup: bool,
}

impl ScriptAvail {
    fn has(self, kind: ScriptKind) -> bool {
        match kind {
            ScriptKind::Setup => self.setup,
            ScriptKind::Run => self.run,
            ScriptKind::Cleanup => self.cleanup,
        }
    }
}

/// Width of the menu card.
const MENU_WIDTH: f32 = 160.0;
/// One row height. Intentionally 2px shorter than the global
/// `density.h_overlay_item = 30` because the left-rail row menu sits
/// inside an already-narrow rail and reads tighter at 28px. Document
/// the divergence locally instead of bumping the global token.
const ROW_MENU_ITEM_H: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Y offset below the trigger button so the menu doesn't visually overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// Action the user picked from the row menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRowAction {
    RunSetup,
    Run,
    RunCleanup,
    Rename,
    Archive,
    Delete,
}

impl WorkspaceRowAction {
    fn label(self) -> &'static str {
        match self {
            Self::RunSetup => "Run setup",
            Self::Run => "Run",
            Self::RunCleanup => "Run cleanup",
            Self::Rename => "Rename",
            Self::Archive => "Archive",
            Self::Delete => "Delete",
        }
    }

    fn is_destructive(self) -> bool {
        matches!(self, Self::Delete)
    }

    /// The lifecycle-script kind this action runs, if it is a script action.
    fn script_kind(self) -> Option<ScriptKind> {
        match self {
            Self::RunSetup => Some(ScriptKind::Setup),
            Self::Run => Some(ScriptKind::Run),
            Self::RunCleanup => Some(ScriptKind::Cleanup),
            _ => None,
        }
    }
}

/// Script actions, in surface order. Filtered to the ones actually defined
/// for the workspace before rendering.
const SCRIPT_ACTIONS: &[WorkspaceRowAction] = &[
    WorkspaceRowAction::RunSetup,
    WorkspaceRowAction::Run,
    WorkspaceRowAction::RunCleanup,
];

/// Always-present management actions, rendered below any script actions.
const ACTIONS: &[WorkspaceRowAction] = &[
    WorkspaceRowAction::Rename,
    WorkspaceRowAction::Archive,
    WorkspaceRowAction::Delete,
];

pub struct WorkspaceRowMenu {
    /// `None` when closed; `Some` carries the target workspace, which
    /// lifecycle scripts are defined for it, and the screen-pixel anchor.
    open_for: Option<(Workspace, ScriptAvail, f32, f32)>,
    weak_root: WeakEntity<WorkspaceRoot>,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl WorkspaceRowMenu {
    pub fn new(
        theme: Theme,
        density: Density,
        typography: Typography,
        weak_root: WeakEntity<WorkspaceRoot>,
    ) -> Self {
        Self {
            open_for: None,
            weak_root,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// Open the menu anchored at (x, y) for the given workspace. `avail`
    /// is the set of lifecycle scripts defined for it (computed by the
    /// caller from `.oximux/scripts.toml`).
    pub fn open(
        &mut self,
        workspace: Workspace,
        avail: ScriptAvail,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.open_for = Some((workspace, avail, x, y + ANCHOR_Y_OFFSET));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        cx.notify();
    }

    fn dispatch(&self, action: WorkspaceRowAction, window: &mut Window, cx: &mut gpui::App) {
        let Some((workspace, ..)) = self.open_for.clone() else {
            return;
        };
        let _ = self.weak_root.update(cx, |root, cx| {
            if let Some(kind) = action.script_kind() {
                root.run_workspace_script(workspace, kind, window, cx);
                return;
            }
            match action {
                WorkspaceRowAction::Rename => root.request_rename_workspace(workspace, window, cx),
                WorkspaceRowAction::Archive => root.archive_workspace(workspace, cx),
                WorkspaceRowAction::Delete => root.request_delete_workspace(workspace, window, cx),
                // Script actions handled above.
                WorkspaceRowAction::RunSetup
                | WorkspaceRowAction::Run
                | WorkspaceRowAction::RunCleanup => {}
            }
        });
    }

    /// A single Pin / Unpin row. The label reflects the workspace's current
    /// pin state; dispatching floats (or releases) the row to the top of its
    /// project group in every sort mode. Synthesized primary rows are omitted
    /// (the primary already anchors first).
    fn render_pin_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some((workspace, ..)) = self.open_for.clone() else {
            return div().into_any_element();
        };
        if workspace.id.starts_with("primary:") {
            return div().into_any_element();
        }
        let theme = self.theme;
        let label = if workspace.pinned { "Unpin" } else { "Pin" };
        div()
            .id("row-menu-pin")
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_MENU_ITEM_H))
            .px(px(ROW_PADDING_X))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover_overlay))
            .text_size(px(self.typography.t_body_md))
            .text_color(theme.fg_base)
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    let ws = workspace.clone();
                    let _ = this
                        .weak_root
                        .update(cx, |root, cx| root.toggle_workspace_pin(ws, cx));
                    this.close(cx);
                }),
            )
            .into_any_element()
    }

    /// A "Color" swatch row (clear + the 9-swatch palette) for tagging the
    /// workspace with an identifier hue. Each swatch dispatches
    /// `set_workspace_tint` and closes the menu. The current tint gets a
    /// contrasting ring.
    fn render_color_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some((workspace, ..)) = self.open_for.clone() else {
            return div().into_any_element();
        };
        // Synthesized "primary:<proj>" rows aren't real workspace rows — tinting
        // them would no-op against the DB, so omit the picker entirely.
        if workspace.id.starts_with("primary:") {
            return div().into_any_element();
        }
        let theme = self.theme;
        let current = workspace.tint.as_deref().and_then(TabColor::from_slug);
        let id = workspace.id.clone();

        let label = div()
            .px(px(ROW_PADDING_X))
            .py(px(4.0))
            .text_size(px(self.typography.t_sub_label))
            .text_color(theme.fg_subtle)
            .child("Color");

        let mut choices: Vec<Option<TabColor>> = vec![None];
        choices.extend(TabColor::ALL.iter().copied().map(Some));

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(ROW_PADDING_X))
            .py(px(4.0));
        for (index, choice) in choices.into_iter().enumerate() {
            let selected = current == choice;
            let id_for = id.clone();
            let mut swatch = div()
                .id(("ws-tint-swatch", index))
                .w(px(14.0))
                .h(px(14.0))
                .rounded_full()
                .cursor_pointer()
                .border_1();
            swatch = match choice {
                Some(c) => {
                    let col = gpui::rgb(c.rgb());
                    let border = if selected {
                        theme.fg_base
                    } else {
                        gpui::Hsla::from(col)
                    };
                    swatch.bg(col).border_color(border)
                }
                None => swatch.border_color(if selected {
                    theme.fg_base
                } else {
                    theme.border_active
                }),
            };
            row = row.child(swatch.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    let _ = this
                        .weak_root
                        .update(cx, |root, cx| root.set_workspace_tint(&id_for, choice, cx));
                    this.close(cx);
                }),
            ));
        }
        div()
            .flex()
            .flex_col()
            .child(label)
            .child(row)
            .into_any_element()
    }
}

impl Render for WorkspaceRowMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((_, avail, x, y)) = self.open_for.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let typography = self.typography.clone();

        // Surface only the script rows that are actually defined for this
        // workspace, followed by the always-present management actions.
        let actions: Vec<WorkspaceRowAction> = SCRIPT_ACTIONS
            .iter()
            .copied()
            .filter(|a| a.script_kind().is_some_and(|k| avail.has(k)))
            .chain(ACTIONS.iter().copied())
            .collect();

        let mut card = div()
            .flex()
            .flex_col()
            .p(px(density.pad_overlay))
            .bg(theme.bg_overlay)
            .border_1()
            .border_color(theme.border_active)
            .rounded(px(density.r_card))
            .shadow_lg();

        // Pin / Unpin sits at the top — it's the most reached-for management
        // action and the label reflects the row's current pin state.
        card = card.child(self.render_pin_row(cx));

        for (ix, &action) in actions.iter().enumerate() {
            let fg = if action.is_destructive() {
                theme.status_error
            } else {
                theme.fg_base
            };
            let row = div()
                .id(("row-menu-item", ix))
                .flex()
                .flex_row()
                .items_center()
                .h(px(ROW_MENU_ITEM_H))
                .px(px(ROW_PADDING_X))
                .rounded(px(density.r_xs))
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover_overlay))
                .text_size(px(typography.t_body_md))
                .text_color(fg)
                .child(action.label())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                        this.dispatch(action, window, cx);
                        this.close(cx);
                    }),
                );
            card = card.child(row);
        }
        card = card.child(self.render_color_row(cx));

        div()
            .absolute()
            .inset_0()
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _window, cx| this.close(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(MENU_WIDTH))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(card),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_flag_only_for_delete() {
        assert!(!WorkspaceRowAction::Rename.is_destructive());
        assert!(!WorkspaceRowAction::Archive.is_destructive());
        assert!(WorkspaceRowAction::Delete.is_destructive());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(WorkspaceRowAction::Rename.label(), "Rename");
        assert_eq!(WorkspaceRowAction::Archive.label(), "Archive");
        assert_eq!(WorkspaceRowAction::Delete.label(), "Delete");
    }

    #[test]
    fn action_list_order_is_rename_archive_delete() {
        assert_eq!(
            ACTIONS,
            &[
                WorkspaceRowAction::Rename,
                WorkspaceRowAction::Archive,
                WorkspaceRowAction::Delete,
            ]
        );
    }

    #[test]
    fn script_actions_map_to_their_kind() {
        assert_eq!(
            WorkspaceRowAction::RunSetup.script_kind(),
            Some(ScriptKind::Setup)
        );
        assert_eq!(WorkspaceRowAction::Run.script_kind(), Some(ScriptKind::Run));
        assert_eq!(
            WorkspaceRowAction::RunCleanup.script_kind(),
            Some(ScriptKind::Cleanup)
        );
        assert_eq!(WorkspaceRowAction::Rename.script_kind(), None);
        assert_eq!(WorkspaceRowAction::Delete.script_kind(), None);
    }

    #[test]
    fn script_actions_are_not_destructive() {
        assert!(!WorkspaceRowAction::RunSetup.is_destructive());
        assert!(!WorkspaceRowAction::Run.is_destructive());
        assert!(!WorkspaceRowAction::RunCleanup.is_destructive());
    }

    #[test]
    fn avail_filters_to_defined_scripts_only() {
        let avail = ScriptAvail {
            setup: true,
            run: false,
            cleanup: true,
        };
        let surfaced: Vec<WorkspaceRowAction> = SCRIPT_ACTIONS
            .iter()
            .copied()
            .filter(|a| a.script_kind().is_some_and(|k| avail.has(k)))
            .collect();
        assert_eq!(
            surfaced,
            vec![WorkspaceRowAction::RunSetup, WorkspaceRowAction::RunCleanup]
        );
    }

    #[test]
    fn avail_empty_surfaces_no_script_rows() {
        let avail = ScriptAvail::default();
        let count = SCRIPT_ACTIONS
            .iter()
            .filter(|a| a.script_kind().is_some_and(|k| avail.has(k)))
            .count();
        assert_eq!(count, 0);
    }
}
