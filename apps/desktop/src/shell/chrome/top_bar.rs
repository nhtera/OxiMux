//! Per-column top headers — 40px strip aligned with each body column.
//!
//! Per-column header pattern: instead of one full-width chrome row across
//! the entire window, each of the three body columns (left rail / center /
//! right sidebar) renders its OWN 40px header strip. The tab strip stays
//! confined to the center column's width; collapsing a side panel naturally
//! yanks its header along.
//!
//! When the left rail is collapsed, its chrome (traffic-light gutter,
//! wordmark, left toggle button) moves to the start of the center header so
//! the toggle stays reachable. Same idea on the right: when the panel is
//! closed, the activity-bar tabs + right toggle move to the end of the
//! center header.

use gpui::prelude::FluentBuilder;
use gpui::{
    Anchor, AnyElement, Div, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Styled, Window, WindowControlArea, div, px, svg,
};
use gpui_component::{
    Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
};
use oximux_settings::{Density, Theme, Typography};

use crate::actions::{
    NewWindow, OpenQuickOpen, OpenSettings, ToggleLeftSidebar, ToggleRightSidebar, ToggleWhatsNew,
};
use crate::shell::chrome::window_controls::WindowsWindowControls;

/// Width reserved on the left for macOS traffic lights (12px inset +
/// 3 × ~14px buttons with ~6px gaps + comfortable breathing room before
/// the left-sidebar toggle button starts).
const TRAFFIC_LIGHT_GUTTER: f32 = 76.0;

/// Windows draws no traffic lights, so the wordmark leads the strip with a
/// small inset instead of the macOS gutter.
const WINDOWS_LEADING_INSET: f32 = 8.0;

/// Leading gutter for the current platform: traffic-light clearance on
/// macOS, a hair of breathing room elsewhere.
fn leading_gutter() -> f32 {
    if cfg!(target_os = "macos") {
        TRAFFIC_LIGHT_GUTTER
    } else {
        WINDOWS_LEADING_INSET
    }
}

/// Each toggle icon's hit target. Public so positioning code (e.g. the
/// Pane Actions dropdown anchor) can compute distances from the window
/// right edge without hardcoding the magic 36 in multiple places.
pub const TOGGLE_BUTTON_WIDTH: f32 = 36.0;

/// Toggle icon glyph size.
const ICON_SIZE: f32 = 16.0;

/// Header strip for the left-rail column (when open). Hosts traffic-light
/// gutter, wordmark, and the left-rail close toggle.
///
/// Wordmark is centered at y=15 inside the 30-px chrome strip; the OS
/// draws macOS traffic lights centered at y=19 over the
/// `TRAFFIC_LIGHT_GUTTER`, so the two visually align on a single row.
pub fn left_header(
    update_ready: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    // The left column's header sits on the lifted rail surface — match it
    // so the rail reads as one continuous slab from titlebar to toolbar.
    chrome_strip(theme, density, true)
        .bg(theme.bg_rail)
        .child(left_chrome_cluster(true, update_ready, theme, typography))
}

/// Header strip for the center column. Hosts any chrome bits whose
/// owning column is currently collapsed (so toggles stay reachable);
/// the center zone is a spacer (the tab strip lives in its own row
/// in `workspace_root.rs`, BELOW this header, so chip drag-reorder
/// works correctly — see the layout comment there).
#[allow(clippy::too_many_arguments)]
pub fn center_header(
    left_open: bool,
    right_open: bool,
    update_ready: bool,
    center_zone: Option<AnyElement>,
    right_tabs: Option<AnyElement>,
    // When the center column has a tab strip directly below this header,
    // suppress the header's bottom border so the two don't stack into a
    // doubled hairline — the tab strip owns the single separator.
    has_tabs: bool,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let mut row = chrome_strip(theme, density, !has_tabs);
    if !left_open {
        // Left rail is collapsed — host the chrome cluster (toggle now uses
        // the "open" icon since clicking it expands the rail).
        row = row.child(left_chrome_cluster(false, update_ready, theme, typography));
    }
    let center: AnyElement = center_zone.unwrap_or_else(|| spacer_zone().into_any_element());
    row = row.child(center);
    if !right_open {
        row = row.child(right_chrome_cluster(false, right_tabs, theme));
    }
    row
}

/// Header strip for the right-sidebar column (when open). Hosts the
/// activity-bar tab buttons (Files / Search / Source Control) at the
/// left edge of the right-sidebar column, plus the close toggle at the
/// far-right. Per-column scoping ensures the activity tabs visually
/// dock at the boundary between center and right sidebar — not at the
/// far right of the window.
pub fn right_header(
    right_tabs: Option<AnyElement>,
    theme: Theme,
    density: Density,
) -> impl IntoElement {
    chrome_strip(theme, density, true).child(right_chrome_cluster(true, right_tabs, theme))
}

fn chrome_strip(theme: Theme, density: Density, bottom_border: bool) -> Div {
    let mut strip = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(density.h_top_bar))
        .bg(theme.bg_panel);
    // The center column suppresses this border when a tab strip renders
    // directly below it — the tab strip owns the single separator, so a
    // border here would double the hairline. Side columns (no tab strip
    // below) keep it.
    if bottom_border {
        strip = strip.border_b_1().border_color(theme.border_inactive);
    }
    // Windows: with `appears_transparent` GPUI strips the native caption, so
    // the strip itself must be marked as the drag region. WM_NCHITTEST then
    // resolves it to HTCAPTION and the OS provides drag, double-click
    // maximize, Snap, and the right-click system menu natively. Interactive
    // children opt OUT of the drag region by occluding their own hitbox
    // (`.occlude()`) — the hit-test walk stops at the first opaque hitbox,
    // so an occluded button never resolves to HTCAPTION. macOS keeps native
    // NSWindow titlebar dragging and needs neither.
    if cfg!(target_os = "windows") {
        strip = strip.window_control_area(WindowControlArea::Drag);
    }
    strip
        // Double-click on the chrome row → toggle window Zoom (the green
        // traffic-light action). With `appears_transparent: true` we paint
        // our own bar over the macOS title bar, which can suppress the
        // OS-level double-click-to-zoom hit-test on some setups. Wiring it
        // explicitly here guarantees parity with native-titlebar IDEs that
        // expand to fill the work area on a chrome double-click.
        //
        // Single clicks are ignored — `click_count == 2` gates the zoom so
        // ordinary drag-window and child-element clicks still work. Child
        // interactive elements (toggle buttons, tab strips, etc.) do not
        // call `cx.stop_propagation()`, so a double-click landing on them
        // would also trigger Zoom; that's an acceptable trade-off (the
        // child's own action self-cancels under two presses) and avoids a
        // brittle hit-test exclusion list.
        .on_mouse_down(
            MouseButton::Left,
            |event: &MouseDownEvent, window: &mut Window, _cx: &mut gpui::App| {
                if event.click_count == 2 {
                    window.zoom_window();
                }
            },
        )
}

fn left_chrome_cluster(
    left_open: bool,
    update_ready: bool,
    theme: Theme,
    typography: &Typography,
) -> impl IntoElement {
    // Order: traffic gutter → wordmark → left-rail toggle. Keeping the
    // wordmark anchored left mirrors macOS native chrome.
    //
    // The wordmark uses `flex + items_center + h_full` (instead of
    // relying solely on the outer cluster's `items_center`) so its
    // text is centered along the CHROME ROW's vertical center — same
    // vertical position as the macOS traffic-light glyphs drawn at
    // `point(12, 12)`. Without the inner centering, the wordmark's
    // shrink-to-text-box behavior parks the glyphs above the row
    // center because font ascender/descender padding is asymmetric.
    // `line_height` forces the text-box to equal the chrome row height
    // (`h_top_bar`). Without this, the text's natural line-box (font
    // size × 1.2-ish default leading) is smaller than the chrome row,
    // so `items_center` on the parent centers the SMALL box — which
    // places the visible glyphs above the row's mid-line because font
    // ascender padding is asymmetric. Forcing the box to fill the row
    // height makes the glyphs sit on the row's own baseline (the
    // platform-default text positioning), aligning with the macOS
    // traffic-light glyph center.
    let row_h = px(32.0);
    let wordmark = div()
        .h(row_h)
        .line_height(row_h)
        .px(px(8.0))
        .text_size(px(typography.t_brand))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .child("OxiMux");

    let mut cluster = div()
        .flex()
        .flex_row()
        .items_center()
        .h_full()
        .flex_shrink_0()
        .child(div().w(px(leading_gutter())))
        .child(wordmark)
        // Windows has no native menu bar (GPUI's `set_menus` only stores the
        // menus there), so the app menu collapses into a `⋯` dropdown next
        // to the wordmark — the same pattern the reference app uses on
        // Windows. macOS keeps the real menu bar and skips the button.
        .when(cfg!(target_os = "windows"), |c| c.child(app_menu_button()))
        .child(toggle_button(
            left_toggle_icon(left_open),
            theme,
            ToggleSide::Left,
        ));
    if update_ready {
        cluster = cluster.child(update_pill(theme, typography));
    }
    cluster
}

/// Title-bar `⋯` application menu (Windows only). Hosts the items macOS
/// serves from its native menu bar: New Window, Settings, Quit. Edit-menu
/// items (undo/copy/paste) are deliberately absent — on Windows those are
/// plain keymap bindings with no OS menu semantics to mirror.
///
/// The occluding wrapper keeps the button clickable inside the strip's
/// `WindowControlArea::Drag` region — see the note in [`chrome_strip`].
fn app_menu_button() -> impl IntoElement {
    div().occlude().flex_shrink_0().child(
        Button::new("titlebar-app-menu")
            .ghost()
            .xsmall()
            .icon(Icon::default().path("icons/ellipsis.svg"))
            .tooltip("Menu")
            .dropdown_menu_with_anchor(
                Anchor::TopLeft,
                |menu: PopupMenu, _window: &mut Window, _cx: &mut gpui::Context<'_, PopupMenu>| {
                    menu.min_w(px(200.0))
                        .item(PopupMenuItem::new("New Window").on_click(
                            |_, window: &mut Window, cx: &mut gpui::App| {
                                window.dispatch_action(Box::new(NewWindow), cx);
                            },
                        ))
                        .item(PopupMenuItem::new("Settings…").on_click(
                            |_, window: &mut Window, cx: &mut gpui::App| {
                                window.dispatch_action(Box::new(OpenSettings), cx);
                            },
                        ))
                        .separator()
                        .item(PopupMenuItem::new("Quit OxiMux").on_click(
                            |_, window: &mut Window, cx: &mut gpui::App| {
                                window.dispatch_action(Box::new(crate::menu::Quit), cx);
                            },
                        ))
                },
            ),
    )
}

/// Reference-editor-style title-bar "Update" pill. Renders only once a new
/// version is fully downloaded, verified, and staged — never for a mere
/// "available" version, so clicking through can always deliver. Opens the
/// What's New popover (release notes + the restart button) rather than
/// restarting directly: the pill sits in busy chrome, and a restart must
/// stay one deliberate click away from a stray one.
fn update_pill(theme: Theme, typography: &Typography) -> impl IntoElement {
    let base = theme.status_info;
    div()
        .id("titlebar-update-pill")
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h(px(20.0))
        .px(px(10.0))
        .ml(px(2.0))
        .rounded_full()
        .bg(base.alpha(0.9))
        .text_size(px(typography.t_body_sm))
        .font_weight(typography.w_semibold)
        .text_color(theme.fg_base)
        .cursor_pointer()
        // Opt out of the Windows title-bar drag region (see `chrome_strip`).
        .when(cfg!(target_os = "windows"), |d| d.occlude())
        .hover(move |s| s.bg(base))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| {
                window.dispatch_action(Box::new(ToggleWhatsNew), cx);
            },
        )
        .child("Update")
}

fn right_chrome_cluster(
    right_open: bool,
    right_tabs: Option<AnyElement>,
    theme: Theme,
) -> impl IntoElement {
    // When the right sidebar is open this cluster sits inside its own
    // `right_header` strip and spans the full strip width. Activity
    // tabs dock at the LEADING (left) edge of the right column — the
    // boundary with the center column — and a flex_1 spacer pushes
    // the close toggle to the trailing edge. When the sidebar is
    // closed the cluster is appended to `center_header` after the
    // center spacer; `flex_shrink_0` keeps it intact at the right.
    //
    // Child order:
    //   open   → [activity tabs, flex_1 spacer, toggle] (tabs leading-docked)
    //   closed → [activity tabs, toggle]                (compact, no spacer)
    let zone_base = div().flex().flex_row().items_center().h_full();
    let mut zone = if right_open {
        zone_base.w_full()
    } else {
        zone_base.flex_shrink_0()
    };
    if let Some(tabs) = right_tabs {
        // Windows: the activity tabs sit inside the strip's drag region, so
        // they need an occluding wrapper to stay clickable (see
        // `chrome_strip`). The wrapper mirrors the cluster's row layout so
        // it is invisible to the flexbox.
        if cfg!(target_os = "windows") {
            zone = zone.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .occlude()
                    .child(tabs),
            );
        } else {
            zone = zone.child(tabs);
        }
    }
    if right_open {
        // Push the close toggle to the trailing edge while keeping
        // the activity tabs anchored at the column's leading edge.
        zone = zone.child(div().flex_1().h_full());
    }
    // The "..." pane-actions button used to live here; it now ships
    // inside the hoisted tab strip itself so it stays adjacent to the
    // tabs (matching the reference editor's chrome).
    zone = zone.child(toggle_button(
        right_toggle_icon(right_open),
        theme,
        ToggleSide::Right,
    ));
    // Windows: this cluster always ends at the window's top-right corner
    // (in `right_header` when the sidebar is open, appended to
    // `center_header` when closed), so it hosts the custom caption buttons.
    zone.when(cfg!(target_os = "windows"), |z| {
        z.child(WindowsWindowControls::new(theme))
    })
}

fn spacer_zone() -> impl IntoElement {
    div().flex().flex_1().h_full().min_w(px(0.0))
}

/// Width of the command-center field at its widest. Below this the field
/// shrinks with the window (`w_full` + `max_w`); the flanking space stays a
/// window-drag region.
const COMMAND_CENTER_MAX_W: f32 = 520.0;

/// Glyph size for the command-center search icon and the `⌘P` hint scale.
const COMMAND_CENTER_ICON: f32 = 12.0;

/// IDE-style "Command Center": a centered, fixed-max-width field hosted in
/// the center chrome zone that opens Quick Open on click.
///
/// Deliberately NOT the reference editor's draggable tab strip in the title
/// bar: chip drag-reorder breaks inside AppKit's title-bar drag zone (`y < 28`)
/// — see the layout note in `workspace_root.rs`. A command center is a plain
/// click target (no drag source, no drop target), so it lives here safely,
/// exactly like the toggle buttons. The wrapper is `flex_1` and only the field
/// itself takes mouse-down, so the empty flanks keep dragging the window.
pub fn command_center(
    project_label: Option<String>,
    theme: Theme,
    density: Density,
    typography: &Typography,
) -> impl IntoElement {
    let label = project_label.unwrap_or_else(|| "Search".to_string());

    let field = div()
        .id("command-center")
        .flex()
        .flex_row()
        .items_center()
        .gap(px(density.gap_inline))
        .h(px(22.0))
        .w_full()
        .max_w(px(COMMAND_CENTER_MAX_W))
        .px(px(8.0))
        .rounded(px(density.r_xs))
        .bg(theme.bg_panel_alt)
        .border_1()
        .border_color(theme.border_inactive)
        .cursor_pointer()
        // Opt out of the Windows title-bar drag region (see `chrome_strip`);
        // the flanks around the field stay draggable chrome.
        .when(cfg!(target_os = "windows"), |d| d.occlude())
        .hover(|s| s.border_color(theme.border_active))
        .on_mouse_down(
            MouseButton::Left,
            |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| {
                window.dispatch_action(Box::new(OpenQuickOpen), cx);
            },
        )
        .child(
            svg()
                .path("icons/search.svg")
                .size(px(COMMAND_CENTER_ICON))
                .flex_shrink_0()
                .text_color(theme.fg_subtle),
        )
        .child(
            div()
                .text_size(px(typography.t_body_sm))
                .text_color(theme.fg_muted)
                .child(label),
        )
        // Spacer pushes the keyboard hint to the trailing edge (search input
        // convention): icon + label lead, `⌘P` trails.
        .child(div().flex_1().h_full().min_w(px(0.0)))
        .child(
            div()
                .text_size(px(typography.t_label_xs))
                .text_color(theme.fg_subtle)
                .child(
                    crate::keymap_registry::display_chord_for("open_quick_open")
                        .unwrap_or_default(),
                ),
        );

    // `flex_1` + `justify_center` centers the field and leaves the flanks as
    // plain (draggable) chrome. `px` breathing room keeps the field clear of
    // any collapsed-rail chrome cluster prepended before it.
    div()
        .flex()
        .flex_1()
        .h_full()
        .items_center()
        .justify_center()
        .px(px(8.0))
        .min_w(px(0.0))
        .child(field)
}

#[derive(Clone, Copy)]
enum ToggleSide {
    Left,
    Right,
}

fn toggle_button(icon_path: &'static str, theme: Theme, side: ToggleSide) -> impl IntoElement {
    let glyph = svg()
        .path(icon_path)
        .size(px(ICON_SIZE))
        .text_color(theme.fg_muted);

    div()
        .w(px(TOGGLE_BUTTON_WIDTH))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        // Opt out of the Windows title-bar drag region (see `chrome_strip`).
        .when(cfg!(target_os = "windows"), |d| d.occlude())
        .on_mouse_down(
            MouseButton::Left,
            move |_: &MouseDownEvent, window: &mut Window, cx: &mut gpui::App| match side {
                ToggleSide::Left => {
                    window.dispatch_action(Box::new(ToggleLeftSidebar), cx);
                }
                ToggleSide::Right => {
                    window.dispatch_action(Box::new(ToggleRightSidebar), cx);
                }
            },
        )
        .child(glyph)
}

/// Lucide `PanelLeftClose` when open (click collapses left); `PanelLeftOpen`
/// when collapsed (click expands left). Both ship in gpui-component bundle.
pub(crate) fn left_toggle_icon(left_open: bool) -> &'static str {
    if left_open {
        "icons/panel-left-close.svg"
    } else {
        "icons/panel-left-open.svg"
    }
}

/// Mirror of `left_toggle_icon` for the right edge.
pub(crate) fn right_toggle_icon(right_open: bool) -> &'static str {
    if right_open {
        "icons/panel-right-close.svg"
    } else {
        "icons/panel-right-open.svg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_open_uses_close_icon() {
        assert_eq!(left_toggle_icon(true), "icons/panel-left-close.svg");
    }

    #[test]
    fn left_closed_uses_open_icon() {
        assert_eq!(left_toggle_icon(false), "icons/panel-left-open.svg");
    }

    #[test]
    fn right_open_uses_close_icon() {
        assert_eq!(right_toggle_icon(true), "icons/panel-right-close.svg");
    }

    #[test]
    fn right_closed_uses_open_icon() {
        assert_eq!(right_toggle_icon(false), "icons/panel-right-open.svg");
    }
}
