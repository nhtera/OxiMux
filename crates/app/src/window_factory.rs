//! Workspace window factory — constructs, options, registers, and opens new
//! workspace windows.
//!
//! Separated from `main.rs` so the cross-window tear-off path in
//! `WorkspaceRoot` can call `open_workspace_window_with` via an async spawn
//! without needing access to the binary crate's private functions. Both
//! `main.rs` (fresh boot + Cmd+N) and the tear-off path use this entry point.

use gpui::{
    AnyView, App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use oximux_git::Repository;

use crate::state::AppState;
use crate::workspace_root::WorkspaceRoot;

/// Build `WindowOptions` for a new workspace window. The `cascade` argument
/// offsets the frame by 32 CSS px per already-open window so stacked windows
/// stay individually grabbable.
pub fn workspace_window_options(cx: &mut App, cascade: usize) -> WindowOptions {
    let mut bounds = cx
        .primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| {
            let fallback = size(px(1400.0), px(900.0));
            Bounds::centered(None, fallback, cx)
        });
    if cascade > 0 {
        let step = px(32.0 * cascade as f32);
        bounds.origin.x = bounds.origin.x + step;
        bounds.origin.y = bounds.origin.y + step;
    }
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(720.0), px(480.0))),
        titlebar: Some(TitlebarOptions {
            title: Some("OxiMux".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(12.), px(8.))),
        }),
        ..Default::default()
    }
}

/// Open one workspace window under an explicit persist id, register it, and
/// activate a project. `restore_project = Some(id)` activates that specific
/// project (multi-window restore at boot + tear-off); `None` bootstraps the
/// most-recent project (fresh window via Cmd+N).
///
/// After registration, checks the pending tear-off mailbox
/// (`window_registry::consume_pending_tearoff`). When an entry is waiting for
/// this `window_id`, the new window mounts the torn-off relay PTY as its first
/// tab instead of spawning a fresh shell.
pub fn open_workspace_window_with(
    cx: &mut App,
    repo: Option<Repository>,
    app_state: AppState,
    window_id: String,
    restore_project: Option<String>,
) {
    let cascade = crate::window_registry::remaining(cx);
    let options = workspace_window_options(cx, cascade);
    // Consume the pending tear-off BEFORE opening the window so the build
    // closure captures it by move and avoids any re-entrant borrow on the
    // global store.
    let pending = crate::window_registry::consume_pending_tearoff(&window_id);
    let _ = cx.open_window(options, move |window, cx| {
        let workspace =
            cx.new(|cx| WorkspaceRoot::new(repo, app_state, window_id.clone(), window, cx));
        match restore_project.as_deref() {
            Some(project_id) => {
                workspace.update(cx, |root, cx| {
                    root.restore_active_project(project_id, window, cx);
                });
            }
            None => {
                workspace.update(cx, |root, cx| {
                    root.bootstrap_active_project(window, cx);
                });
            }
        }
        // If a pending tear-off is waiting, mount the torn-off tab(s) in
        // the new window after the project is activated.
        if let Some(tearoff) = pending {
            workspace.update(cx, |root, cx| {
                root.mount_pending_tearoff(tearoff, window, cx);
            });
        }
        let gpui_window_id = window.window_handle().window_id();
        crate::window_registry::register(cx, gpui_window_id, window_id, workspace.clone());
        let view: AnyView = workspace.into();
        cx.new(|cx| gpui_component::Root::new(view, window, cx))
    });
}

/// Open a fresh workspace window with a freshly-minted persist id ("main"
/// for the first, "w{n}" for subsequent). Bootstraps the most-recent project.
/// Used by `main.rs` for the initial window and the Cmd+N handler.
pub fn open_workspace_window(cx: &mut App, repo: Option<Repository>, app_state: AppState) {
    let persist_id = crate::window_registry::next_persist_id(cx);
    open_workspace_window_with(cx, repo, app_state, persist_id, None);
}
