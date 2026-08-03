//! Chrome concern — window chrome: top bar, status bar, toasts, dividers, and
//! the tab context/rename menus.

pub mod divider;
pub mod rename_tab_dialog;
pub mod status_bar;
pub mod tab_context_menu;
pub mod toast;
pub mod top_bar;
pub mod whats_new;
// Windows-only custom caption buttons (min/max/close) for the frameless
// title bar; compiled everywhere, rendered only when `cfg!(windows)`.
pub mod window_controls;
