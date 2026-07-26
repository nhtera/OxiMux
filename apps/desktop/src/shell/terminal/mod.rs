//! Terminal surface — the raw-PTY terminal view and everything that paints,
//! drives, or floats it.
//!
//! `terminal_view` is the entry point (the GPUI view); `terminal_canvas` +
//! `terminal_row` + `cell_metrics` + `box_drawing` do the painting;
//! `key_input` + `mouse_report` translate input; `terminal_search*` +
//! `terminal_palette` + `terminal_scrollbar` + `terminal_links` are overlays;
//! `floating_terminal*` host detached terminals; `adapter_picker` chooses the
//! agent/shell adapter. Clustered here for traversal; each submodule is
//! re-exported from `shell` so existing `crate::shell::<name>::…` paths
//! keep resolving unchanged.

pub mod adapter_picker;
pub mod box_drawing;
pub mod cell_metrics;
pub mod shell_integration;
pub mod terminal_context_menu;
pub mod floating_terminal;
pub mod floating_terminal_host;
pub mod floating_terminal_persistence;
pub mod key_input;
pub mod mouse_report;
pub mod terminal_canvas;
pub mod terminal_links;
pub mod terminal_palette;
pub mod terminal_row;
pub mod terminal_scrollbar;
pub mod terminal_search;
pub mod terminal_search_overlay;
pub mod terminal_search_state;
pub mod terminal_view;
