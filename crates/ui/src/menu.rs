//! The shared shape of an overlay menu: one clickable row, and the rule
//! between groups of them.
//!
//! # Why this is one function and not six
//!
//! Six context menus grew their own `menu_row`, and they had drifted into
//! four different signatures for what paints as the same control: one took an
//! explicit `fg` for a destructive tint, three derived `fg` from an `enabled`
//! flag, one added a trailing shortcut column, and one took `impl Into<SharedString>`
//! for a label the others required to be `&'static str`. The *painted* row was
//! the same in all six — identical height, padding, radius, type size and
//! hover — so the drift was in the call shape, not the design.
//!
//! [`MenuRow`] is the union of those needs as a builder, so a caller asks for
//! exactly the variations it uses and the rest stay at the defaults every menu
//! already agreed on. `ROW_PADDING_X` was a separate `const` in thirteen files,
//! all of them `10.0`; it lives here now.
//!
//! # What is deliberately NOT here
//!
//! The menu *card* — its width, its absolute positioning against the cursor,
//! its scrim and its close-on-outside-click — stays with each menu. Those
//! genuinely differ (the tab menu hugs a chip, the stash menu hugs the cursor,
//! the rail menu is anchored to a row) and folding them together would trade
//! six honest differences for one parameter list nobody can read.

use gpui::{
    App, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement,
    SharedString, Styled, Window, div, px,
};
use oximux_settings::{Density, Theme, Typography};

/// Horizontal padding inside a menu row.
///
/// Was a private `const` in thirteen modules, every one of them `10.0`.
pub const ROW_PADDING_X: f32 = 10.0;

/// Minimum gap between a row's label and its trailing shortcut.
///
/// A literal, not `density.gap_inline` (6.0): the terminal menu — the more
/// deliberate of the two shortcut-bearing menus — used 12.0, and a refactor
/// must not move pixels. The file-tree menu reached the same layout with a
/// `flex_1` spacer and no gap; since the label already takes the slack, the
/// two render identically until a label runs long enough to fill the row, at
/// which point 12px of breathing room is what the terminal menu chose.
const SHORTCUT_GAP: f32 = 12.0;

/// A single menu row, built through [`MenuRow::new`].
///
/// Defaults match what every menu already used: enabled, no shortcut, and
/// `theme.fg_base` for the label.
pub struct MenuRow {
    id: SharedString,
    label: SharedString,
    shortcut: Option<SharedString>,
    fg: Option<Hsla>,
    enabled: bool,
    theme: Theme,
    density: Density,
    typography: Typography,
}

impl MenuRow {
    pub fn new(
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        theme: Theme,
        density: Density,
        typography: Typography,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            shortcut: None,
            fg: None,
            enabled: true,
            theme,
            density,
            typography,
        }
    }

    /// Trailing, dimmed shortcut glyphs (`⌘C`). Absent by default.
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Label colour **when enabled** — a destructive tint, typically.
    ///
    /// A disabled row ignores this and uses `fg_subtle`: "this is dangerous"
    /// and "you cannot do this" are different statements, and painting a
    /// disabled row red makes the second look like the first.
    pub fn fg(mut self, fg: Hsla) -> Self {
        self.fg = Some(fg);
        self
    }

    /// A disabled row paints dimmed and takes no pointer at all — no hover, no
    /// click handler. Enabled by default.
    ///
    /// Note that several menus prefer to OMIT an item rather than disable it;
    /// a greyed row still invites the click that teaches the user the menu
    /// lies. Use this for items whose absence would be more confusing than
    /// their being unavailable.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Build the row. `on_click` is wired to left mouse-DOWN, not click,
    /// matching every menu in the cockpit: an overlay that closes on
    /// outside-mouse-down would otherwise race its own items on mouse-up.
    pub fn build<H>(self, on_click: H) -> impl IntoElement
    where
        H: Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    {
        let MenuRow {
            id,
            label,
            shortcut,
            fg,
            enabled,
            theme,
            density,
            typography,
        } = self;
        let fg = if enabled {
            fg.unwrap_or(theme.fg_base)
        } else {
            theme.fg_subtle
        };
        let mut row = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .h(px(density.h_overlay_item))
            .px(px(ROW_PADDING_X))
            .rounded(px(density.r_xs))
            .text_size(px(typography.t_body_md))
            .text_color(fg);
        // The label takes the slack only when there is a shortcut to push to
        // the right; without one, a `flex_1` child would change nothing and
        // cost a nesting level on every row in every menu.
        row = match &shortcut {
            Some(_) => row
                .gap(px(SHORTCUT_GAP))
                .child(div().flex_1().child(label)),
            None => row.child(label),
        };
        if let Some(sc) = shortcut {
            row = row.child(
                div()
                    .text_size(px(typography.t_body_sm))
                    .text_color(theme.fg_subtle)
                    .child(sc),
            );
        }
        if enabled {
            row = row
                // Never a tinted hover, even for a destructive row: a red hover
                // background fights the focus cursor and reads as "the control
                // broke". The label carries the danger; the background does not.
                .cursor_pointer()
                .hover(move |s| s.bg(theme.hover_overlay))
                .on_mouse_down(MouseButton::Left, on_click);
        }
        row
    }
}

/// The hairline between two groups of menu rows.
///
/// Was byte-identical in every menu that had one.
pub fn separator(theme: Theme) -> impl IntoElement {
    div().h(px(1.0)).my(px(4.0)).bg(theme.border_inactive)
}
