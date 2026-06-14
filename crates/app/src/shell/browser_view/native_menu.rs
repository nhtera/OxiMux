//! Native dropdown menus for the browser toolbar (macOS).
//!
//! The toolbar's profile + page-theme pickers are real `NSMenu`s rather than
//! HTML injected into the page. A GPUI-drawn menu can't help here: the webview
//! is a native view layered *above* the GPU canvas, so anything GPUI paints
//! lands underneath it. `NSMenu` sidesteps both layers — it pops in its own
//! window that the window server composites above everything, including the
//! webview — which is also why a normal browser's menus look native.
//!
//! [`popup`] runs the menu modally (a nested run loop) and returns the chosen
//! row synchronously, so the caller can apply the result inline with the
//! `Window`/`Context` it already holds — no IPC round-trip or deferral.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel,
};
use objc2_app_kit::{NSEvent, NSMenu, NSMenuItem, NSView};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::{NSObjectProtocol, NSString};

/// Anchors a [`popup`] menu under a toolbar button (a drop-down from the
/// button's bottom edge, right-aligned to it) rather than at the mouse.
pub struct Anchor {
    /// The view the menu positions against (`rect` is in its coordinate space).
    pub view: Retained<NSView>,
    /// The button's frame in `view`'s coordinates.
    pub rect: CGRect,
    /// Whether `view` is flipped (top-left origin) vs. AppKit's default
    /// bottom-left — decides which way "below the button" is.
    pub flipped: bool,
}

/// One selectable row in a [`popup`] menu.
pub struct MenuItem {
    pub label: String,
    /// Show the native checkmark (the active profile / theme).
    pub checked: bool,
    /// Draw a separator line *above* this row (groups "New Profile…" off).
    pub separator_before: bool,
}

impl MenuItem {
    pub fn new(label: impl Into<String>, checked: bool) -> Self {
        Self { label: label.into(), checked, separator_before: false }
    }
}

/// Interior-mutable slot the action selector writes the picked tag into. The
/// menu runs modally on the main thread, so a plain `Cell` is sufficient.
struct TargetIvars {
    selected: Cell<isize>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OxiMuxBrowserMenuTarget"]
    #[ivars = TargetIvars]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        /// Action fired by the chosen item during the modal pop-up; records its
        /// tag (the row index) for [`popup`] to read once the loop returns.
        #[unsafe(method(menuItemPicked:))]
        fn menu_item_picked(&self, sender: &NSMenuItem) {
            self.ivars().selected.set(sender.tag());
        }
    }
);

impl MenuTarget {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(TargetIvars { selected: Cell::new(-1) });
        unsafe { msg_send![super(this), init] }
    }
}

/// Pop a native dropdown and block until it's dismissed. With an `anchor` the
/// menu drops from just below the triggering button (right-aligned to it, a
/// small gap under the toolbar); without one it opens at the mouse location.
/// Returns the chosen row's index, or `None` if dismissed without a pick.
///
/// `header`, when set, is a disabled title row above the items (matching the
/// in-page menu's section label). Main-thread only; returns `None` off it.
pub fn popup(anchor: Option<Anchor>, header: Option<&str>, items: &[MenuItem]) -> Option<usize> {
    let mtm = MainThreadMarker::new()?;
    let target = MenuTarget::new();

    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
    // We validate items ourselves (every real row has a target+action); leave
    // the header row disabled.
    menu.setAutoenablesItems(false);

    if let Some(text) = header {
        let head = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(text),
                None,
                &NSString::from_str(""),
            )
        };
        head.setEnabled(false);
        menu.addItem(&head);
    }

    for (i, item) in items.iter().enumerate() {
        if item.separator_before {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        let row = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&item.label),
                Some(sel!(menuItemPicked:)),
                &NSString::from_str(""),
            )
        };
        unsafe { row.setTarget(Some(&target)) };
        row.setTag(i as isize);
        row.setEnabled(true);
        if item.checked {
            // NSControlStateValueOn == 1; set raw to avoid pulling the NSCell
            // feature in just for the constant.
            let _: () = unsafe { msg_send![&row, setState: 1isize] };
        }
        menu.addItem(&row);
    }

    present(&menu, &target, anchor).map(|tag| tag as usize)
}

/// Lay out + pop a built menu and return the picked item's tag (or `None` if
/// dismissed). Shared by the flat [`popup`] and the nested [`popup_tree`].
///
/// Anchored: position the menu's top-right under the button's bottom-right,
/// dropping downward with a small gap (a button drop-down). `size` forces
/// AppKit to lay the menu out so we know its width for the right-align.
/// Unanchored: fall back to the mouse (screen) location.
fn present(menu: &NSMenu, target: &MenuTarget, anchor: Option<Anchor>) -> Option<isize> {
    const GAP: f64 = 5.0;
    let (location, in_view): (CGPoint, Option<&NSView>) = match &anchor {
        Some(a) => {
            let size = menu.size();
            let x = (a.rect.origin.x + a.rect.size.width - size.width).max(4.0);
            let y = if a.flipped {
                a.rect.origin.y + a.rect.size.height + GAP
            } else {
                a.rect.origin.y - GAP
            };
            (CGPoint::new(x, y), Some(&a.view))
        }
        None => (NSEvent::mouseLocation(), None),
    };
    let _: bool = menu.popUpMenuPositioningItem_atLocation_inView(None, location, in_view);

    let picked = target.ivars().selected.get();
    (picked >= 0).then_some(picked)
}

/// A node in a nested menu tree: either a leaf row carrying a caller-supplied
/// `id` (returned when picked) or a submenu holding more entries.
pub enum MenuEntry {
    Item { label: String, checked: bool, id: u32, separator_before: bool },
    Submenu { label: String, children: Vec<MenuEntry>, separator_before: bool },
}

impl MenuEntry {
    /// A selectable leaf; `id` is what [`popup_tree`] returns when it's chosen.
    pub fn item(label: impl Into<String>, checked: bool, id: u32) -> Self {
        Self::Item { label: label.into(), checked, id, separator_before: false }
    }

    /// A submenu row expanding to `children`.
    pub fn submenu(label: impl Into<String>, children: Vec<MenuEntry>) -> Self {
        Self::Submenu { label: label.into(), children, separator_before: false }
    }

    /// Draw a separator line above this entry (groups sections off).
    pub fn with_separator(mut self) -> Self {
        match &mut self {
            MenuEntry::Item { separator_before, .. }
            | MenuEntry::Submenu { separator_before, .. } => *separator_before = true,
        }
        self
    }
}

/// Recursively build an `NSMenu` from a [`MenuEntry`] slice. Leaf rows wire the
/// shared `target`+`menuItemPicked:` selector with their `id` as the tag;
/// submenu rows attach a child menu (no action — they just expand).
fn build_tree(
    mtm: MainThreadMarker,
    target: &MenuTarget,
    entries: &[MenuEntry],
) -> Retained<NSMenu> {
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
    menu.setAutoenablesItems(false);
    for entry in entries {
        match entry {
            MenuEntry::Item { label, checked, id, separator_before } => {
                if *separator_before {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                }
                let row = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(label),
                        Some(sel!(menuItemPicked:)),
                        &NSString::from_str(""),
                    )
                };
                unsafe { row.setTarget(Some(target)) };
                row.setTag(*id as isize);
                row.setEnabled(true);
                if *checked {
                    let _: () = unsafe { msg_send![&row, setState: 1isize] };
                }
                menu.addItem(&row);
            }
            MenuEntry::Submenu { label, children, separator_before } => {
                if *separator_before {
                    menu.addItem(&NSMenuItem::separatorItem(mtm));
                }
                let item = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(label),
                        None,
                        &NSString::from_str(""),
                    )
                };
                item.setEnabled(true);
                let child = build_tree(mtm, target, children);
                item.setSubmenu(Some(&child));
                menu.addItem(&item);
            }
        }
    }
    menu
}

/// Pop a nested dropdown (with submenus) and block until dismissed. Returns the
/// chosen leaf's `id`, or `None` if dismissed without a pick. `header`, when
/// set, is a disabled title row at the top. Main-thread only.
pub fn popup_tree(
    anchor: Option<Anchor>,
    header: Option<&str>,
    entries: &[MenuEntry],
) -> Option<u32> {
    let mtm = MainThreadMarker::new()?;
    let target = MenuTarget::new();
    let menu = build_tree(mtm, &target, entries);
    if let Some(text) = header {
        let head = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(text),
                None,
                &NSString::from_str(""),
            )
        };
        head.setEnabled(false);
        menu.insertItem_atIndex(&head, 0);
    }
    present(&menu, &target, anchor).map(|tag| tag as u32)
}
