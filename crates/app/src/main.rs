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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::{
    AnyView, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point,
    px, size,
};
use oximux_app::actions::{
    CloseGroup, CloseTab, DismissOverlay, FocusNextPane, FocusNextSubPane, FocusPrevPane,
    FocusPrevSubPane, MruNext, MruPrev, NewAgent, NewTab, NewWindow, NextTab, OpenCommandPalette,
    OpenCommitDialog, OpenProjectPicker, OpenQuickOpen, OpenWorkspaceCreate, PrevTab, Search,
    SelectExplorerTab, SelectSearchTab, SelectSourceControlTab, SplitSubPaneDown,
    SplitSubPaneRight, ToggleLeftSidebar, ToggleRightSidebar, ToggleZoomSubPane,
};
// SaveFile is declared in oximux-editor (not oximux-app) to avoid a circular
// crate dependency: oximux-app → oximux-editor → oximux-app would be a cycle.
use oximux_app::assets::CompositeAssets;
use oximux_app::relay_supervisor::{RelaySupervisor, SupervisorError};
use oximux_app::shell::terminal_view::install_shared_backend;
use oximux_app::state;
use oximux_app::window_factory::{open_workspace_window, open_workspace_window_with};
use oximux_editor::SaveFile;
use oximux_git::Repository;
use oximux_pty::TerminalBackend;
use oximux_relay_client::{RelayBackend, RelayClient};
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

    // `oximux notify [--title T] [--body B]` — explicit attention signal for
    // the current pane, invoked by agent hooks (Claude Code `Stop`, Codex
    // `notify`) or scripts. Reads OXIMUX_PTY_ID from the env (injected by the
    // daemon at spawn), connects to the relay, and asks it to ring that pane.
    // Short-circuits the GUI/db boot entirely.
    if std::env::args().nth(1).as_deref() == Some("notify") {
        std::process::exit(run_notify_cli(&rt));
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
            // cmd-d / cmd-shift-d trigger SUB-PANE splits inside the
            // focused terminal tab (matches common terminals and the
            // reference editor). Tab-GROUP splits remain accessible via
            // tab right-click → "Split X" and the Pane Actions "..." menu.
            KeyBinding::new("cmd-d", SplitSubPaneRight, None),
            KeyBinding::new("cmd-shift-d", SplitSubPaneDown, None),
            // cmd-w closes the active sub-pane when the focused tab has
            // multiple sub-panes; otherwise it closes the whole tab.
            // Disambiguation lives in `PaneGroup::on_close_tab`.
            KeyBinding::new("cmd-w", CloseTab, None),
            // cmd-shift-w closes the focused PANE GROUP (no-op when only
            // one group exists). Sub-pane / tab close stays on cmd-w
            // above so the muscle-memory split is consistent with the
            // reference editor's group-vs-tab tier.
            KeyBinding::new("cmd-shift-w", CloseGroup, None),
            // cmd-[ / cmd-] cycle sub-pane focus within the active tab.
            // Tab navigation lives on cmd-{ / cmd-} below.
            KeyBinding::new("cmd-]", FocusNextSubPane, None),
            KeyBinding::new("cmd-[", FocusPrevSubPane, None),
            // cmd-shift-] / cmd-shift-[ cycle GROUP focus across the
            // pane-group tree's in-order traversal. macOS shifts the
            // key to `}` / `{` post-modifier, matching the cmd-} / cmd-{
            // remap explanation below.
            KeyBinding::new("cmd-shift-}", FocusNextPane, None),
            KeyBinding::new("cmd-shift-{", FocusPrevPane, None),
            // cmd-shift-enter zooms (maximizes) the focused sub-pane; a
            // second press restores the prior layout. Matches the
            // reference editor's "zoom pane" binding.
            KeyBinding::new("cmd-shift-enter", ToggleZoomSubPane, None),
            KeyBinding::new("cmd-t", NewTab, None),
            // cmd-n opens a new top-level window (each with its own
            // WorkspaceRoot). Mirrors the terminal-app convention where
            // cmd-t is a new tab and cmd-n is a new window.
            KeyBinding::new("cmd-n", NewWindow, None),
            // macOS strips `shift` from the runtime keystroke and remaps the
            // key to the shifted character (`]`→`}`, `[`→`{`). Binding strings
            // must use the post-shift character — `cmd-shift-]` would never
            // match. Same remap the reference editor's next-tab binding uses.
            KeyBinding::new("cmd-}", NextTab, None),
            KeyBinding::new("cmd-{", PrevTab, None),
            // MRU cycle — ctrl-tab is the cross-platform standard for
            // "switch to last tab" (cmd-tab is reserved by macOS for
            // app switching).
            KeyBinding::new("ctrl-tab", MruNext, None),
            KeyBinding::new("ctrl-shift-tab", MruPrev, None),
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
            // `cmd-shift-t` previously routed to `SelectFilesTab`. The Files
            // tab is currently hidden from `visible_tabs` (see
            // shell/right_sidebar/tab.rs) so dispatching would clamp to
            // Explorer — the tooltip would advertise "Files" but the key
            // would open Explorer. Rebind when Files reappears under its
            // future label. The `SelectFilesTab` action itself stays in
            // `actions.rs` so test code and any programmatic dispatch
            // path remain intact.
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
            // Escape dismisses any open transient overlay (pane actions
            // menu, tab context menu, adapter picker). Handled at the
            // WorkspaceRoot level; other components ignore the action.
            KeyBinding::new("escape", DismissOverlay, None),
        ]);
        cx.activate(true);

        // Register the once-per-process lifecycle observers (quit-save,
        // last-window-close → quit, SIGINT/SIGTERM watchdog) plus the New
        // Window action handler, then open the first workspace window. Every
        // subsequent window is opened by that same handler through the shared
        // `open_workspace_window` factory, so each gets an independent
        // `WorkspaceRoot` and the quit / close paths treat them uniformly.
        install_app_lifecycle(cx, repo.clone(), app_state.clone());

        // Reopen the windows that were open at the last quit. An empty /
        // absent manifest (fresh install, or data from before multi-window)
        // takes the legacy single-window path: one "main" window bootstrapped
        // to the most-recent project.
        let manifest =
            oximux_app::project_panes_factory::load_windows_manifest(app_state.settings_repo());
        if manifest.windows.is_empty() {
            open_workspace_window(cx, repo, app_state);
        } else {
            // Reserve every restored id up front so a later Cmd+N can't re-mint
            // one and alias a restored window's persisted rows.
            for entry in &manifest.windows {
                oximux_app::window_registry::note_restored_id(cx, &entry.window_id);
            }
            for entry in manifest.windows {
                open_workspace_window_with(
                    cx,
                    repo.clone(),
                    app_state.clone(),
                    entry.window_id,
                    entry.project_id,
                );
            }
        }
    });
}

/// Register the once-per-process app lifecycle observers + the New Window
/// action handler. Splitting these out of the per-window `open_window` closure
/// is what makes multi-window correct: a SINGLE quit observer iterates every
/// tracked window, and a SINGLE window-closed observer decides "last window →
/// quit the app" vs. "dismiss just this window".
fn install_app_lifecycle(
    cx: &mut gpui::App,
    repo: Option<Repository>,
    app_state: oximux_app::state::AppState,
) {
    use oximux_app::window_registry;

    // Cmd+N → open another independent workspace window. Handled globally
    // (not on `WorkspaceRoot`) because opening a window needs `&mut App`.
    {
        let repo = repo.clone();
        let app_state = app_state.clone();
        cx.on_action::<NewWindow>(move |_, cx| {
            open_workspace_window(cx, repo.clone(), app_state.clone());
        });
    }

    // App quit (Cmd+Q / menu): flag shutdown BEFORE any view teardown so
    // `TerminalView::drop` leaves relay PTYs alive in the daemon for
    // next-launch reattach, then capture EVERY open window's layout +
    // scrollback + relay ids. Runs synchronously inside GPUI's grace window.
    cx.on_app_quit(move |cx| {
        oximux_app::shell::terminal_view::APP_QUITTING
            .store(true, std::sync::atomic::Ordering::SeqCst);
        window_registry::capture_session(cx);
        async {}
    })
    .detach();

    // Window close. Closing the LAST window quits the app — and saves the
    // whole session + manifest synchronously, keeping PTYs alive (belt-and-
    // braces for the race where on_app_quit is skipped, e.g. red-traffic-light
    // close followed by a Ctrl+C in the launching terminal). The last window
    // is intentionally NOT removed from the registry first, so capture_session
    // still sees it (on_app_quit re-runs the capture idempotently). Closing a
    // NON-last window dismisses just that window: dropping its registry entry
    // tears down its `WorkspaceRoot` and SIGTERMs only its own PTYs (because
    // APP_QUITTING stays clear), leaving the other windows untouched.
    cx.on_window_closed(move |cx, window_id| {
        if window_registry::remaining(cx) <= 1 {
            oximux_app::shell::terminal_view::APP_QUITTING
                .store(true, std::sync::atomic::Ordering::SeqCst);
            window_registry::capture_session(cx);
            cx.quit();
        } else if let Some(removed) = window_registry::remove(cx, window_id) {
            drop(removed);
        }
    })
    .detach();

    // SIGINT / SIGTERM handler. The signal can arrive two ways: (1) `cargo
    // run` then Ctrl+C in the launching terminal cascades SIGINT to the child;
    // (2) launchd / killall sends SIGTERM. Cocoa's terminate flow never gets a
    // chance, so on_app_quit observers don't fire. The handler flips an atomic
    // flag; a tiny background task polls it and triggers cx.quit() from inside
    // the GPUI event loop, which DOES run on_app_quit and persists state.
    install_signal_watchdog(cx);
}

/// Flag flipped by the SIGINT/SIGTERM handler. Read by the watchdog
/// poll loop installed via `install_signal_watchdog`. Static so the
/// signal handler can touch it from an async-signal-safe context
/// (any thread, no allocation, no mutex).
static SHUTDOWN_SIGNAL: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler. Stores to an atomic and returns.
/// Anything else (logging, locking, calling into GPUI) would risk
/// deadlock if the signal interrupted a critical section.
extern "C" fn handle_shutdown_signal(_sig: libc::c_int) {
    SHUTDOWN_SIGNAL.store(true, Ordering::SeqCst);
}

/// Install SIGINT + SIGTERM handlers + start a GPUI task that polls
/// the flag every 200 ms and triggers `cx.quit()` once flipped. Calling
/// quit from inside the GPUI event loop is what makes on_app_quit fire
/// — a bare process exit (e.g. SIGKILL) cannot be rescued.
fn install_signal_watchdog(cx: &mut gpui::App) {
    // SAFETY: libc::signal is async-signal-safe for installing a
    // handler. We pass a plain extern "C" fn with no captured state.
    unsafe {
        let handler = handle_shutdown_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    cx.spawn(async move |cx| {
        loop {
            if SHUTDOWN_SIGNAL.load(Ordering::SeqCst) {
                // Same as the quit/window-close hooks: preserve relay
                // PTYs across the SIGINT/SIGTERM shutdown so reattach
                // works on the next launch.
                oximux_app::shell::terminal_view::APP_QUITTING.store(true, Ordering::SeqCst);
                let _ = cx.update(|cx| cx.quit());
                break;
            }
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
        }
    })
    .detach();
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
                traffic_light_position: Some(point(px(12.), px(8.))),
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
                traffic_light_position: Some(point(px(12.), px(8.))),
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
            let view = cx.new(|cx| FileTreeView::new(tree, on_open, None, window, cx));
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

/// `oximux notify` CLI entry. Resolves the relay socket/token the same way
/// the supervisor does, connects, and sends `Request::Notify` for the pane
/// named by `OXIMUX_PTY_ID`. Returns a process exit code (0 = ok).
fn run_notify_cli(rt: &tokio::runtime::Runtime) -> i32 {
    let mut title = String::new();
    let mut body = String::new();
    let mut args = std::env::args().skip(2);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--title" => title = args.next().unwrap_or_default(),
            "--body" => body = args.next().unwrap_or_default(),
            _ => {}
        }
    }
    let pty_id = match std::env::var("OXIMUX_PTY_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("oximux notify: OXIMUX_PTY_ID not set (run inside an OxiMux terminal)");
            return 1;
        }
    };
    let (Some(data_dir), Some(home)) = (dirs::data_dir(), dirs::home_dir()) else {
        eprintln!("oximux notify: cannot resolve application data directory");
        return 1;
    };
    let supervisor = RelaySupervisor::new(
        data_dir.join(APP_DATA_SUBDIR),
        home.join("Library/Logs").join(APP_DATA_SUBDIR),
    );
    let socket = supervisor.socket_path();
    let token = match std::fs::read_to_string(supervisor.token_path()) {
        Ok(t) => t.trim().to_owned(),
        Err(err) => {
            eprintln!("oximux notify: relay not reachable ({err})");
            return 1;
        }
    };
    rt.block_on(async move {
        let client = match RelayClient::connect(&socket, &token).await {
            Ok(c) => c,
            Err(err) => {
                eprintln!("oximux notify: connect failed: {err}");
                return 1;
            }
        };
        match client
            .request(oximux_relay_proto::Request::Notify {
                pty_id,
                title,
                body,
            })
            .await
        {
            Ok(oximux_relay_proto::Response::Ok) => 0,
            Ok(other) => {
                eprintln!("oximux notify: unexpected response: {other:?}");
                1
            }
            Err(err) => {
                eprintln!("oximux notify: request failed: {err}");
                1
            }
        }
    })
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
    // Record the daemon socket so spawned shells can advertise it via
    // OXIMUX_SOCKET_PATH (lets `oximux notify` / agents dial the daemon).
    oximux_app::shell::context_env::set_relay_socket_path(
        supervisor.socket_path().to_string_lossy().into_owned(),
    );
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
