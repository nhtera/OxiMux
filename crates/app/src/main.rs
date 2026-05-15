//! OxiMux — application entry point.
//!
//! Boots GPUI + gpui-component, opens the main window, and mounts
//! `WorkspaceRoot`. Phase 0 ships only the visual shell — no interactions.

mod shell;
mod workspace_root;

use gpui::{AppContext, Bounds, WindowBounds, WindowOptions, px, size};
use tracing_subscriber::EnvFilter;
use workspace_root::WorkspaceRoot;

fn main() {
    init_tracing();

    let app = gpui_platform::application();

    app.run(move |cx| {
        gpui_component::init(cx);
        cx.activate(true);

        let window_size = size(px(1400.0), px(900.0));
        let bounds = Bounds::centered(None, window_size, cx);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.0), px(480.0))),
            ..Default::default()
        };

        let _ = cx.open_window(options, |window, cx| {
            cx.new(|cx| WorkspaceRoot::new(window, cx))
        });
    });
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,oximux=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
