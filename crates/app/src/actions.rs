//! Workspace-level actions, surfaced via key bindings.
//!
//! Defined as unit-struct actions via `gpui::actions!`. Bindings register
//! once at app boot (`main.rs` after `gpui_component::init`); handlers live
//! on the owning entity's root `div` via `.on_action(cx.listener(...))`.
//! GPUI dispatches actions up the element tree from the focused leaf, so
//! the focused `TerminalView` keystrokes still reach the shell while
//! `Cmd-*` combos bubble up to `MainPane`.
//!
//! See `crates/app/src/shell/main_pane.rs` for handlers.

use gpui::actions;

actions!(
    oximux,
    [
        /// Split the focused pane horizontally — new pane on the right.
        SplitHorizontal,
        /// Split the focused pane vertically — new pane on the bottom.
        SplitVertical,
        /// Close the focused pane. No-op when only one pane remains.
        ClosePane,
        /// Cycle focus to the next pane in in-order traversal.
        FocusNextPane,
    ]
);
