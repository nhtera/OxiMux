//! Project-scope picker for the Tasks toolbar.
//!
//! A dropdown — "All projects" plus one row per known project — that drives
//! [`TasksView::set_scope`], turning the page from a single-repo view into a
//! cross-project issue/PR inbox. Kept in its own module so the toolbar builder
//! stays focused on the chip row.

use gpui::{Anchor, ClickEvent, Context, Entity, IntoElement, ParentElement, Window, div};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};
// A plain `Button` carrying its own menu, not `DropdownButton`: that widget
// splits the label and the chevron into two buttons and hangs the menu off the
// chevron alone, leaving the label — most of the control's width — dead to
// clicks. `dropdown_caret(true)` keeps the chevron look on a single trigger.
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use oximux_core::Project;

use crate::shell::tasks_view::{TaskScope, TasksView};

/// Build the scope dropdown: a button labeled by the active scope, opening a
/// menu of "All projects" + each known project. Selecting a row sets the scope.
pub(super) fn render_scope_picker(
    scope: &TaskScope,
    projects: &[Project],
    cx: &mut Context<TasksView>,
) -> impl IntoElement {
    let entity = cx.entity();
    let label = scope_label(scope, projects);
    let projects_owned = projects.to_vec();
    let scope_owned = scope.clone();
    Button::new("tasks-scope")
        .label(label)
        .small()
        .ghost()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopLeft, move |mut menu, window, _cx| {
            menu = menu.item(scope_item(
                window,
                &entity,
                TaskScope::All,
                "All projects".to_string(),
                matches!(scope_owned, TaskScope::All),
            ));
            for p in &projects_owned {
                let selected = matches!(&scope_owned, TaskScope::One(id) if id == &p.id);
                menu = menu.item(scope_item(
                    window,
                    &entity,
                    TaskScope::One(p.id.clone()),
                    p.name.clone(),
                    selected,
                ));
            }
            menu
        })
}

/// Button label for the active scope.
fn scope_label(scope: &TaskScope, projects: &[Project]) -> String {
    match scope {
        TaskScope::All => "All projects".to_string(),
        TaskScope::One(id) => projects
            .iter()
            .find(|p| &p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Project".to_string()),
    }
}

/// One menu row — a check-marked label that switches the scope on click. The
/// leading spaces on unselected rows keep labels aligned under the check.
fn scope_item(
    window: &mut Window,
    entity: &Entity<TasksView>,
    scope: TaskScope,
    label: String,
    selected: bool,
) -> PopupMenuItem {
    let display = if selected {
        format!("\u{2713} {label}")
    } else {
        format!("   {label}")
    };
    PopupMenuItem::element(move |_window, _cx| div().child(display.clone())).on_click(
        window.listener_for(
            entity,
            move |tv: &mut TasksView, _ev: &ClickEvent, _window, cx| {
                tv.set_scope(scope.clone(), cx);
            },
        ),
    )
}
