//! macOS application menu.
//!
//! Without an installed main menu, macOS titles the bold application menu
//! from the launching process, so a dev build surfaces under the wrong name.
//! Installing a menu whose first entry is named "OxiMux" fixes the menu bar
//! and gives the standard macOS items (About / Hide / Quit / Window) their
//! conventional shape and keyboard shortcuts.
//!
//! Menu-item shortcuts are derived by GPUI from the keymap binding for each
//! action, so [`key_bindings`] must be installed alongside [`app_menus`] for
//! ⌘Q / ⌘H / ⌘M to display. The native items (About, Hide, Minimize, …) are
//! dispatched as GPUI actions and handled in `main.rs`, which calls into the
//! [`platform`] helpers below to invoke the AppKit responder selectors.

use gpui::{Menu, MenuItem, OsAction, SystemMenuType, actions};

// Menu actions. `Quit` and the native window/app items carry handlers wired
// in `main.rs`; the Edit entries are driven by the OS via their `OsAction`
// selector, so those units only satisfy the menu API.
actions!(
    oximux,
    [
        Quit,
        About,
        HideApp,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        Undo,
        Redo,
        Cut,
        Copy,
        Paste,
        SelectAll,
    ]
);

/// The application menu bar. First entry's name ("OxiMux") becomes the bold
/// app-menu title in the macOS menu bar. v1 is macOS-only, so the Services
/// submenu is always present (no platform gate needed).
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "OxiMux".into(),
            disabled: false,
            items: vec![
                MenuItem::action("About OxiMux", About),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide OxiMux", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
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
        Menu {
            name: "Window".into(),
            disabled: false,
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
            ],
        },
    ]
}

// The ⌘Q / ⌘H / ⌥⌘H / ⌘M chords live in the keymap registry inventory
// (Global category) with every other binding; GPUI reads them back from the
// keymap to render the menu-item glyphs.

/// AppKit responder selectors for the standard app/window menu items. GPUI's
/// menu API only natively wires the clipboard `OsAction`s, so the rest are
/// invoked here against `NSApplication` / its key window.
#[cfg(target_os = "macos")]
pub mod platform {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use std::ptr;

    unsafe fn shared_app() -> *mut AnyObject {
        msg_send![class!(NSApplication), sharedApplication]
    }

    /// Standard About panel — populated from the bundle's name / version /
    /// icon (Info.plist), so it shows "OxiMux 0.1.0" with the app icon.
    pub fn about() {
        unsafe {
            let app = shared_app();
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
            let _: () = msg_send![app, orderFrontStandardAboutPanel: ptr::null_mut::<AnyObject>()];
        }
    }

    pub fn hide() {
        unsafe {
            let app = shared_app();
            let _: () = msg_send![app, hide: ptr::null_mut::<AnyObject>()];
        }
    }

    pub fn hide_others() {
        unsafe {
            let app = shared_app();
            let _: () = msg_send![app, hideOtherApplications: ptr::null_mut::<AnyObject>()];
        }
    }

    pub fn show_all() {
        unsafe {
            let app = shared_app();
            let _: () = msg_send![app, unhideAllApplications: ptr::null_mut::<AnyObject>()];
        }
    }

    pub fn minimize() {
        unsafe {
            let app = shared_app();
            let win: *mut AnyObject = msg_send![app, keyWindow];
            if !win.is_null() {
                let _: () = msg_send![win, performMiniaturize: ptr::null_mut::<AnyObject>()];
            }
        }
    }

    pub fn zoom() {
        unsafe {
            let app = shared_app();
            let win: *mut AnyObject = msg_send![app, keyWindow];
            if !win.is_null() {
                let _: () = msg_send![win, performZoom: ptr::null_mut::<AnyObject>()];
            }
        }
    }
}

/// No-op stubs so non-macOS builds compile (v1 ships macOS-only).
#[cfg(not(target_os = "macos"))]
pub mod platform {
    pub fn about() {}
    pub fn hide() {}
    pub fn hide_others() {}
    pub fn show_all() {}
    pub fn minimize() {}
    pub fn zoom() {}
}
