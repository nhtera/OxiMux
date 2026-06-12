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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use gpui::{
    AnyView, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
// The global keymap lives in `oximux_app::keymap` so the binary and the
// headless keymap tests install the identical bindings. `NewWindow` is the
// one action `main` still references directly (its window-level handler).
use oximux_app::actions::NewWindow;
use oximux_app::assets::CompositeAssets;
use oximux_app::relay_supervisor::{RelaySupervisor, SupervisorError};
use oximux_app::shell::terminal_view::install_shared_backend;
use oximux_app::state;
use oximux_app::window_factory::{open_workspace_window, open_workspace_window_with};
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
    let hydrate_started = std::time::Instant::now();
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
    tracing::info!(
        elapsed_ms = hydrate_started.elapsed().as_millis() as u64,
        "boot: db open + state hydrate"
    );

    // Try to bring up the relay daemon. On success, install a shared
    // `RelayBackend` so every PTY spawn goes through the daemon and
    // survives Cmd-Q. On failure, log and continue — the app falls
    // back to per-pane `PortablePtyBackend` (today's behavior, no
    // survival, but the shell still works).
    // Held for the whole process: dropping the relay runtime would tear
    // down the client's reader/writer tasks and wedge every PTY. Kept
    // alongside `rt` until `app.run` returns on graceful quit.
    // This handshake is the one intentionally-blocking pre-paint step
    // (warm path: socket probe + hello; cold path: daemon spawn + poll).
    // Per-tab attach/spawn no longer happens pre-paint — see
    // `spawn_attach_reconcile` — so this timing line plus the panes-build
    // and reconcile lines bracket the whole boot cost story.
    let relay_boot_started = std::time::Instant::now();
    let _relay_rt = boot_relay_supervisor(app_state.pane_relay_id_repo().clone());
    tracing::info!(
        elapsed_ms = relay_boot_started.elapsed().as_millis() as u64,
        relay_up = _relay_rt.is_some(),
        "boot: relay supervisor handshake"
    );

    // `with_assets` registers our composite SVG source: local app icons
    // (e.g. `icons/git-branch.svg`) first, falling through to gpui-component's
    // bundled `IconName::*` catalog. Without this, both sets paint blank.
    let app = gpui_platform::application().with_assets(CompositeAssets);

    app.run(move |cx| {
        gpui_component::init(cx);
        // Warm the syntect grammar + theme sets (~30 ms) during idle boot
        // so the first diff paint never pays the lazy-init cost.
        cx.background_executor()
            .spawn(async {
                oximux_app::shell::diff_view::syntax::prewarm();
            })
            .detach();
        // gpui-component defaults to ThemeMode::Light; flip to Dark so the
        // TabBar + future component chrome match OxiMux's dark terminal panes.
        gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
        // Align the gpui-component Input border palette with OxiMux's
        // charcoal theme. Default `input`/`ring` are tuned for a light
        // shadcn-style page and read as "always focused" against our deep
        // panel fill. Inputs rest on `border_input` (alpha-white, stronger
        // than the hairline dividers so the type-here affordance reads),
        // and `focus_ring` is the dedicated focus accent — same tokens,
        // single source of truth.
        {
            let palette = oximux_settings::Theme::charcoal();
            // Read the OS auto-hide preference before taking the mutable global
            // borrow below (can't call `cx` methods while `component_theme`
            // holds it). A user who set "Always show scrollbars" (an
            // accessibility choice some low-vision users rely on) reports
            // `false` here.
            let auto_hide_scrollbars = cx.should_auto_hide_scrollbars();
            let component_theme = gpui_component::Theme::global_mut(cx);
            component_theme.colors.input = palette.border_input;
            component_theme.colors.ring = palette.focus_ring;
            // List scrollbars stay invisible until the pointer enters the
            // scroll region, then reveal thumb-only — quiet at rest, no
            // persistent rail chrome. The library default (`Scrolling`) only
            // shows the bar mid-scroll; `Hover` matches the benchmark's calmer
            // "appears on approach" behavior. One global = every
            // `vertical_scrollbar` call site inherits it. Skip the override
            // when the user asked for always-visible scrollbars — leave the
            // appearance gpui-component already synced from the system.
            if auto_hide_scrollbars {
                component_theme.scrollbar_show = gpui_component::scroll::ScrollbarShow::Hover;
            }
        }
        // Load user terminal settings into a global + start the live-reload
        // watcher BEFORE any window opens so the first pane reads real values.
        oximux_app::terminal_settings::install(cx);
        // Same install pattern for AI commit-message settings — the sparkles
        // button reads the global at click time, so loading here means the
        // first click after launch sees the user's configured mode (heuristic
        // by default; agent when configured).
        oximux_app::commit_message_ai_settings::install(cx);
        // Resolve the reduced-motion preference once and install the Motion
        // global before any window opens, so the first animated surface reads
        // the right durations.
        oximux_app::motion_settings::install(cx);
        // Process-wide last-known-`GitState` cache. Registered before any
        // window opens so the first SCM panel can seed from it (no-op on a
        // cold start; populated as each project's poller produces a sample,
        // then read back when the user switches between already-visited
        // projects to paint the prior snapshot instead of "Loading…").
        // Rehydrate the last-known git states from disk so the first SCM
        // poller seeds from them (stale-while-revalidate) — the prior state
        // paints instantly on open instead of "loading git…". On a fresh
        // install the blob is absent → empty cache → normal Loading flash.
        cx.set_global(oximux_app::git_state_cache::GitStateCache::load_from(
            app_state.settings_repo(),
        ));
        // Install the full keymap (registry defaults ⊕ keybindings.toml
        // overrides) — this covers the menu-action chords too, and MUST run
        // before `set_menus`: GPUI reads the keymap when it builds the menu
        // to render each item's ⌘ glyph, and never re-reads it for a static
        // menu. Bind first, then build.
        oximux_app::keybindings_settings::install(cx);
        // Install the application menu so the macOS menu bar reads "OxiMux"
        // (not the launching process's name) and carries the standard
        // About / Hide / Quit / Window items. `Quit` routes through
        // `cx.quit()` so it shares the graceful-shutdown path; the native
        // items defer to AppKit selectors.
        cx.set_menus(oximux_app::menu::app_menus());
        cx.on_action::<oximux_app::menu::Quit>(|_, cx| cx.quit());
        cx.on_action::<oximux_app::menu::About>(|_, _cx| oximux_app::menu::platform::about());
        cx.on_action::<oximux_app::menu::HideApp>(|_, _cx| oximux_app::menu::platform::hide());
        cx.on_action::<oximux_app::menu::HideOthers>(|_, _cx| {
            oximux_app::menu::platform::hide_others()
        });
        cx.on_action::<oximux_app::menu::ShowAll>(|_, _cx| oximux_app::menu::platform::show_all());
        cx.on_action::<oximux_app::menu::Minimize>(|_, _cx| oximux_app::menu::platform::minimize());
        cx.on_action::<oximux_app::menu::Zoom>(|_, _cx| oximux_app::menu::platform::zoom());
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
    let quit_settings_repo = app_state.settings_repo().clone();
    cx.on_app_quit(move |cx| {
        oximux_app::shell::terminal_view::APP_QUITTING
            .store(true, std::sync::atomic::Ordering::SeqCst);
        window_registry::capture_session(cx);
        // Persist the last-known git states so the next launch seeds from
        // them instead of flashing "loading git…". Best-effort: a write
        // error only costs one Loading flash next time.
        if let Some(cache) =
            cx.try_global::<oximux_app::git_state_cache::GitStateCache>()
        {
            cache.save_to(&quit_settings_repo);
        }
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

/// Count of shutdown signals received. The first drives the graceful
/// `cx.quit()` path; the second escalates to a hard kill so a wedged
/// foreground executor can never leave the app un-quittable.
static SHUTDOWN_SIGNAL_COUNT: AtomicU32 = AtomicU32::new(0);

/// How long the graceful `cx.quit()` path gets after the first shutdown
/// signal before the deadline thread force-exits. A successful quit tears
/// the process down well within this window (~400 ms observed).
const SHUTDOWN_GRACE_DEADLINE: Duration = Duration::from_secs(3);

/// Whether a shutdown signal should escalate to a hard kill. `prior_count`
/// is the number of signals seen BEFORE this one: the first (0) drives the
/// graceful path; any repeat (>= 1) means graceful is likely wedged, so we
/// restore the default disposition and re-raise.
fn signal_should_force_exit(prior_count: u32) -> bool {
    prior_count >= 1
}

/// Async-signal-safe handler. Stores to an atomic and returns.
/// Anything else (logging, locking, calling into GPUI) would risk
/// deadlock if the signal interrupted a critical section.
///
/// Escalation: the graceful path (set flag → GPUI task calls `cx.quit()`)
/// only works while the foreground executor still runs. If the main
/// thread is wedged (e.g. blocked inside a macOS dispatch wait), that
/// path is dead and repeated Ctrl+C would otherwise do nothing. So on
/// the SECOND signal we restore the default disposition and re-raise,
/// letting the kernel terminate us immediately. `signal` + `raise` are
/// both async-signal-safe.
extern "C" fn handle_shutdown_signal(sig: libc::c_int) {
    SHUTDOWN_SIGNAL.store(true, Ordering::SeqCst);
    if signal_should_force_exit(SHUTDOWN_SIGNAL_COUNT.fetch_add(1, Ordering::SeqCst)) {
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
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
    // Hard-kill deadline, independent of the GPUI executor. A plain OS
    // thread (never wedged by the UI) waits for the shutdown flag, gives the
    // graceful `cx.quit()` path a few seconds to persist state + exit, then
    // force-exits if we're somehow still alive. A successful graceful quit
    // tears the process down first, so this only fires when the foreground
    // executor cannot run `cx.quit()` — the exact "Ctrl+C does nothing" case.
    std::thread::Builder::new()
        .name("shutdown-deadline".into())
        .spawn(|| loop {
            if SHUTDOWN_SIGNAL.load(Ordering::SeqCst) {
                std::thread::sleep(SHUTDOWN_GRACE_DEADLINE);
                // Still running ⇒ graceful quit stalled. 130 = 128 + SIGINT.
                unsafe { libc::_exit(130) };
            }
            std::thread::sleep(Duration::from_millis(100));
        })
        .expect("spawn shutdown-deadline watchdog thread");
    cx.spawn(async move |cx| {
        loop {
            if SHUTDOWN_SIGNAL.load(Ordering::SeqCst) {
                // Same as the quit/window-close hooks: preserve relay
                // PTYs across the SIGINT/SIGTERM shutdown so reattach
                // works on the next launch.
                oximux_app::shell::terminal_view::APP_QUITTING.store(true, Ordering::SeqCst);
                cx.update(|cx| cx.quit());
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
            component_theme.colors.input = palette.border_input;
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
        // Keep the spike's input chrome in step with the production init
        // blocks above — debugging input styling in the spike must show
        // the same borders the shipping window does.
        {
            let palette = oximux_settings::Theme::charcoal();
            let component_theme = gpui_component::Theme::global_mut(cx);
            component_theme.colors.input = palette.border_input;
            component_theme.colors.ring = palette.focus_ring;
        }
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
//
// The relay client gets its OWN tokio runtime, kept alive by the
// returned handle. This is load-bearing: every `RelayBackend` request
// (spawn / attach / list / resize) is a synchronous `Handle::block_on`
// issued from the GPUI render thread, and that thread already has the
// git/db runtime entered for the whole process. Driving relay requests
// on a SEPARATE runtime keeps `block_on` off the entered runtime's
// context, matching the invariant documented on `RelayBackend::new`.
// `None` ⇒ no relay; the app falls back to in-process PTYs.
fn boot_relay_supervisor(
    pane_relay_id_repo: oximux_storage::PaneRelayIdRepo,
) -> Option<tokio::runtime::Runtime> {
    let Some(data_dir) = dirs::data_dir() else {
        tracing::warn!("no data_dir; skipping relay supervisor");
        return None;
    };
    let runtime_dir = data_dir.join(APP_DATA_SUBDIR);
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("no home_dir; skipping relay supervisor");
        return None;
    };
    // macOS convention: per-app logs live under ~/Library/Logs.
    let log_dir = home.join("Library/Logs").join(APP_DATA_SUBDIR);

    // Dedicated runtime for the relay client's reader/writer tasks and
    // every later `block_on`. Two workers is plenty: the socket I/O is
    // light and the heavy lifting lives in the daemon process.
    let relay_rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("relay-rt")
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::warn!(?err, "failed to build relay runtime; using in-process PTYs");
            return None;
        }
    };

    let supervisor = RelaySupervisor::new(runtime_dir.clone(), log_dir.clone());
    // Run the handshake on `relay_rt` from a thread with NO runtime
    // entered (the caller's thread has the git/db runtime entered via
    // `_rt_guard`). A scoped thread lets `ensure_running` spawn the
    // client's reader/writer tasks onto `relay_rt`, and side-steps the
    // "block_on within an entered runtime" hazard entirely.
    let relay_handle = relay_rt.handle().clone();
    let connect_result = std::thread::scope(|scope| {
        scope
            .spawn(|| relay_handle.block_on(supervisor.ensure_running()))
            .join()
            .expect("relay handshake thread panicked")
    });
    let client = match connect_result {
        Ok(c) => c,
        Err(SupervisorError::VersionMismatch) => {
            tracing::warn!("relay version mismatch; falling back to in-process PTYs");
            #[cfg(target_os = "macos")]
            oximux_app::notifier::mac::post_system_banner(
                "OxiMux relay version mismatch",
                "Restart OxiMux to pick up the new daemon.",
            );
            return None;
        }
        Err(SupervisorError::Other(err)) => {
            tracing::warn!(?err, "relay supervisor failed; using in-process PTYs");
            return None;
        }
    };
    let server_session_id = client.server_session_id().to_owned();
    let client_arc = std::sync::Arc::new(client);

    if let Some(pid) = supervisor.read_pid() {
        arm_relay_heartbeat(
            pid,
            runtime_dir.clone(),
            log_dir.clone(),
            pane_relay_id_repo,
            server_session_id.clone(),
            relay_rt.handle().clone(),
        );
    } else {
        tracing::warn!("relay PID file missing; crash heartbeat disabled");
    }

    let backend = RelayBackend::new(client_arc, relay_rt.handle().clone());
    let boxed: Box<dyn TerminalBackend> = Box::new(backend);
    let shared = std::sync::Arc::new(std::sync::Mutex::new(boxed));
    install_shared_backend(shared);
    // Record the daemon socket so spawned shells can advertise it via
    // OXIMUX_SOCKET_PATH (lets `oximux notify` / agents dial the daemon).
    oximux_app::shell::context_env::set_relay_socket_path(
        supervisor.socket_path().to_string_lossy().into_owned(),
    );
    tracing::info!("relay supervisor up; PTYs will route through the daemon");
    Some(relay_rt)
}

// Watch the relay daemon's PID and recover when it dies: prune the dead
// session's persisted rows, respawn the daemon, swap a fresh backend into
// `SHARED_BACKEND` in place, and re-arm the watch on the new PID. The
// heartbeat fires `on_death` exactly once, so recovery is single-flight by
// construction; the chain ends (no retry storm) if a respawn fails.
fn arm_relay_heartbeat(
    pid: u32,
    runtime_dir: PathBuf,
    log_dir: PathBuf,
    repo: oximux_storage::PaneRelayIdRepo,
    session_id: String,
    handle: tokio::runtime::Handle,
) {
    let supervisor = RelaySupervisor::new(runtime_dir.clone(), log_dir.clone());
    let spawn_handle = handle.clone();
    let _enter = handle.enter();
    // JoinHandle dropped intentionally: the heartbeat task exits by
    // itself after firing `on_death` once.
    std::mem::drop(supervisor.watch_pid(pid, move || {
        // `on_death` is a sync FnOnce on the heartbeat task; the respawn
        // needs async (ensure_running). Boxed so the future type doesn't
        // recursively contain itself through the re-arm closure.
        spawn_handle.clone().spawn(respawn_relay_after_death_boxed(
            runtime_dir,
            log_dir,
            repo,
            session_id,
            spawn_handle,
        ));
    }));
}

// Type-erased wrapper: `respawn → arm_relay_heartbeat → watch closure →
// respawn` would otherwise make the async fn's future type contain itself.
fn respawn_relay_after_death_boxed(
    runtime_dir: PathBuf,
    log_dir: PathBuf,
    repo: oximux_storage::PaneRelayIdRepo,
    dead_session_id: String,
    handle: tokio::runtime::Handle,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(respawn_relay_after_death(
        runtime_dir,
        log_dir,
        repo,
        dead_session_id,
        handle,
    ))
}

// Bounded respawn retry: 5 attempts, 500ms → 8s exponential backoff
// (7.5s total sleep + supervisor-call latency). Enough to ride out a
// socket race or a fork stalled under load, short enough that the user
// isn't left wondering.
const RESPAWN_MAX_ATTEMPTS: u32 = 5;
// The retry loop's post-loop arm is unreachable only while at least one
// attempt runs — keep that true at compile time.
const _: () = assert!(RESPAWN_MAX_ATTEMPTS >= 1);
const RESPAWN_BASE_DELAY: Duration = Duration::from_millis(500);
const RESPAWN_MAX_DELAY: Duration = Duration::from_secs(8);

fn respawn_backoff_delay(attempt: u32) -> Duration {
    RESPAWN_BASE_DELAY
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16))
        .min(RESPAWN_MAX_DELAY)
}

// Daemon-death recovery. Runs on the relay runtime. Ordering matters:
// prune rows first (a quit during recovery must not persist hints that
// point at the dead daemon's PTYs), then respawn + swap, then re-arm.
async fn respawn_relay_after_death(
    runtime_dir: PathBuf,
    log_dir: PathBuf,
    repo: oximux_storage::PaneRelayIdRepo,
    dead_session_id: String,
    handle: tokio::runtime::Handle,
) {
    tracing::warn!(
        session_id = %dead_session_id,
        "relay daemon died mid-session; attempting respawn"
    );
    {
        // SQLite delete is blocking — keep it off the runtime worker.
        let repo = repo.clone();
        let dead = dead_session_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(err) = repo.delete_for_session(&dead) {
                tracing::warn!(?err, "pruning pane_relay_ids for dead session failed");
            }
        })
        .await;
    }
    // The app is tearing down — views are dropping and the next launch
    // runs a full supervisor boot anyway. Don't race it with a respawn.
    // Best-effort check: a quit that STARTS after this load lets the
    // respawn run during teardown — accepted; worst case is a swapped-in
    // backend nobody reads plus one stray notification, and runtime drop
    // waits out the re-armed heartbeat tick (~1s) at exit.
    if oximux_app::shell::terminal_view::APP_QUITTING.load(Ordering::SeqCst) {
        tracing::info!("app quitting; skipping relay respawn");
        return;
    }
    let supervisor = RelaySupervisor::new(runtime_dir.clone(), log_dir.clone());
    // Bounded retry: a transient spawn failure (socket race, slow fork
    // under load) must not permanently end daemon-backed terminals for
    // the rest of the session. Version mismatch is NOT transient — a
    // daemon from another build owns the socket and may serve other
    // windows — so it exits immediately, same as the boot path.
    let outcome = 'retry: {
        for attempt in 1..=RESPAWN_MAX_ATTEMPTS {
            if oximux_app::shell::terminal_view::APP_QUITTING.load(Ordering::SeqCst) {
                tracing::info!("app quitting; abandoning relay respawn retries");
                return;
            }
            match supervisor.ensure_running().await {
                Ok(client) => break 'retry Ok(client),
                Err(err @ SupervisorError::VersionMismatch) => {
                    break 'retry Err(err);
                }
                Err(err) if attempt < RESPAWN_MAX_ATTEMPTS => {
                    let delay = respawn_backoff_delay(attempt);
                    tracing::warn!(
                        ?err,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "relay respawn attempt failed; backing off"
                    );
                    tokio::time::sleep(delay).await;
                }
                Err(err) => break 'retry Err(err),
            }
        }
        unreachable!("loop breaks 'retry on the final attempt")
    };
    match outcome {
        Ok(client) => {
            let new_session_id = client.server_session_id().to_owned();
            let backend = RelayBackend::new(std::sync::Arc::new(client), handle.clone());
            match oximux_app::shell::terminal_view::shared_backend() {
                Some(shared) => {
                    let mut guard = shared.lock().expect("shared backend poisoned");
                    // Seed BEFORE the swap publishes the new backend:
                    // orphaned sessions get one synthetic Exit each, and
                    // the id floor moves past them so no live view's id
                    // is ever re-minted.
                    backend.seed_synthetic_exits(guard.live_session_ids());
                    *guard = Box::new(backend);
                }
                None => {
                    // Unreachable in practice: the heartbeat is only armed
                    // when boot installed a backend. Log rather than install
                    // — consumers cached `None` at boot and won't re-check.
                    tracing::warn!("relay respawned but no shared backend was installed at boot");
                    return;
                }
            }
            if let Some(pid) = supervisor.read_pid() {
                arm_relay_heartbeat(
                    pid,
                    runtime_dir,
                    log_dir,
                    repo,
                    new_session_id.clone(),
                    handle,
                );
            } else {
                tracing::warn!("respawned relay PID file missing; crash heartbeat disabled");
            }
            tracing::info!(
                session_id = %new_session_id,
                "relay daemon respawned; shared backend swapped in place"
            );
            notify_user(
                "OxiMux relay restarted",
                "Terminal sessions from before the crash have ended. \
                 New terminals are daemon-backed again.",
            );
        }
        Err(err) => {
            tracing::warn!(?err, "relay respawn failed; PTYs fall back to in-process");
            notify_user(
                "OxiMux relay could not be restarted",
                "New terminals will run in-process (no quit-survival) until you relaunch OxiMux.",
            );
        }
    }
}

fn notify_user(title: &'static str, message: &'static str) {
    #[cfg(target_os = "macos")]
    oximux_app::notifier::mac::post_system_banner(title, message);
    #[cfg(not(target_os = "macos"))]
    let _ = (title, message);
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,oximux=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression guard for the Ctrl+C escalation contract: the FIRST
    // shutdown signal must take the graceful `cx.quit()` path, and any
    // REPEAT must hard-escalate (restore SIG_DFL + re-raise) so a wedged
    // foreground executor can't leave the app un-quittable.
    #[test]
    fn first_shutdown_signal_is_graceful_repeats_escalate() {
        assert!(
            !signal_should_force_exit(0),
            "first signal must try graceful quit"
        );
        assert!(
            signal_should_force_exit(1),
            "second signal must hard-escalate"
        );
        assert!(signal_should_force_exit(2), "further signals keep escalating");
    }

    // The hard-kill deadline must stay long enough for a normal graceful
    // quit (~400 ms observed) yet short enough that a wedged app dies
    // promptly. Guards against an accidental bump to an effectively-never
    // value that would reintroduce the "unkillable" behavior.
    #[test]
    fn shutdown_grace_deadline_is_bounded() {
        assert!(SHUTDOWN_GRACE_DEADLINE >= Duration::from_secs(1));
        assert!(SHUTDOWN_GRACE_DEADLINE <= Duration::from_secs(5));
    }

    // The respawn backoff must grow geometrically from the base, cap at
    // the max, and never overflow on absurd attempt numbers — a transient
    // daemon-spawn failure rides this exact schedule before giving up.
    #[test]
    fn respawn_backoff_grows_and_caps() {
        assert_eq!(respawn_backoff_delay(1), Duration::from_millis(500));
        assert_eq!(respawn_backoff_delay(2), Duration::from_secs(1));
        assert_eq!(respawn_backoff_delay(3), Duration::from_secs(2));
        assert_eq!(respawn_backoff_delay(4), Duration::from_secs(4));
        assert_eq!(respawn_backoff_delay(5), RESPAWN_MAX_DELAY);
        assert_eq!(respawn_backoff_delay(64), RESPAWN_MAX_DELAY);
        // Total worst-case wait stays well under a minute.
        let total: Duration = (1..RESPAWN_MAX_ATTEMPTS).map(respawn_backoff_delay).sum();
        assert!(total <= Duration::from_secs(30));
    }
}
