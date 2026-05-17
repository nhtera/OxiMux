//! OxiMux — application entry point.
//!
//! Boots GPUI + gpui-component, registers workspace key bindings, opens the
//! main window, and mounts `WorkspaceRoot`.
//!
//! Tokio runtime: built once and entered for the lifetime of `app.run`. The
//! `_rt_guard` keeps `Handle::try_current()` returning Ok in every GPUI
//! callback (panel ops, diff fetch, status poller) without each one having
//! to thread a Handle through. The drop-on-exit ordering — guard first,
//! then runtime — is intentional: GPUI returns from `app.run`, the guard
//! exits the runtime, then the runtime itself shuts down gracefully.

use gpui::{
    AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use oximux_app::actions::{
    CloseTab, FocusNextPane, FocusPrevPane, NewTab, NextTab, OpenCommitDialog, PrevTab, Search,
    SelectExplorerTab, SelectSearchTab, SelectSourceControlTab, SplitHorizontal, SplitVertical,
    ToggleLeftSidebar, ToggleRightSidebar,
};
use oximux_app::assets::CompositeAssets;
use oximux_app::workspace_root::WorkspaceRoot;
use oximux_git::Repository;
use tracing_subscriber::EnvFilter;

fn main() {
    init_tracing();

    // Boot the tokio runtime that every git op + status poller relies on.
    // Held across `app.run` so `Handle::try_current` succeeds in callbacks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _rt_guard = rt.enter();

    // Best-effort: open the repo at cwd. If we're not in a git tree, render
    // without the git column — the rest of the shell still works.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repo = match rt.block_on(Repository::open(&cwd)) {
        Ok(r) => Some(r),
        Err(err) => {
            tracing::info!(?err, "no git repository at cwd; git column hidden");
            None
        }
    };

    // `with_assets` registers our composite SVG source: local app icons
    // (e.g. `icons/git-branch.svg`) first, falling through to gpui-component's
    // bundled `IconName::*` catalog. Without this, both sets paint blank.
    let app = gpui_platform::application().with_assets(CompositeAssets);

    app.run(move |cx| {
        gpui_component::init(cx);
        // gpui-component defaults to ThemeMode::Light; flip to Dark so the
        // TabBar + future component chrome match OxiMux's dark terminal panes.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        cx.bind_keys([
            KeyBinding::new("cmd-d", SplitHorizontal, None),
            KeyBinding::new("cmd-shift-d", SplitVertical, None),
            // cmd-w closes the active tab. If it was the last tab in the pane,
            // the handler cascades into the existing ClosePane logic.
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("cmd-]", FocusNextPane, None),
            KeyBinding::new("cmd-[", FocusPrevPane, None),
            KeyBinding::new("cmd-t", NewTab, None),
            // macOS strips `shift` from the runtime keystroke and remaps the
            // key to the shifted character (`]`→`}`, `[`→`{`). Binding strings
            // must use the post-shift character — `cmd-shift-]` would never
            // match. Mirrors Zed's own `pane::ActivateNextItem` binding.
            KeyBinding::new("cmd-}", NextTab, None),
            KeyBinding::new("cmd-{", PrevTab, None),
            // cmd-f opens the per-pane scrollback search overlay. Handled
            // on the focused TerminalView's root div — when no pane is
            // focused (no editor exists yet in v1), the action no-ops.
            KeyBinding::new("cmd-f", Search, None),
            // Sidebar keybinds. cmd-shift-g rebound from OpenGitPanel
            // to SelectSourceControlTab — same destination, new routing path.
            KeyBinding::new("cmd-b", ToggleLeftSidebar, None),
            KeyBinding::new("cmd-l", ToggleRightSidebar, None),
            KeyBinding::new("cmd-shift-e", SelectExplorerTab, None),
            KeyBinding::new("cmd-shift-f", SelectSearchTab, None),
            KeyBinding::new("cmd-shift-g", SelectSourceControlTab, None),
            // cmd-k opens the commit dialog (Phase 04 attaches the handler).
            KeyBinding::new("cmd-k", OpenCommitDialog, None),
        ]);
        cx.activate(true);

        let window_size = size(px(1400.0), px(900.0));
        let bounds = Bounds::centered(None, window_size, cx);

        // Transparent unified titlebar: macOS draws traffic lights into the
        // app chrome at point(12, 12); WorkspaceRoot's 40px top_bar sits
        // beneath them with a 56px left gutter for the inset. On non-macOS,
        // `traffic_light_position` is a no-op (system titlebar still drawn).
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.0), px(480.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("OxiMux".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(12.))),
            }),
            ..Default::default()
        };

        let repo_for_window = repo.clone();
        let _ = cx.open_window(options, move |window, cx| {
            cx.new(|cx| WorkspaceRoot::new(repo_for_window, window, cx))
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
