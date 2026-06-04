//! macOS application menu.
//!
//! Without an installed main menu, macOS titles the bold application menu
//! from the launching process (e.g. the terminal that spawned a dev build),
//! so the app surfaces under the wrong name. Installing a menu whose first
//! entry is named "OxiMux" makes the menu bar read correctly and restores
//! the standard Quit / Edit items the OS-default menu would otherwise carry.
//!
//! Built once at startup and handed to `cx.set_menus(...)` in `main.rs`.
//! The `Quit` action is dispatched by the menu and handled centrally so it
//! flows through the same graceful-shutdown path as Cmd+Q today.

use gpui::{Menu, MenuItem, OsAction, SystemMenuType, actions};

// Marker actions for menu items. The clipboard entries (Cut/Copy/Paste/
// Select All) are driven by the OS through their `OsAction` selector, so
// these units exist only to satisfy the menu API; only `Quit` carries a
// handler (wired in `main.rs`). Undo/Redo are no-ops in a terminal context
// but kept so the Edit menu has its conventional shape.
actions!(oximux, [Quit, Undo, Redo, Cut, Copy, Paste, SelectAll]);

/// The application menu bar. First entry's name ("OxiMux") becomes the
/// bold app-menu title in the macOS menu bar. v1 is macOS-only, so the
/// Services submenu is always present (no platform gate needed).
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "OxiMux".into(),
            disabled: false,
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit OxiMux", Quit),
            ],
        },
        Menu {
            name: "Edit".into(),
            disabled: false,
            items: vec![
                MenuItem::os_action("Undo", Undo, OsAction::Undo),
                MenuItem::os_action("Redo", Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::separator(),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            ],
        },
    ]
}
