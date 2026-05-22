//! Split-direction enum + helpers for the pane-actions dropdown.
//!
//! Lifted from the legacy tab strip so it has no dependency on any
//! particular tab-group implementation. Consumers: `pane_actions.rs`
//! menu rendering + dispatch.

use gpui::{App, Window};

use crate::actions::{SplitDown, SplitLeft, SplitRight, SplitUp};

#[derive(Clone, Copy)]
pub enum SplitDirection {
    Right,
    Down,
    Left,
    Up,
}

impl SplitDirection {
    pub fn label(self) -> &'static str {
        match self {
            SplitDirection::Right => "Split Right",
            SplitDirection::Down => "Split Down",
            SplitDirection::Left => "Split Left",
            SplitDirection::Up => "Split Up",
        }
    }

    pub fn dispatch(self, window: &mut Window, cx: &mut App) {
        match self {
            SplitDirection::Right => window.dispatch_action(Box::new(SplitRight), cx),
            SplitDirection::Down => window.dispatch_action(Box::new(SplitDown), cx),
            SplitDirection::Left => window.dispatch_action(Box::new(SplitLeft), cx),
            SplitDirection::Up => window.dispatch_action(Box::new(SplitUp), cx),
        }
    }
}

pub fn split_icon(action: SplitDirection) -> &'static str {
    match action {
        SplitDirection::Right => "icons/arrow-right.svg",
        SplitDirection::Down => "icons/arrow-down.svg",
        SplitDirection::Left => "icons/arrow-left.svg",
        SplitDirection::Up => "icons/arrow-up.svg",
    }
}
