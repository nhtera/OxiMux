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

use crate::actions::{CheckForUpdates, OpenAbout};

/// Where the Help menu's two links go. Same destinations the Windows `⋯` menu
/// offers, named here once so the two menus cannot drift.
pub const DOCS_URL: &str = "https://github.com/nhtera/OxiMux#readme";
pub const ISSUES_URL: &str = "https://github.com/nhtera/OxiMux/issues";

// Menu actions. `Quit` and the native window/app items carry handlers wired
// in `main.rs`; the Edit entries are driven by the OS via their `OsAction`
// selector, so those units only satisfy the menu API.
//
// `About` is gone from this list on purpose. It used to open AppKit's standard
// About panel; it now dispatches `actions::OpenAbout`, the same action the
// Windows menu and the palette use, so both platforms land on the one pane
// that can answer "is there a newer version" as well as "what am I running".
actions!(
    oximux,
    [
        Quit,
        /// Help → OxiMux Documentation. Handled in `main.rs`, which is where
        /// the `&mut App` needed to open a URL is available; the Windows menu
        /// opens the same URL directly from its popup item.
        OpenDocs,
        /// Help → Report an Issue.
        ReportIssue,
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
                MenuItem::action("About OxiMux", OpenAbout),
                // Directly under About and above the separator, which is where
                // macOS apps have put it since Sparkle made it a convention —
                // and where anyone looking for it will look first.
                MenuItem::action("Check for Updates…", CheckForUpdates),
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
        // macOS puts a Help menu last and expects one to exist — it is where
        // the system's own help search inserts itself. Two links rather than a
        // help book: there is no bundled documentation to open, and a menu item
        // that opens an empty help viewer is worse than one that opens the
        // README someone would have searched for anyway.
        Menu {
            name: "Help".into(),
            disabled: false,
            items: vec![
                MenuItem::action("OxiMux Documentation", OpenDocs),
                MenuItem::action("Report an Issue", ReportIssue),
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

    // `orderFrontStandardAboutPanel` used to live here. It is gone rather than
    // kept unused: the About menu item opens the in-app pane now, and a second
    // About surface that nothing reaches is how the two drift apart.

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
    pub fn hide() {}
    pub fn hide_others() {}
    pub fn show_all() {}
    pub fn minimize() {}
    pub fn zoom() {}
}
