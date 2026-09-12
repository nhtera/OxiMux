//! Per-workspace-row action menu — small popover anchored under the "…"
//! trigger button. Routes the user's selection back to `WorkspaceRoot`
//! via a `WeakEntity` callback, mirroring the pattern established by the
//! adapter / project pickers.
//!
//! Closed state is `open_for: None`. Open state pins the carried
//! `Workspace`, its [`RowCapabilities`], and a screen-relative `(x, y)`
//! anchor, and renders the popover.
//!
//! **What the menu offers is decided once, by a pure function.** [`menu_actions`]
//! takes the row's capabilities and returns the action list, so the gating is
//! a table of cases testable without a window — and so a primary row, which
//! IS the project's checkout, is asserted never to be offered `Delete`,
//! `Archive`, `Rename`, or `Merge`.

use std::path::Path;

use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Render,
    Styled, WeakEntity, Window, div, px, svg,
};
use oximux_core::{Project, WorkPhase, Workspace};
use oximux_settings::{Density, OpenInApp, ScriptKind, Theme, Typography};

use crate::shell::left_rail::open_in;
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

/// What a row can be asked to do. Derived once when the menu opens, consumed
/// by [`menu_actions`] and by the handlers that need the row's project.
#[derive(Debug, Clone, PartialEq)]
pub struct RowCapabilities {
    /// The row's OWN project, resolved from `workspace.project_id` — never
    /// the active one. The rail renders every project's workspaces at once,
    /// and a handler that reached for the active project would run against
    /// whichever repository happened to be selected. Every destructive
    /// handler resolves the same way; this is the copy the menu labels and
    /// the `New workspace here` action read.
    pub project: Project,
    /// The synthesized `primary:<project>` row — the project's main
    /// checkout, which goes away with the project and never on its own.
    pub is_primary: bool,
    /// An archived row: restore-or-delete, plus the two ways of finding it.
    pub is_archived: bool,
    pub scripts: ScriptAvail,
    /// Whether the project has a default branch to land the work in. The
    /// builder still refuses a merge on a primary or archived row.
    pub can_merge: bool,
    /// The `Open in ▸` entries. Empty renders no submenu at all rather than
    /// an empty one.
    pub open_in: Vec<OpenInApp>,
}

/// Width of the menu card. Wider than the project menu's: `New workspace
/// here` and `Merge into <branch>` are the longest labels in the rail.
const MENU_WIDTH: f32 = 176.0;
/// One row height. Intentionally 2px shorter than the global
/// `density.h_overlay_item = 30` because the left-rail row menu sits
/// inside an already-narrow rail and reads tighter at 28px. Document
/// the divergence locally instead of bumping the global token.
const ROW_MENU_ITEM_H: f32 = 28.0;
/// Horizontal padding inside each row.
const ROW_PADDING_X: f32 = 10.0;
/// Extra left inset for a submenu's entries, so they read as children of
/// the header above them.
const SUBMENU_INDENT: f32 = 12.0;
/// The chevron / check glyph beside a submenu row.
const GLYPH_SIZE: f32 = 12.0;
/// Y offset below the trigger button so the menu doesn't visually overlap it.
const ANCHOR_Y_OFFSET: f32 = 4.0;

/// Action the user picked from the row menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRowAction {
    RunSetup,
    Run,
    RunCleanup,
    /// Header of the `Open in ▸` submenu; expands rather than dispatching.
    OpenIn,
    CopyPath,
    /// Header of the `Move to Status ▸` submenu; expands rather than
    /// dispatching.
    MoveToStatus,
    /// Primary rows only: the project-group `+`, one right-click closer.
    NewWorkspaceHere,
    Merge,
    Rename,
    Archive,
    Unarchive,
    Delete,
}

/// The two rows that expand in place instead of acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Submenu {
    OpenIn,
    Status,
}

impl WorkspaceRowAction {
    fn label(self) -> &'static str {
        match self {
            Self::RunSetup => "Run setup",
            Self::Run => "Run",
            Self::RunCleanup => "Run cleanup",
            Self::OpenIn => "Open in",
            Self::CopyPath => "Copy Path",
            Self::MoveToStatus => "Move to Status",
            Self::NewWorkspaceHere => "New workspace here",
            // Never rendered: `Merge` is labelled with the real default-branch
            // name via `label_with`. A generic "Merge" would not say what it
            // merges into, which is the only thing the user needs to know
            // before clicking it.
            Self::Merge => "Merge",
            Self::Rename => "Rename",
            Self::Archive => "Archive",
            Self::Unarchive => "Unarchive",
            Self::Delete => "Delete",
        }
    }

    /// The row's label, given the project's default branch.
    ///
    /// `Merge` is the only action whose label depends on the project, and it
    /// has to: "Merge" alone leaves "into what?" unanswered, and the answer is
    /// the one thing worth checking before landing a branch.
    fn label_with(self, default_branch: &str) -> String {
        match self {
            Self::Merge => format!("Merge into {default_branch}"),
            other => other.label().to_string(),
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

    /// The submenu this row opens, for the two that do.
    fn submenu(self) -> Option<Submenu> {
        match self {
            Self::OpenIn => Some(Submenu::OpenIn),
            Self::MoveToStatus => Some(Submenu::Status),
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

/// Management actions on a live, non-primary row, rendered after the
/// finding rows (`Open in`, `Copy Path`) and the status writer.
const ACTIONS: &[WorkspaceRowAction] = &[
    WorkspaceRowAction::Merge,
    WorkspaceRowAction::Rename,
    WorkspaceRowAction::Archive,
    WorkspaceRowAction::Delete,
];

/// Management actions offered on an ARCHIVED row.
const ARCHIVED_ACTIONS: &[WorkspaceRowAction] =
    &[WorkspaceRowAction::Unarchive, WorkspaceRowAction::Delete];

/// The menu's action list for one row, in surface order — the pure half of
/// `Render`, so the gating is testable without a window.
///
/// - A **primary** row is the project's main checkout. It gets only what is
///   meaningful there: `Run setup`, the two finding rows, and `New workspace
///   here`. Never `Rename`, `Archive`, `Delete`, or `Merge into` — each would
///   act on the project itself. The `Run` and `Run cleanup` scripts are for
///   a worktree's lifecycle, not the repo's, so they are withheld too.
/// - An **archived** row offers the finding rows plus `Unarchive` and
///   `Delete`. The `Run *` scripts and `Rename` act on a worktree the user
///   cannot activate, `Archive` is a no-op there, and `Pin` / `Color` order
///   and tag a row that does not appear in the live list.
/// - A **live** row gets the defined scripts, the finding rows, the status
///   writer, then the management actions.
///
/// `Open in` appears only when there is at least one app to offer.
fn menu_actions(caps: &RowCapabilities) -> Vec<WorkspaceRowAction> {
    let open_in = (!caps.open_in.is_empty()).then_some(WorkspaceRowAction::OpenIn);
    let finding = open_in.into_iter().chain([WorkspaceRowAction::CopyPath]);

    if caps.is_archived {
        // A row at the project root can be archived (by the CLI, or before
        // it was recognised as primary). `Unarchive` only restores the row;
        // `Delete` on it would take the force path through the project's own
        // checkout, so the primary rule holds here too.
        return finding
            .chain(
                ARCHIVED_ACTIONS
                    .iter()
                    .copied()
                    .filter(|a| !(caps.is_primary && *a == WorkspaceRowAction::Delete)),
            )
            .collect();
    }

    let scripts = SCRIPT_ACTIONS
        .iter()
        .copied()
        .filter(|a| a.script_kind().is_some_and(|k| caps.scripts.has(k)))
        // Only `Run setup` belongs on the project's own checkout.
        .filter(|a| !caps.is_primary || *a == WorkspaceRowAction::RunSetup);

    if caps.is_primary {
        return scripts
            .chain(finding)
            .chain([WorkspaceRowAction::NewWorkspaceHere])
            .collect();
    }

    scripts
        .chain(finding)
        .chain([WorkspaceRowAction::MoveToStatus])
        .chain(ACTIONS.iter().copied())
        .filter(|a| *a != WorkspaceRowAction::Merge || caps.can_merge)
        .collect()
}

/// The open menu: the row it is for, what that row can do, and where the
/// card is anchored on screen.
#[derive(Clone)]
struct OpenState {
    workspace: Workspace,
    caps: RowCapabilities,
    x: f32,
    y: f32,
}

pub struct WorkspaceRowMenu {
    /// `None` when closed.
    open_for: Option<OpenState>,
    /// Which submenu is expanded in place, if any. Reset on every open.
    expanded: Option<Submenu>,
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
            expanded: None,
            weak_root,
            theme,
            density,
            typography,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open_for.is_some()
    }

    /// Open the menu anchored at (x, y) for the given workspace. `caps` is
    /// computed by the caller — it loads the lifecycle scripts, resolves the
    /// row's project, and reads the `Open in` list.
    pub fn open(
        &mut self,
        workspace: Workspace,
        caps: RowCapabilities,
        x: f32,
        y: f32,
        cx: &mut Context<Self>,
    ) {
        self.open_for = Some(OpenState { workspace, caps, x, y: y + ANCHOR_Y_OFFSET });
        self.expanded = None;
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.open_for = None;
        self.expanded = None;
        self.release_trigger_tooltip(cx);
        cx.notify();
    }

    /// Hand the `…` trigger's tooltip back to the rail, once this menu is
    /// really closed. Paired with the suppression in
    /// [`WorkspaceRoot::open_row_menu`].
    ///
    /// **Deferred, and re-checked.** Deferred because `close` is also called
    /// from inside `WorkspaceRoot`'s own update (`close_modal_overlays`), and
    /// reaching back into an entity that is mid-update aborts. Re-checked
    /// because opening any row menu closes the others first: without the
    /// re-check this clear would land *after* the new menu opened and
    /// un-suppress the very tooltip it exists to hide.
    fn release_trigger_tooltip(&self, cx: &mut Context<Self>) {
        let this = cx.weak_entity();
        let weak_root = self.weak_root.clone();
        cx.defer(move |cx| {
            let reopened = this
                .upgrade()
                .is_some_and(|menu| menu.read(cx).is_open());
            if reopened {
                return;
            }
            let _ = weak_root.update(cx, |root, cx| {
                root.left_rail
                    .update(cx, |rail, cx| rail.set_row_menu_open(false, cx));
            });
        });
    }

    fn dispatch(&self, action: WorkspaceRowAction, window: &mut Window, cx: &mut gpui::App) {
        let Some(state) = self.open_for.clone() else {
            return;
        };
        let workspace = state.workspace;
        let _ = self.weak_root.update(cx, |root, cx| {
            if let Some(kind) = action.script_kind() {
                root.run_workspace_script(workspace, kind, window, cx);
                return;
            }
            match action {
                WorkspaceRowAction::CopyPath => root.copy_workspace_path(&workspace, cx),
                WorkspaceRowAction::NewWorkspaceHere => {
                    root.new_workspace_in_project(state.caps.project, window, cx)
                }
                WorkspaceRowAction::Merge => {
                    root.merge_workspace_into_default(workspace, window, cx)
                }
                WorkspaceRowAction::Rename => root.request_rename_workspace(workspace, window, cx),
                WorkspaceRowAction::Archive => root.archive_workspace(workspace, cx),
                WorkspaceRowAction::Unarchive => root.unarchive_workspace(workspace, cx),
                WorkspaceRowAction::Delete => root.request_delete_workspace(workspace, window, cx),
                // Script actions handled above; submenu headers expand in
                // place (`toggle_submenu`) and never reach here.
                WorkspaceRowAction::RunSetup
                | WorkspaceRowAction::Run
                | WorkspaceRowAction::RunCleanup
                | WorkspaceRowAction::OpenIn
                | WorkspaceRowAction::MoveToStatus => {}
            }
        });
    }

    fn toggle_submenu(&mut self, which: Submenu, cx: &mut Context<Self>) {
        self.expanded = if self.expanded == Some(which) { None } else { Some(which) };
        cx.notify();
    }

    /// Hand the worktree directory to `app`. A spawn failure is the one
    /// thing worth telling the user about — the app is gone from the menu's
    /// point of view the moment it starts.
    fn dispatch_open_in(&self, app: &OpenInApp, cx: &mut gpui::App) {
        let Some(state) = self.open_for.as_ref() else {
            return;
        };
        if let Err(reason) = open_in::launch(app, Path::new(&state.workspace.worktree_path)) {
            tracing::warn!(app = %app.name, %reason, "open in: launch failed");
            let _ = self.weak_root.update(cx, |root, cx| {
                root.push_toast(
                    crate::shell::toast::ToastKind::Error,
                    format!("Open in {}: {reason}", app.name),
                    cx,
                );
            });
        }
    }

    fn dispatch_phase(&self, phase: Option<WorkPhase>, cx: &mut gpui::App) {
        let Some(state) = self.open_for.as_ref() else {
            return;
        };
        let id = state.workspace.id.clone();
        let _ = self
            .weak_root
            .update(cx, |root, cx| root.set_workspace_phase(&id, phase, cx));
    }

    /// One plain menu row: label, hover fill, and the given click handler.
    fn menu_row(
        &self,
        id: impl Into<gpui::ElementId>,
        indent: f32,
        fg: gpui::Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        let theme = self.theme;
        div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .h(px(ROW_MENU_ITEM_H))
            .pl(px(ROW_PADDING_X + indent))
            .pr(px(ROW_PADDING_X))
            .rounded(px(self.density.r_xs))
            .cursor_pointer()
            .hover(move |s| s.bg(theme.hover_overlay))
            .text_size(px(self.typography.t_body_md))
            .text_color(fg)
    }

    /// A submenu header: the label with a chevron that points right when
    /// collapsed and down when expanded. Clicking toggles the expansion —
    /// in place, under the header, rather than as a flyout: a flyout has to
    /// survive the pointer crossing the gap to reach it, and the rail is
    /// narrow enough that the gap is where the pointer usually goes.
    fn render_submenu_header(
        &self,
        ix: usize,
        action: WorkspaceRowAction,
        which: Submenu,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = self.theme;
        let expanded = self.expanded == Some(which);
        let chevron = if expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" };
        self.menu_row(("row-menu-item", ix), 0.0, theme.fg_base)
            .justify_between()
            .child(action.label())
            .child(
                svg()
                    .path(chevron)
                    .size(px(GLYPH_SIZE))
                    .text_color(theme.fg_muted)
                    .flex_shrink_0(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                    this.toggle_submenu(which, cx);
                }),
            )
            .into_any_element()
    }

    /// The `Open in ▸` entries: one row per app, in list order.
    fn render_open_in_entries(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(state) = self.open_for.as_ref() else {
            return Vec::new();
        };
        state
            .caps
            .open_in
            .iter()
            .enumerate()
            .map(|(ix, app)| {
                let app = app.clone();
                self.menu_row(("row-menu-open-in", ix), SUBMENU_INDENT, self.theme.fg_base)
                    .child(app.name.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                            this.dispatch_open_in(&app, cx);
                            this.close(cx);
                        }),
                    )
                    .into_any_element()
            })
            .collect()
    }

    /// The `Move to Status ▸` entries: `Clear` then every phase in the order
    /// work moves through them, with a check on the row's current value.
    /// `Clear` carries the check when no phase is set — the choice is
    /// radio-style, and "none" is one of the choices.
    fn render_status_entries(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let Some(state) = self.open_for.as_ref() else {
            return Vec::new();
        };
        let current = WorkPhase::parse(&state.workspace.phase);
        let theme = self.theme;
        let choices = std::iter::once((None, "Clear"))
            .chain(WorkPhase::ALL.iter().map(|p| (Some(*p), p.label())));
        choices
            .enumerate()
            .map(|(ix, (phase, label))| {
                let checked = phase == current;
                let mut row = self
                    .menu_row(("row-menu-status", ix), SUBMENU_INDENT, theme.fg_base)
                    .justify_between()
                    .child(label);
                if checked {
                    row = row.child(
                        svg()
                            .path("icons/check.svg")
                            .size(px(GLYPH_SIZE))
                            .text_color(theme.fg_base)
                            .flex_shrink_0(),
                    );
                }
                row.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &MouseDownEvent, _window, cx| {
                        this.dispatch_phase(phase, cx);
                        this.close(cx);
                    }),
                )
                .into_any_element()
            })
            .collect()
    }

    /// A single Pin / Unpin row. The label reflects the workspace's current
    /// pin state; dispatching floats (or releases) the row to the top of its
    /// project group in every sort mode. Synthesized primary rows are omitted
    /// (the primary already anchors first).
    fn render_pin_row(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(state) = self.open_for.clone() else {
            return div().into_any_element();
        };
        // Pinning orders a row within the live list; an archived row is not in it.
        if state.caps.is_primary || state.caps.is_archived {
            return div().into_any_element();
        }
        let workspace = state.workspace;
        let label = if workspace.pinned { "Unpin" } else { "Pin" };
        self.menu_row("row-menu-pin", 0.0, self.theme.fg_base)
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
        let Some(state) = self.open_for.clone() else {
            return div().into_any_element();
        };
        // Synthesized "primary:<proj>" rows aren't real workspace rows — tinting
        // them would no-op against the DB, so omit the picker entirely. Archived
        // rows are omitted too: the hue tags a row in the live list.
        if state.caps.is_primary || state.caps.is_archived {
            return div().into_any_element();
        }
        let workspace = state.workspace;
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
        oximux_settings::appearance::sync(&mut self.theme, &mut self.density, &mut self.typography, cx);
        let Some(state) = self.open_for.clone() else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let density = self.density;
        let default_branch = state.caps.project.default_branch.clone();
        let actions = menu_actions(&state.caps);
        let (x, y) = (state.x, state.y);

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
            if let Some(which) = action.submenu() {
                card = card.child(self.render_submenu_header(ix, action, which, cx));
                if self.expanded == Some(which) {
                    let entries = match which {
                        Submenu::OpenIn => self.render_open_in_entries(cx),
                        Submenu::Status => self.render_status_entries(cx),
                    };
                    card = card.children(entries);
                }
                continue;
            }
            let fg = if action.is_destructive() {
                theme.status_error
            } else {
                theme.fg_base
            };
            let row = self
                .menu_row(("row-menu-item", ix), 0.0, fg)
                .child(action.label_with(&default_branch))
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
    use WorkspaceRowAction as A;

    fn project(default_branch: &str) -> Project {
        Project {
            id: "proj-1".into(),
            name: "repo".into(),
            root_path: "/tmp/repo".into(),
            default_branch: default_branch.into(),
            created_at: String::new(),
            last_opened_at: None,
            sort_order: 0.0,
        }
    }

    fn one_app() -> Vec<OpenInApp> {
        vec![OpenInApp { name: "Finder".into(), command: "open".into() }]
    }

    const ALL_SCRIPTS: ScriptAvail = ScriptAvail { setup: true, run: true, cleanup: true };

    /// A row that can never be offered on a primary row — it would act on
    /// the project's own checkout.
    fn acts_on_the_worktree_itself(a: A) -> bool {
        matches!(a, A::Merge | A::Rename | A::Archive | A::Unarchive | A::Delete)
    }

    /// A live, mergeable row with every script defined and one app.
    fn live() -> RowCapabilities {
        RowCapabilities {
            project: project("main"),
            is_primary: false,
            is_archived: false,
            scripts: ALL_SCRIPTS,
            can_merge: true,
            open_in: one_app(),
        }
    }

    fn primary() -> RowCapabilities {
        RowCapabilities { is_primary: true, ..live() }
    }

    fn archived() -> RowCapabilities {
        RowCapabilities { is_archived: true, ..live() }
    }

    // ── the case table ──────────────────────────────────────────────────────

    /// The guard this phase exists for: a primary row IS the project's
    /// checkout, and the exact list — not merely "non-empty" — is what keeps
    /// a future action from reaching it by accident.
    #[test]
    fn a_primary_row_offers_exactly_setup_open_in_copy_path_and_new_workspace() {
        assert_eq!(
            menu_actions(&primary()),
            vec![A::RunSetup, A::OpenIn, A::CopyPath, A::NewWorkspaceHere],
        );
    }

    #[test]
    fn a_primary_row_is_never_offered_a_worktree_action() {
        for caps in [
            primary(),
            RowCapabilities { scripts: ScriptAvail::default(), ..primary() },
            RowCapabilities { open_in: Vec::new(), ..primary() },
            RowCapabilities { can_merge: false, ..primary() },
        ] {
            let actions = menu_actions(&caps);
            assert!(
                actions.iter().all(|a| !acts_on_the_worktree_itself(*a)),
                "{actions:?}"
            );
            assert!(!actions.contains(&A::MoveToStatus), "{actions:?}");
            assert!(!actions.is_empty(), "every row has at least Copy Path");
        }
    }

    /// `Run` and `Run cleanup` describe a worktree's lifecycle. The repo's
    /// own checkout gets `Run setup` only.
    #[test]
    fn a_primary_row_withholds_run_and_cleanup_even_when_defined() {
        let actions = menu_actions(&primary());
        assert!(actions.contains(&A::RunSetup));
        assert!(!actions.contains(&A::Run));
        assert!(!actions.contains(&A::RunCleanup));
    }

    #[test]
    fn a_live_row_offers_scripts_finding_status_then_management() {
        assert_eq!(
            menu_actions(&live()),
            vec![
                A::RunSetup,
                A::Run,
                A::RunCleanup,
                A::OpenIn,
                A::CopyPath,
                A::MoveToStatus,
                A::Merge,
                A::Rename,
                A::Archive,
                A::Delete,
            ],
        );
    }

    #[test]
    fn a_live_row_without_scripts_or_apps_keeps_copy_path_status_and_management() {
        let caps = RowCapabilities { scripts: ScriptAvail::default(), open_in: Vec::new(), ..live() };
        assert_eq!(
            menu_actions(&caps),
            vec![A::CopyPath, A::MoveToStatus, A::Merge, A::Rename, A::Archive, A::Delete],
        );
    }

    #[test]
    fn scripts_surface_only_when_defined() {
        let caps = RowCapabilities {
            scripts: ScriptAvail { setup: true, run: false, cleanup: true },
            ..live()
        };
        let actions = menu_actions(&caps);
        assert_eq!(&actions[..2], &[A::RunSetup, A::RunCleanup]);
        assert!(!actions.contains(&A::Run));
    }

    /// An archived row is restore-or-delete, plus the two ways of finding
    /// the directory `Archive` deliberately left on disk.
    #[test]
    fn an_archived_row_offers_finding_rows_then_unarchive_and_delete() {
        assert_eq!(
            menu_actions(&archived()),
            vec![A::OpenIn, A::CopyPath, A::Unarchive, A::Delete],
        );
        // The reduction does not depend on which scripts happen to be defined.
        let no_scripts = RowCapabilities { scripts: ScriptAvail::default(), ..archived() };
        assert_eq!(menu_actions(&no_scripts), menu_actions(&archived()));
    }

    /// An archived row that is ALSO the project's checkout — possible for a
    /// persisted row at the project root — keeps `Unarchive` (it only restores
    /// the row) and loses `Delete` (its force path would go through the
    /// project's own checkout).
    #[test]
    fn an_archived_primary_row_keeps_unarchive_but_not_delete() {
        let actions = menu_actions(&RowCapabilities { is_primary: true, ..archived() });
        assert_eq!(actions, vec![A::OpenIn, A::CopyPath, A::Unarchive]);
    }

    #[test]
    fn merge_is_offered_only_when_the_row_can_merge() {
        assert!(menu_actions(&live()).contains(&A::Merge));
        let no_default = RowCapabilities { can_merge: false, ..live() };
        assert_eq!(
            menu_actions(&no_default),
            vec![
                A::RunSetup,
                A::Run,
                A::RunCleanup,
                A::OpenIn,
                A::CopyPath,
                A::MoveToStatus,
                A::Rename,
                A::Archive,
                A::Delete,
            ],
            "the merge gate must remove ONLY the merge row"
        );
    }

    /// The gate that keeps an empty `Open in ▸` from rendering: no apps, no
    /// header. Every row kind honours it.
    #[test]
    fn an_empty_app_list_renders_no_open_in_row() {
        for caps in [live(), primary(), archived()] {
            let caps = RowCapabilities { open_in: Vec::new(), ..caps };
            assert!(!menu_actions(&caps).contains(&A::OpenIn), "{caps:?}");
        }
    }

    #[test]
    fn copy_path_is_on_every_row_kind() {
        for caps in [live(), primary(), archived()] {
            assert!(menu_actions(&caps).contains(&A::CopyPath), "{caps:?}");
        }
    }

    // ── per-action properties ───────────────────────────────────────────────

    #[test]
    fn only_delete_is_destructive() {
        for a in [
            A::RunSetup,
            A::Run,
            A::RunCleanup,
            A::OpenIn,
            A::CopyPath,
            A::MoveToStatus,
            A::NewWorkspaceHere,
            A::Merge,
            A::Rename,
            A::Archive,
            A::Unarchive,
        ] {
            assert!(!a.is_destructive(), "{a:?}");
        }
        assert!(A::Delete.is_destructive());
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(A::Rename.label(), "Rename");
        assert_eq!(A::Archive.label(), "Archive");
        assert_eq!(A::Delete.label(), "Delete");
        assert_eq!(A::Unarchive.label(), "Unarchive");
        assert_eq!(A::CopyPath.label(), "Copy Path");
        assert_eq!(A::OpenIn.label(), "Open in");
        assert_eq!(A::MoveToStatus.label(), "Move to Status");
        assert_eq!(A::NewWorkspaceHere.label(), "New workspace here");
    }

    /// The label has to name the branch. "Merge" alone leaves "into what?"
    /// unanswered, and on a rail showing several projects at once that is the
    /// only thing worth checking before clicking.
    #[test]
    fn the_merge_row_is_labelled_with_the_real_default_branch() {
        assert_eq!(A::Merge.label_with("main"), "Merge into main");
        assert_eq!(A::Merge.label_with("develop"), "Merge into develop");
        // Every other row ignores it.
        assert_eq!(A::Rename.label_with("main"), "Rename");
        assert_eq!(A::Delete.label_with("develop"), "Delete");
    }

    #[test]
    fn exactly_the_two_headers_open_a_submenu() {
        assert_eq!(A::OpenIn.submenu(), Some(Submenu::OpenIn));
        assert_eq!(A::MoveToStatus.submenu(), Some(Submenu::Status));
        for a in [A::CopyPath, A::NewWorkspaceHere, A::Merge, A::Rename, A::Delete, A::RunSetup] {
            assert_eq!(a.submenu(), None, "{a:?}");
        }
    }

    #[test]
    fn script_actions_map_to_their_kind() {
        assert_eq!(A::RunSetup.script_kind(), Some(ScriptKind::Setup));
        assert_eq!(A::Run.script_kind(), Some(ScriptKind::Run));
        assert_eq!(A::RunCleanup.script_kind(), Some(ScriptKind::Cleanup));
        for a in [A::Rename, A::Delete, A::Merge, A::CopyPath, A::OpenIn, A::MoveToStatus] {
            assert_eq!(a.script_kind(), None, "{a:?}");
        }
    }

    #[test]
    fn action_list_order_is_merge_rename_archive_delete() {
        assert_eq!(ACTIONS, &[A::Merge, A::Rename, A::Archive, A::Delete]);
    }

    /// The status writer offers the closed vocabulary in the order work moves
    /// through it — the same `WorkPhase::ALL` the CLI validates against, so
    /// there is no second list to drift.
    #[test]
    fn the_status_submenu_is_clear_then_every_phase_in_order() {
        let labels: Vec<&str> = std::iter::once("Clear")
            .chain(WorkPhase::ALL.iter().map(|p| p.label()))
            .collect();
        assert_eq!(labels, vec!["Clear", "To do", "In progress", "In review", "Done"]);
    }

    // ── the painter ─────────────────────────────────────────────────────────

    fn workspace(id: &str, phase: &str) -> Workspace {
        Workspace {
            id: id.into(),
            project_id: "proj-1".into(),
            branch_minted: false,
            name: "Fix login".into(),
            slug: "fix-login".into(),
            branch: "oximux/fix-login".into(),
            worktree_path: "/tmp/repo-wt/fix-login".into(),
            status: "active".into(),
            created_at: String::new(),
            archived_at: None,
            linked_issue: None,
            tint: None,
            sort_order: 0.0,
            pinned: false,
            comment: String::new(),
            phase: phase.into(),
        }
    }

    /// The menu paints for every row kind with each submenu expanded, through
    /// the window's real draw cycle. A duplicate element id, a borrow fault
    /// in a listener, or an svg with no colour is a runtime panic that no
    /// `cargo check` and none of the table tests above can report.
    #[gpui::test]
    fn the_menu_paints_every_row_kind_with_each_submenu_expanded(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;
        // gpui-component reads a theme global nothing in a bare test app
        // installs.
        cx.update(gpui_component::init);
        let sink: Rc<RefCell<Option<gpui::Entity<WorkspaceRowMenu>>>> = Rc::new(RefCell::new(None));
        let out = sink.clone();
        let w = cx.add_window(move |window, cx| {
            let menu = cx.new(|_cx| {
                WorkspaceRowMenu::new(
                    Theme::default(),
                    Density::default(),
                    Typography::default(),
                    // The menu has to survive a root that is gone; a test
                    // window is one.
                    gpui::WeakEntity::new_invalid(),
                )
            });
            *out.borrow_mut() = Some(menu.clone());
            let view: gpui::AnyView = menu.into();
            gpui_component::Root::new(view, window, cx)
        });
        let menu = sink.borrow().clone().expect("the menu was built");
        let mut vcx = gpui::VisualTestContext::from_window(w.into(), cx);
        vcx.simulate_resize(gpui::size(px(400.0), px(700.0)));

        let cases = [
            ("live", workspace("ws-1", "in-review"), live()),
            ("primary", workspace("primary:proj-1", ""), primary()),
            ("archived", workspace("ws-2", "done"), archived()),
        ];
        for (kind, ws, caps) in cases {
            for expanded in [None, Some(Submenu::OpenIn), Some(Submenu::Status)] {
                menu.update(&mut vcx, |m, cx| {
                    m.open(ws.clone(), caps.clone(), 20.0, 20.0, cx);
                    m.expanded = expanded;
                    cx.notify();
                });
                vcx.run_until_parked();
                menu.update(&mut vcx, |m, _cx| {
                    assert!(m.is_open(), "{kind} / {expanded:?}: still open after paint");
                });
            }
        }
        menu.update(&mut vcx, |m, cx| m.close(cx));
        vcx.run_until_parked();
        menu.update(&mut vcx, |m, _cx| assert!(!m.is_open()));
    }
}
