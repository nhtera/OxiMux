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

use std::path::PathBuf;

use gpui::{
    AnyView, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point,
    px, size,
};
use oximux_app::actions::{
    CloseTab, FocusNextPane, FocusPrevPane, NewAgent, NewTab, NextTab, OpenCommandPalette,
    OpenCommitDialog, OpenProjectPicker, OpenQuickOpen, OpenWorkspaceCreate, PrevTab, Search,
    SelectExplorerTab, SelectSearchTab, SelectSourceControlTab, SplitHorizontal, SplitVertical,
    ToggleLeftSidebar, ToggleRightSidebar,
};
// SaveFile is declared in oximux-editor (not oximux-app) to avoid a circular
// crate dependency: oximux-app → oximux-editor → oximux-app would be a cycle.
use oximux_editor::SaveFile;
use oximux_app::assets::CompositeAssets;
use oximux_app::relay_supervisor::{RelaySupervisor, SupervisorError};
use oximux_app::shell::terminal_view::install_shared_backend;
use oximux_app::state;
use oximux_app::workspace_root::WorkspaceRoot;
use oximux_git::Repository;
use oximux_pty::TerminalBackend;
use oximux_relay_client::RelayBackend;
use oximux_storage::Db;
use tracing_subscriber::EnvFilter;

/// macOS bundle identifier — anchors the on-disk data directory under
/// `~/Library/Application Support`. Must stay in lockstep with
/// `CFBundleIdentifier` in `assets/Info.plist`.
const APP_DATA_SUBDIR: &str = "dev.nhtera.oximux";
const DB_FILE_NAME: &str = "oximux.db";

fn main() {
    init_tracing();

    // Boot the tokio runtime that every git op + status poller relies on.
    // Held across `app.run` so `Handle::try_current` succeeds in callbacks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let _rt_guard = rt.enter();

    // Phase 5 step 1 spike: `--editor-spike` short-circuits the normal
    // workspace boot and opens a single editor window on this file
    // (`crates/app/src/main.rs`). The spike validates that
    // gpui-component's `code_editor()` + tree-sitter highlight works
    // before days 2-3 wire rust-analyzer into the LSP provider traits.
    // The flag is intentionally undocumented in `--help` — it ships
    // alongside the spike and disappears with it.
    if std::env::args().any(|a| a == "--editor-spike") {
        run_editor_spike();
        return;
    }

    // Phase 5 step 4 spike: `--file-tree-spike` opens a standalone window
    // mounting `FileTreeView` against the current working directory. Used
    // to validate the lazy expand / placeholder / on_open click flow
    // before step 5 wires the tree into the real workspace shell. The
    // flag is intentionally undocumented in `--help`.
    if std::env::args().any(|a| a == "--file-tree-spike") {
        run_file_tree_spike();
        return;
    }

    // Best-effort: open the repo at cwd. If we're not in a git tree, render
    // without the git column — the rest of the shell still works.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = match rt.block_on(Repository::open(&cwd)) {
        Ok(r) => Some(r),
        Err(err) => {
            tracing::info!(?err, "no git repository at cwd; git column hidden");
            None
        }
    };

    // Open SQLite + hydrate boot state. A failure here means every later
    // write would fail too, so we surface the error loudly via eprintln +
    // exit(1) rather than degrade silently. `spawn_blocking` keeps the
    // rusqlite calls off the runtime worker pool — even though we
    // immediately `block_on` it, the seam matches every other repo call
    // site in the codebase (per `crates/storage/src/repositories/mod.rs`).
    //
    // The original `db` is moved into the closure; the surviving handle
    // lives inside `AppState.db`. If `state::hydrate` panics, the
    // closure's `Db` mutex is poisoned — but the `exit(1)` in the
    // `JoinError` arm makes that path unreachable, so retry callers
    // would need to reopen `Db` from scratch.
    let db = open_db_or_exit();
    let app_state = rt.block_on(async {
        tokio::task::spawn_blocking(move || state::hydrate(db))
            .await
            .unwrap_or_else(|join_err| {
                eprintln!("oximux: hydration task panicked: {join_err}");
                std::process::exit(1);
            })
    });
    let app_state = match app_state {
        Ok(s) => s,
        Err(err) => {
            eprintln!("oximux: failed to hydrate AppState: {err}");
            std::process::exit(1);
        }
    };

    // Try to bring up the relay daemon. On success, install a shared
    // `RelayBackend` so every PTY spawn goes through the daemon and
    // survives Cmd-Q. On failure, log and continue — the app falls
    // back to per-pane `PortablePtyBackend` (today's behavior, no
    // survival, but the shell still works).
    boot_relay_supervisor(&rt, app_state.pane_relay_id_repo().clone());

    // `with_assets` registers our composite SVG source: local app icons
    // (e.g. `icons/git-branch.svg`) first, falling through to gpui-component's
    // bundled `IconName::*` catalog. Without this, both sets paint blank.
    let app = gpui_platform::application().with_assets(CompositeAssets);

    app.run(move |cx| {
        gpui_component::init(cx);
        // gpui-component defaults to ThemeMode::Light; flip to Dark so the
        // TabBar + future component chrome match OxiMux's dark terminal panes.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        // Align the gpui-component Input border palette with OxiMux's
        // charcoal theme. Default `input`/`ring` are tuned for a light
        // shadcn-style page and read as "always focused" against our deep
        // panel fill. `border_inactive` is the same color the rest of the
        // shell uses for hairline edges, and `focus_ring` is the dedicated
        // focus accent — same tokens, single source of truth.
        {
            let palette = oximux_settings::Theme::charcoal();
            let component_theme = gpui_component::Theme::global_mut(cx);
            component_theme.colors.input = palette.border_inactive;
            component_theme.colors.ring = palette.focus_ring;
        }
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
            // Command palette (Phase 05 shell). cmd-k stays bound to the
            // commit dialog, so the palette uses cmd-shift-p instead.
            KeyBinding::new("cmd-p", OpenQuickOpen, None),
            KeyBinding::new("cmd-shift-p", OpenCommandPalette, None),
            // cmd-o opens the project picker (Phase 04 step 5).
            KeyBinding::new("cmd-o", OpenProjectPicker, None),
            // cmd-shift-n opens the workspace create dialog (Phase 04 step 6).
            KeyBinding::new("cmd-shift-n", OpenWorkspaceCreate, None),
            // Cmd-shift-a spawns a new agent tab using the first available
            // built-in adapter. Throwaway stopgap until step 10 ships the
            // inline-popover adapter picker on the `+` button.
            KeyBinding::new("cmd-shift-a", NewAgent, None),
            // cmd-s saves the active editor buffer. Handled by EditorView's
            // root div via `.on_action`; no-op when no editor is focused.
            KeyBinding::new("cmd-s", SaveFile, None),
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
        let app_state_for_window = app_state.clone();
        let _ = cx.open_window(options, move |window, cx| {
            // Wrap the workspace in gpui-component's `Root`. `Root` hosts the
            // tooltip / sheet / dialog / notification overlays, which is what
            // makes `Button::tooltip(...)` (and any other component tooltip)
            // actually paint. On macOS its `window_border` shadow size is 0,
            // so it's a transparent pass-through — purely additive.
            let workspace =
                cx.new(|cx| WorkspaceRoot::new(repo_for_window, app_state_for_window, window, cx));
            // Restore last-active project so the sidebar isn't empty after
            // relaunch. Helper reads recents (ORDER BY last_opened_at DESC).
            workspace.update(cx, |root, cx| root.bootstrap_active_project(window, cx));
            // Capture every open project's pane scrollback to `pane_buffers`
            // on app quit. The on_app_quit closure runs synchronously inside
            // GPUI's grace window (default 100 ms); BLOB writes for ~10
            // panes fit comfortably. Restore on next launch reads these
            // same rows in `set_active_project` (Phase 4 step 16).
            let workspace_for_quit = workspace.clone();
            cx.on_app_quit(move |cx| {
                let root = workspace_for_quit.read(cx);
                root.capture_all_pane_buffers(cx);
                root.capture_all_pane_relay_ids(cx);
                async {}
            })
            .detach();
            let view: AnyView = workspace.into();
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        });
    });
}

/// Phase 5 step 1 spike entry point. Opens a single window mounting
/// `EditorView` on `crates/app/src/main.rs`. The DB / repo / relay /
/// workspace shell are all skipped so the spike isolates the editor +
/// (days 2-3) LSP surface.
///
/// Step-1 day-1 deliverable: window opens, file content visible, Rust
/// tree-sitter highlights render. No LSP wiring yet — that's day 2.
fn run_editor_spike() {
    use oximux_editor::EditorView;

    // Day-1 verification (success criteria checkbox in the sub-plan):
    // confirm the rt.enter guard above is in scope for callbacks. If
    // this returns Err, the spike is a NO-GO before any window opens —
    // every later LSP request via `cx.background_executor().spawn`
    // would explode on `Handle::current()`.
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tracing::info!("editor-spike: tokio runtime context confirmed"),
        Err(err) => {
            eprintln!(
                "editor-spike: NO-GO precondition failed — tokio handle not \
                 in scope inside main(): {err}. The Phase 5 design assumes \
                 rt.enter() guards `app.run`; spike cannot proceed."
            );
            std::process::exit(1);
        }
    }

    let file_path = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("crates/app/src/main.rs");
    tracing::info!(file = %file_path.display(), "editor-spike: opening file");

    let app = gpui_platform::application().with_assets(CompositeAssets);
    app.run(move |cx| {
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        {
            let palette = oximux_settings::Theme::charcoal();
            let component_theme = gpui_component::Theme::global_mut(cx);
            component_theme.colors.input = palette.border_inactive;
            component_theme.colors.ring = palette.focus_ring;
        }
        cx.activate(true);

        let window_size = size(px(1100.0), px(800.0));
        let bounds = Bounds::centered(None, window_size, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(480.0), px(320.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("OxiMux — Editor Spike".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(12.))),
            }),
            ..Default::default()
        };

        // Workspace root is the cwd — for OxiMux's own dogfood path that
        // resolves to the repo root, which is what rust-analyzer wants.
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let file_for_window = file_path.clone();
        let _ = cx.open_window(options, move |window, cx| {
            let editor = cx.new(|cx| {
                let mut v = EditorView::new(file_for_window.clone(), window, cx);
                v.attach_lsp("rust-analyzer", "rust", workspace_root.clone(), cx);
                v
            });
            let view: AnyView = editor.into();
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        });
    });
}

/// Phase 5 step 4 spike entry point. Opens a standalone window with
/// `FileTreeView` mounted against the cwd. No DB / relay / workspace
/// boot — just the file-tree pane in isolation, mirroring the
/// editor-spike pattern at line 240. The `on_open` callback is a
/// no-op `tracing::info!` until step 5 lands the real pane-split
/// handler.
fn run_file_tree_spike() {
    use oximux_app::shell::file_tree_view::{FileTreeView, OnOpenFile};
    use oximux_editor::FileTree;
    use std::sync::Arc;

    // Same tokio precondition the editor spike checks — `cx.spawn` inside
    // `FileTree::new` calls `tokio::sync::mpsc` constructors which require
    // a live runtime context.
    match tokio::runtime::Handle::try_current() {
        Ok(_) => tracing::info!("file-tree-spike: tokio runtime context confirmed"),
        Err(err) => {
            eprintln!(
                "file-tree-spike: NO-GO precondition failed — tokio handle \
                 not in scope inside main(): {err}"
            );
            std::process::exit(1);
        }
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    tracing::info!(root = %cwd.display(), "file-tree-spike: opening tree");

    let app = gpui_platform::application().with_assets(CompositeAssets);
    app.run(move |cx| {
        gpui_component::init(cx);
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        cx.activate(true);

        let window_size = size(px(400.0), px(800.0));
        let bounds = Bounds::centered(None, window_size, cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(240.0), px(320.0))),
            titlebar: Some(TitlebarOptions {
                title: Some("OxiMux — File Tree Spike".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(12.), px(12.))),
            }),
            ..Default::default()
        };

        let cwd_for_window = cwd.clone();
        let _ = cx.open_window(options, move |window, cx| {
            let tree = cx.new(|cx| FileTree::new(cwd_for_window.clone(), cx));
            let on_open: OnOpenFile = Arc::new(|path, _window, _cx| {
                tracing::info!(
                    target: "file_tree_spike",
                    "would open: {}",
                    path.display()
                );
            });
            let view = cx.new(|cx| FileTreeView::new(tree, on_open, window, cx));
            let any: AnyView = view.into();
            cx.new(|cx| gpui_component::Root::new(any, window, cx))
        });
    });
}

/// Resolve `~/Library/Application Support/dev.nhtera.oximux/oximux.db`,
/// mkdir-p the parent, and open the SQLite database. Any failure on this
/// path is fatal — `eprintln` + `exit(1)` rather than panic so the user
/// sees a one-line message instead of a Rust backtrace.
fn open_db_or_exit() -> Db {
    let Some(data_dir) = dirs::data_dir() else {
        eprintln!(
            "oximux: cannot resolve Application Support directory; \
             try setting $HOME or running outside a restrictive sandbox"
        );
        std::process::exit(1);
    };
    let db_dir = data_dir.join(APP_DATA_SUBDIR);
    if let Err(err) = std::fs::create_dir_all(&db_dir) {
        eprintln!(
            "oximux: cannot create data directory {}: {err}",
            db_dir.display()
        );
        std::process::exit(1);
    }
    let db_path = db_dir.join(DB_FILE_NAME);
    match oximux_storage::open(&db_path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!(
                "oximux: cannot open database {} (is another OxiMux instance \
                 running? if the file is corrupt, delete it to reset): {err}",
                db_path.display()
            );
            std::process::exit(1);
        }
    }
}

// Best-effort: bring up the relay daemon and install the shared
// backend. Any error here is downgraded to a warning — the app
// degrades to in-process PTYs rather than refusing to start.
fn boot_relay_supervisor(
    rt: &tokio::runtime::Runtime,
    pane_relay_id_repo: oximux_storage::PaneRelayIdRepo,
) {
    let Some(data_dir) = dirs::data_dir() else {
        tracing::warn!("no data_dir; skipping relay supervisor");
        return;
    };
    let runtime_dir = data_dir.join(APP_DATA_SUBDIR);
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("no home_dir; skipping relay supervisor");
        return;
    };
    // macOS convention: per-app logs live under ~/Library/Logs.
    let log_dir = home.join("Library/Logs").join(APP_DATA_SUBDIR);

    let supervisor = RelaySupervisor::new(runtime_dir, log_dir);
    let client = match rt.block_on(supervisor.ensure_running()) {
        Ok(c) => c,
        Err(SupervisorError::VersionMismatch) => {
            tracing::warn!("relay version mismatch; falling back to in-process PTYs");
            #[cfg(target_os = "macos")]
            {
                // Plain thread (not spawn_blocking) — we are NOT inside
                // a tokio runtime context here; this branch is sync boot
                // code reached via `block_on(ensure_running())`.
                std::thread::spawn(|| {
                    let _ = mac_notification_sys::Notification::new()
                        .title("OxiMux relay version mismatch")
                        .message("Restart OxiMux to pick up the new daemon.")
                        .send();
                });
            }
            return;
        }
        Err(SupervisorError::Other(err)) => {
            tracing::warn!(?err, "relay supervisor failed; using in-process PTYs");
            return;
        }
    };
    let server_session_id = client.server_session_id().to_owned();
    let client_arc = std::sync::Arc::new(client);

    if let Some(pid) = supervisor.read_pid() {
        let repo_for_death = pane_relay_id_repo;
        let session_for_death = server_session_id.clone();
        let _enter = rt.handle().enter();
        // JoinHandle dropped intentionally: the heartbeat task exits
        // by itself when the daemon dies, and we have no way to call
        // back into the GPUI window from this boot frame anyway.
        std::mem::drop(supervisor.watch_pid(pid, move || {
            on_relay_died(repo_for_death, session_for_death);
        }));
    } else {
        tracing::warn!("relay PID file missing; crash heartbeat disabled");
    }

    let backend = RelayBackend::new(client_arc, rt.handle().clone());
    let boxed: Box<dyn TerminalBackend> = Box::new(backend);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(boxed));
    install_shared_backend(shared);
    tracing::info!("relay supervisor up; PTYs will route through the daemon");
}

// Invoked once when the supervisor's heartbeat sees the relay PID go
// ESRCH. Both the SQLite delete and the macOS AppKit notify call are
// blocking, so off-load to `spawn_blocking`; the heartbeat caller is
// a regular async tokio task and must not stall the runtime worker.
fn on_relay_died(repo: oximux_storage::PaneRelayIdRepo, session_id: String) {
    tracing::warn!(session_id, "relay daemon died mid-session");
    tokio::task::spawn_blocking(move || {
        if let Err(err) = repo.delete_for_session(&session_id) {
            tracing::warn!(?err, "pruning pane_relay_ids for dead session failed");
        }
        #[cfg(target_os = "macos")]
        {
            let _ = mac_notification_sys::Notification::new()
                .title("OxiMux relay restarted")
                .message("Your terminals were reset. Relaunch OxiMux to recover.")
                .send();
        }
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
