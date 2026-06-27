# OxiMux — System Architecture

**Updated**: 2026-06-16  
**Phase**: 5 + multiplexer enhancements + UI/UX batch (settings modal, Quick Open, lifecycle scripts, Create PR + CI, floating PiP terminal, markdown preview) shipped

---

## Layer overview

```
┌─────────────────────────────────────────────────────┐
│  GPUI UI layer  (crates/app)                        │
│  WindowRegistry (global) — one WorkspaceRoot / window│
│  WorkspaceRoot → MainPane (grid of pane leaves)     │
│                   each leaf: LeafTabs (per-pane tabs)│
│                     each tab: PaneContent::Terminal  │
│                             | PaneContent::Editor    │
│                → RightSidebar (tab-switched panel)  │
│                   Explorer tab: FileExplorer         │
│                   Files tab:    FileTreeView         │
│                   Search tab:   SearchPanel          │
│                   SourceControl tab: GitPanel +      │
│                     DiffView + pr_ops + ci_status    │
│                → StatusBar (git zone + smart button) │
│  Overlays (mounted last-child in WorkspaceRoot)      │
│    PaletteModal   — Cmd+P / Cmd+Shift+P              │
│    SettingsModal  — Cmd+,  / left-rail cog           │
│    FloatingTerminal — Cmd+Shift+T (PiP, draggable)  │
├─────────────────────────────────────────────────────┤
│  Shared widget layer  (crates/ui — oximux-ui)       │
│  FloatingSurface overlay chrome, button variants,   │
│  ConfirmDialog. App-agnostic; app depends on ui,    │
│  ui never on app. Host re-exports as crate::ui.     │
├─────────────────────────────────────────────────────┤
│  Domain / backend layer                             │
│  crates/pty    — TerminalBackend + PortablePtyBackend│
│  crates/git    — Repository, StatusPoller, git ops, │
│                  GhCmd (gh CLI wrapper)              │
│  crates/agents — AgentRuntime trait + CliRuntime    │
│                  CliAgentAdapter + StatusMachine     │
│  crates/settings — terminal.toml, commit_message_ai.toml,│
│                    ProjectScripts (.oximux/scripts.toml)  │
│  crates/core   — shared domain types (no deps)      │
├─────────────────────────────────────────────────────┤
│  Async runtime                                      │
│  tokio multi-thread (booted in main.rs before GPUI) │
└─────────────────────────────────────────────────────┘
```

---

## Source map — where things live

Folder-level index (coarse by design — it rots slowly). Logic lives in the
backend crates (`core`/`git`/`agents`/`pty`/…); GPUI views live in
`crates/app/src/shell/<domain>/`. To find a feature, grep the domain column.

### Crates (`crates/<dir>/` → package → entry)

| Crate dir | Package | Entry | Purpose |
|---|---|---|---|
| `core` | `oximux-core` | `src/lib.rs` | shared domain types, no deps |
| `pty` | `oximux-pty` | `src/lib.rs` | `TerminalBackend` + `PortablePtyBackend` |
| `proc-cwd` | `oximux-proc-cwd` | `src/lib.rs` | resolve a process's working dir |
| `git` | `oximux-git` | `src/lib.rs` | `Repository`, `StatusPoller`, git ops, `GhCmd` |
| `agents` | `oximux-agents` | `src/lib.rs` | `AgentRuntime` trait, `CliRuntime`, `StatusMachine` |
| `editor` | `oximux-editor` | `src/lib.rs` | gpui-component editor wrapper + LSP glue |
| `storage` | `oximux-storage` | `src/lib.rs` | SQLite + migration ladder + CI guard |
| `settings` | `oximux-settings` | `src/lib.rs` | theme tokens, density, typography, TOML config |
| `relay-proto` | `oximux-relay-proto` | `src/lib.rs` | wire protocol shared by daemon + client |
| `relay` | `oximux-relay` | `src/lib.rs` + `src/main.rs` | out-of-process PTY relay daemon |
| `relay-client` | `oximux-relay-client` | `src/lib.rs` | in-app client for the relay daemon |
| `ui` | `oximux-ui` | `src/lib.rs` | app-agnostic widgets (`FloatingSurface`, buttons, `ConfirmDialog`); re-exported as `crate::ui` |
| `app` | `oximux-app` | `src/lib.rs` + `src/main.rs` | GPUI cockpit; all views (the 73%-LOC crate) |
| `xtask/` | `xtask` | `src/main.rs` | repo lint orchestrator (`file-size-lint` etc.) |

### `crates/app/src/` — top-level (non-view)

| File / folder | Holds |
|---|---|
| `workspace_root/` | `WorkspaceRoot` — one per window; owns panes + sidebar (`mod`/`ops`/`render`) |
| `project_panes_factory.rs` | manifest save/load, pane-buffer load, attach-reconcile |
| `actions.rs` / `state.rs` / `left_rail_layout.rs` | GPUI actions, app state, rail layout |
| `agent_glue/` | app-side agent wiring (bridges `oximux-agents` ↔ views) |
| `app_settings/` | in-app settings store + persistence |
| `keymap_registry/` | keybinding registration |
| `loaders/` | startup data loaders |
| `notifier/` | OS notifications |
| `platform/` | macOS-specific glue (App Nap, single-instance) |
| `session_restore/` | cold/warm session restore orchestration |

### `crates/app/src/shell/<domain>/` — GPUI views

One folder per cockpit zone: `agent_ui`, `agents_dashboard`, `browser_view`,
`chrome`, `command_palette`, `commit_dialog`, `compose_bar`, `diff_view`,
`file_explorer`, `forge`, `git_panel`, `left_rail`, `pane_group`, `panes`,
`pr_dialog`, `project_panes`, `right_sidebar`, `search_panel`, `session_history`,
`settings_modal`, `source_control`, `stash_panel`, `tasks_view`, `terminal`,
`usage`, `welcome`, `workspace`, `worktree_panel`. Each re-exports its modules
so existing `crate::shell::<name>::…` paths resolve regardless of folder.
A small set of cross-cutting glue files (`context_env.rs`, `openable_text_file.rs`,
`open_url.rs`, `cwd_resolver.rs`) stays loose at `shell/` root by design.

---

## WorkspaceRoot + RightSidebar wiring

```
WorkspaceRoot (GPUI entity)
├── fields
│   ├── main_pane: Entity<MainPane>        ← grid of pane leaves (Terminal | Editor)
│   ├── right_sidebar: Option<Entity<RightSidebar>>   ← None when no git repo
│   │     open_file_in_active_pane(path, window, cx)
│   │       → MainPane::open_editor_in_focused_pane(path, window, cx)
│   ├── palette: Entity<PaletteModal>      ← Cmd+P / Cmd+Shift+P overlay
│   ├── settings_modal: Entity<SettingsModal> ← Cmd+, / left-rail cog overlay
│   └── floating_terminal: Option<Entity<FloatingTerminal>> ← Cmd+Shift+T PiP overlay
│         draggable/resizable; PTY persists across hide; geometry debounce-persisted
│
└── RightSidebar (GPUI entity, present only when cwd is a git repo)
    ├── active_tab: RightTab  (Explorer | Search | SourceControl | Files)
    ├── activity_bar: 40px strip; single-letter glyph buttons
    ├── _repo: Arc<Repository>
    ├── _poller: StatusPoller              ← AbortHandle; drops → task stops
    ├── file_explorer: Entity<FileExplorer> ← shown on Explorer tab
    │     uniform_list virtualized tree; lazy load; git status badges M/A/D/R/U/C
    │     5s tokio timeout per dir load; focus-regain refresh via observe_window_activation
    ├── file_tree_view: Entity<FileTreeView> ← shown on Files tab (always visible; no repo gate)
    │     on_open callback → WorkspaceRoot::open_file_in_active_pane
    ├── source_control: Entity<SourceControlPanel> ← shown on SourceControl tab
    │     primary_action resolver: CreatePR (in-sync + GitHub + no open PR) /
    │       Push / Sync-ahead / Commit / Stage All / Pull
    │     pr_ops.rs: gh pr create --fill → opens browser
    │     ci_status.rs: gh pr checks → ✓N ✗N ●N row; 30s throttle; only while PR open
    ├── git_panel: Entity<GitPanel>        ← changed-files list
    └── diff_view: Entity<DiffView>        ← hunk diff render
```

`WorkspaceRoot::new` creates `RightSidebar::new(repo, …)` when `Repository::open` succeeds. `StatusPoller::spawn` wires the 500ms tick inside `RightSidebar`. Status bar reads `right_sidebar.read(cx).latest_poll_state()`. On each non-duplicate status delta, `cx.notify()` propagates to all subscribers including the status bar center zone.

---

## StatusPoller pattern

```
tokio task (500ms tick)
  │  pause when window blurred
  │
  ├─ git status --porcelain=v2 --branch
  │
  └─ send_if_modified(new_state)
       │  only fires when state differs from last emission
       │
       └─ entity.update(cx, |root, cx| {
              root.poll_state = Some(Ready(new_state));
              cx.notify();
          })
```

Security: every `GitCmd` runs with `GIT_CONFIG_NOSYSTEM=1` (blocks malicious `core.hooksPath`) and `GIT_OPTIONAL_LOCKS=0` (prevents `FETCH_HEAD` lock contention under parallel polling).

---

## Terminal data flow

```
shell process (PTY)
  │
  ├─ watcher thread (per session)
  │    reads bytes → TerminalEvent::Output(bytes)
  │    sends to bounded sync_channel(256)
  │
  └─ 16ms poll task (cx.spawn in TerminalView)
       drain_events()
       │  on Output → state.advance(&buf) → grid update
       │  on Exit   → mark dead, skip grid alloc
       └─ cx.notify() → render visible rows only
```

Blink suppression: unfocused `TerminalView` entities gate `cx.notify()` on `view.focused` — idle unfocused panes emit 0 repaints/s from the blink task.

---

## Git UI flow (Phase 2)

```
GitPanel (GPUI entity)
  ← subscription on StatusPoller updates
  ← renders: staged / unstaged / untracked sections

DiffView (GPUI entity)
  ← DiffView::load(path, staged, cx)
      → Repository::diff_for_path (tokio)
      → parse_unified_diff
      → render hunk lines (capped at LARGE_DIFF_LINE_THRESHOLD; expand() removes cap)

CommitDialog (GPUI entity)
  ← cycle_prefix() → conventional-commit prefix dropdown
  ← submit() → Repository::commit (tokio)

ConfirmDialog (GPUI entity)
  ← type-to-confirm gate for destructive ops (revert, stash drop, worktree remove)
  ← typed_matches() calls pure logic::is_match

StashPanel (GPUI entity)
  ← refresh() → git stash list
  ← apply/pop/request_drop → git stash ops

WorktreePanel (GPUI entity)
  ← refresh() → git worktree list
  ← submit_create() → git worktree add + branch create
  ← pending_remove → confirm_dialog gate → git worktree remove
```

---

## Agent runtime flow (Phase 3)

```
CliRuntime (AgentRuntime impl)
│
├── Adapter registry: HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>
│     registered manually until step-8 detection scan
│
├── start_session(id, config)
│     spawn_blocking → openpty/fork via PortablePtyBackend
│     optional stdin_seed write (warn if > MAX_SAFE_STDIN_SEED = 4096 bytes)
│     tokio 50ms poll task → poll_helpers::process_poll_events
│       TerminalEvent::Output → AgentOscScanner::feed (strip OSC-9999 → cleaned + SidebandEvent)
│                               → StatusMachine::feed on cleaned bytes; sideband applies last via feed_sideband→force
│       on TerminalEvent::Exit → StatusMachine::note_exit / note_interrupted
│       exits when saw_exit || status_tx.is_closed()
│     watch::channel<AgentSnapshot> — cloned to all subscribers (badge / sidebar / dashboard)
│
├── send_message(id, text)
│     spawn_blocking → write bytes to PTY stdin
│
├── cancel(id)
│     spawn_blocking(drain_events + close)
│     select!{ await poll_handle / sleep(timeout) }
│       on timeout → poll_handle.abort()
│     removes session entry (double-cancel returns error)
│
└── subscribe_status(id) / current_status(id)
      brief lock → clone / borrow watch::Receiver
```

`CustomCommandAdapter` is the escape-hatch `CliAgentAdapter`: reads `AgentSessionConfig::custom_command: Option<(String, Vec<String>)>`; empty `status_patterns()` delegates all state transitions to `StatusMachine` defaults. Useful for one-off CLIs without a dedicated adapter.

`ClaudeCodeAdapter` is the first branded adapter: builds `claude [--model M] [--effort E] [<prompt>]`, no `-p/--print` (interactive PTY), no env layering. Two starter `status_patterns()` map the workspace-trust dialog and the generic "Do you want to proceed/continue" tool prompt to `NeedsApproval`. Patterns deliberately omit leading word-boundary so they still match ANSI-SGR-prefixed output from the live TUI. Detection shells out to `which claude` via the shared `cli::detect::which_on_path` helper.

`CodexAdapter` is the second branded adapter: builds `codex [-m M] [<prompt>]`. `cfg.effort` ignored (no CLI analog), no `--ask-for-approval` / `--sandbox` overrides (user's `~/.codex/config.toml` owns approval cadence — the "no paternalistic defaults" v0.9 retro rule). `status_patterns()` intentionally empty; pattern calibration is deferred to dogfood week 18 (step 14) when real Codex TUI bytes get captured into fixtures — writing regex against an imagined haystack is the exact "tests-pass / runtime-broken" trap the step-5 journal documents.

`AiderAdapter` is the third branded adapter: builds `aider [--model M]` and routes `cfg.prompt` through `CommandSpec::stdin_seed` rather than argv — Aider's REPL has no positional-prompt argument and `--message` is one-shot incompatible with the OxiMux interactive PTY model. Aider is the first real consumer of the `stdin_seed` path the trait reserved at Phase 3 step 1. Embedded `\n` in the prompt submits each line as a discrete REPL prompt — intentional, no collapse. No `--yes` / `--auto-commits` paternalism (user's `~/.aider.conf.yml` owns). Empty `status_patterns()` with the same step-14 calibration deferral.

The status channel carries `AgentSnapshot { status: AgentStatus, detail: Option<SidebandDetail> }` (not a bare `AgentStatus`): the regex `StatusMachine` path publishes `detail: None`, while an OSC-9999 sideband event (`osc_sideband::AgentOscScanner`) attaches structured `tool` / `tool_input` / `msg` detail. This closes the Codex/Aider `EMPTY_PATTERNS` blindness — an agent (or hook) emitting `ESC]9999;{"v":1,"state":"needs_approval",...}BEL` drives status with no regex pattern needed. `current_status()` still returns a bare `AgentStatus` for the common lifecycle-only consumer.

Future ACP runtime (v1.1) will be a sibling `AgentRuntime` impl with identical `watch::Receiver<AgentSnapshot>` contract — UI code subscribes to the trait, not the impl.

---

## Relay daemon lifecycle (Phase 5)

```
launchd / manual spawn
  │
  └── relay main.rs
        ├── PidGuard  — writes pid file; unlinks on drop
        ├── SocketGuard — creates Unix socket; unlinks on drop
        ├── tracing subscriber stack
        │     stderr text layer
        │     + tracing-appender daily-rolled JSON  (relay.log.YYYY-MM-DD, 7-day purge at startup)
        │     + macOS oslog layer
        │
        └── Server::run(config, shutdown_notify)
              ├── accept loop → per-connection task
              │     Request::Stats    → PtyRegistry::stats() → Response::StatsOk
              │     Request::Shutdown → Notify::notify_waiters()
              │
              ├── spawn_idle_gc task
              │     ticks at idle_tick_interval
              │     reaps PtyRegistry entries idle > idle_timeout
              │
              └── SIGTERM / SIGINT handler → Notify::notify_waiters()
                    Server awaits Notify; drops guards → socket + pid cleaned up
```

**App-side supervisor** (`crates/app/src/relay_supervisor.rs`):

```
boot_relay_supervisor(PaneRelayIdRepo)
  ├── read_pid → Option<u32>
  ├── if pid alive → ExistingConnect (attach path)
  ├── else → spawn relay binary
  └── spawn crash heartbeat task
        watch_pid: 1Hz kill(pid,0) loop
        on death → on_relay_died
              spawn_blocking: sqlite delete orphaned pane rows
                + AppKit banner if VersionMismatch (no auto-respawn)
```

**VersionMismatch** (`SupervisorError::VersionMismatch`): app reads relay's reported protocol version on connect; mismatch shows macOS notification banner and parks in degraded mode — never silently auto-respawns a mismatched binary.

---

## Editor + LSP (Phase 5)

### Step 1 — spike (go)

> Cook report: `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`

```
crates/app/src/main.rs
  --editor-spike flag
    └── run_editor_spike()
          tokio Handle check (self-aborts with clear message if no reactor in scope)
          cx.open_window("OxiMux — Editor Spike")
            └── EditorView::new(file_path, cx)       crates/editor
                  gpui-component Input (code_editor("rust") mode)
                  attach_lsp("rust-analyzer", "rust", workspace_root, cx)
                    ├── LspClient::spawn(program, workspace_root, handle)
                    │     Content-Length framing (transport.rs)
                    │     initialize + initialized + didOpen handshake
                    │     dispatch loop: responses → pending_requests map
                    │                   server-initiated requests → {result:null}
                    │                   publishDiagnostics → WeakEntity<InputState>
                    │
                    └── register HoverProvider → LspHoverProvider
                          hover(pos, cx):
                            handle.spawn(async { LspClient::hover(pos) })  ← tokio worker
                            gpui::Task awaits JoinHandle                    ← GCD thread
```

Key design decisions locked in spike:
- One `LspClient` per (workspace_root, language); no pooling in v1.
- `tokio::runtime::Handle` captured at spawn, passed into GPUI tasks — avoids `rt.enter()` thread-local vs GCD-worker mismatch.
- `REQUEST_TIMEOUT = 5s` via `tokio::time::timeout`; no `$/cancelRequest`.
- Missing binary → `tracing::warn` + editor renders without LSP; no panic.
- Spike is read-only: no `didChange`, no save round-trip (step 2 owns that).

### Markdown rendered preview

`.md` / `.markdown` files open in a rendered view; `.mdx` keeps the plain code editor.

```
EditorView::new(path, cx)
  is_markdown_path(path) → true for .md / .markdown (not .mdx)
  md_mode: MarkdownViewMode  (Source | Preview | Split)  default = Preview
  │
  ├── header: mode_toggle() — segmented button row, right-aligned, markdown-only
  │
  └── body match md_mode:
        Source  → gpui-component Input (normal code editor)
        Preview → render_preview()
        Split   → h_resizable(source_pane | preview_pane)

render_preview()
  absolutize_image_paths(text, file_path)
    rewrites repo-relative ![](path) → file:// URI (pure fn)
  gpui-component text::markdown (GFM renderer)
    headings / bold / italic / inline-code / tables / task lists /
    blockquotes / links / fenced code blocks / images
```

**FileHttpClient** (`crates/app/src/file_http_client.rs`):

gpui defaults to `NullHttpClient` — its image element loads image URLs through the http client, never the filesystem. `FileHttpClient` is a `file://`-only `gpui::HttpClient` impl installed at app startup via `cx.set_http_client(Arc::new(FileHttpClient))`. It overrides `get()` because `http::Uri` rejects `file:///path` (empty authority) in the default path. HTTP/HTTPS requests are not forwarded (rejected).

```
main.rs
  cx.set_http_client(Arc::new(FileHttpClient))   ← installed once at boot
    └── FileHttpClient::get(file:///abs/path)
          read file bytes → Response with Content-Type image/*
```

**Deferred** (not in v1): WYSIWYG editing, scroll-sync, TOC, mode-cycle keybinding, remote http(s) image rendering, `.mdx` preview.

---

### Step 2 — save round-trip + LSP textDocument lifecycle (go, smoke green)

> Tester report: `plans/reports/tester-260522-1939-phase-05-step02-editor-save.md`  
> Code review: `plans/reports/code-review-260522-1939-phase-05-step02-editor-save.md` (8/10)

**EditorView lifecycle (step 2 additions):**

```
EditorView::new(path, cx)
  ├── uri: lsp_types::Uri  — parse-once from path_to_file_uri; reused for all LSP calls
  ├── dirty = false, doc_version = 1, last_sent_text = file_content
  └── cx.observe(&state, callback) → _observe_sub (keeps observer alive)
        callback fires on every cx.notify() from InputState (incl. silent undo/redo)
        decide_change_propagation(last_sent_text, current_text, doc_version)
          → None if text unchanged (cursor moves, scroll) — no-op
          → Some(prop) if text differs: last_sent_text = prop.text,
                                        dirty = true,
                                        doc_version = prop.new_version,
                                        client.did_change(&uri, version, text)

attach_lsp(program, lang, workspace_root, cx)
  └── lsp_bridge::spawn_attach_lsp(...)
        tokio: LspClient::spawn → initialize/initialized/didOpen (version=1)
               reads file again → did_open_text
        GPUI: entity.update → editor.set_lsp_client(client, did_open_text)
               if last_sent_text ≠ did_open_text  ← buffer drifted during handshake
                 doc_version += 1; client.did_change(catch-up)  ← Fix #3

on Cmd+S (SaveFile action)
  ├── fs::write(file_path, text)
  ├── if Ok: dirty = false; client.did_save(&uri); cx.notify()
  └── if Err: tracing::error; dirty stays true; cx.notify()

impl Drop for EditorView
  └── client.did_close(&uri)  — sync UnboundedSender::send; non-blocking; safe in Drop
```

**Why `cx.observe` not `cx.subscribe(InputEvent::Change)`:**  
gpui-component's undo/redo calls `replace_text_in_range_silent` which bypasses `InputEvent::Change`. `cx.observe` catches every `cx.notify()` from `InputState`; the `decide_change_propagation` guard discards cursor-move noise at zero allocation cost.

**Key invariants:**
- `dirty` and `doc_version` live on `EditorView`, not `InputState` — no gpui-component fork needed.
- `doc_version` is a plain `i32` (GPUI main-thread only; no atomic needed); strictly monotonic across normal edits + undo/redo.
- `didChange` uses full-sync (entire buffer text, `range: None`) per LSP §3.17.2; 0ms debounce.
- LSP calls are no-ops when `lsp_client` is `None` (editor-without-LSP degraded path preserved).
- `SaveFile` action declared in `oximux-editor` (not `oximux-app::actions`) to break the `oximux-app → oximux-editor → oximux-app` circular crate dependency.

---

### Step 3 — file tree backend (headless entity)

```
FileTree (GPUI entity)
  ├── load(root_path, cx)
  │     cx.background_executor().spawn → walker::walk_dir(root, SKIP_NAMES)
  │       WalkBuilder::max_depth(1) + filter_entry → Vec<(PathBuf, bool)>
  │       sort_entries: dirs first, then ASCII-lowercase alpha
  │     → inserts children into nodes map; emits FileTreeEvent::Loaded(id)
  │
  ├── expand(node_id, cx)
  │     remove_subtree(id) — recursive evict from nodes + open_dirs (prevents phantom Refresh)
  │     then re-runs walker for that dir → Loaded(id)
  │
  ├── collapse(node_id) — marks closed; children stay in map (lazy re-walk on next expand)
  │
  └── cx.spawn → watcher event loop
        spawn_watcher(root, tx) wraps notify_debouncer_full::new_debouncer(200ms)
          closure: DebounceEventResult → tx.send(paths)
        loop: rx.recv() → is_ignored(path, SKIP_NAMES)
                        → find_node_to_invalidate(path, nodes)
                        → emits FileTreeEvent::Refresh(ancestor_id)
```

**Key invariants:**
- Single FSEvents watch from root; userspace `is_ignored` filter in debounce callback (not kernel-level).
- `SKIP_NAMES` const shared by walker + watcher — one place to add `node_modules` etc.
- `notify` accessed via `notify_debouncer_full::notify` re-export; avoids two-versions-of-notify compile failure (debouncer 0.4 pins notify 7).
- `Refresh(id)` is coarse — step 4 diffs against UI state; step 3 does not diff.

---

### Step 4 — file tree UI (FileTreeView)

`FileTreeView` lives in `oximux-app/shell/file_tree_view.rs` and subscribes to `Entity<FileTree>` (from `oximux-editor`) via `cx.subscribe_in` registered before the first `expand()` call so no `Loaded` event can be missed.

**Lazy expand pattern:**
```
dir click → tree.expand(id)           oximux-editor entity
  immediately: RowKind::Placeholder    italic "…" sentinel shown
  Loaded(id) event fires later
    → rebuild_rows() swaps sentinel for real children
```

**Click flows:**
- Dir click: toggles `expanded_ids: HashSet<TreeNodeId>` on view; calls `tree.expand(id)` if newly expanded
- File click: fires `on_open: Arc<dyn Fn(PathBuf, &mut Window, &mut App)>` callback

**Key invariants:**
- `expanded_ids` (view) is separate from `FileTreeNode.loaded` (model) — chevron direction reads `expanded`, not `loaded`
- Raw `uniform_list` used, not `gpui-component::Tree` — matches `FileExplorer` precedent; avoids auto-expand-on-click conflict with lazy walker model
- `build_display_rows` is a pure fn extracted for unit testing (8 tests in `app/tests/file_tree_view_unit.rs`)

---

### Step 5 — pane as editor host (workspace wiring)

`MainPane` workspace grid is no longer terminal-only. Each leaf now holds a `PaneContent` enum.

```
PaneContent::Terminal(Entity<TerminalView>)
PaneContent::Editor(Entity<EditorView>)

MainPane::open_editor_in_focused_pane(path, window, cx)
  same-path short-circuit → no-op if focused leaf already shows that file
  else → replace focused leaf content with EditorView::new(path, cx)

RightSidebar (Files tab — always visible, no repo gate)
  FileTreeView on_open callback
    → OnOpenFile event
    → WorkspaceRoot::open_file_in_active_pane(path, window, cx)
    → MainPane::open_editor_in_focused_pane(path, window, cx)
```

**EditorView focus parity (step 5 addition):**
- `focused: bool` field mirrored by `cx.on_focus` / `cx.on_blur` — matches `TerminalView` observer pattern
- `set_window_title` removed from `EditorView::render`: multi-leaf editors cannot share a single window title

**Key invariants:**
- Files tab (`RightTab::Files`): always present regardless of whether cwd is a git repo
- `SelectFilesTab` action bound to `Cmd+Shift+T`
- Editor leaves persist for the running session; silently dropped on app quit or project switch (no editor restore in v1)

---

## Multi-window architecture (mux-P3)

```
WindowRegistry (GPUI global, app-lifetime)
  ├── windows: Vec<RegisteredWindow>
  │     each: window_id (GPUI WindowId) + persist_id ("main" | "w{n}") + Entity<WorkspaceRoot>
  │
  ├── mint_persist_id() → "main" (first) | "w2", "w3", … (subsequent)
  │
  └── pending_tearoffs: Vec<PendingTearOff>
        pushed by source window before calling open_workspace_window_with;
        consumed by destination window's WorkspaceRoot::new
```

**Window lifecycle:**
- `NewWindow` (Cmd+N) → `open_workspace_window(cx, repo, app_state)` → mints id → `open_workspace_window_with`
- Last-window close → `cx.quit()`; non-last close dismisses only that window (single quit observer guards)
- Boot: `capture_session` manifest → `open_workspace_window_with` per persisted window_id

**Cross-window tear-off (`MoveTabToNewWindow`):**
```
source WorkspaceRoot
  1. TerminalBackend::detach(pty_id)   ← releases relay attachment; PTY stays alive
  2. removes tab from its group
  3. pushes PendingTearOff { dest_window_id, relay_id, … }
  4. open_workspace_window_with(cx, …, dest_window_id)

destination WorkspaceRoot::new
  consume_pending_tearoff(dest_window_id)
  → attach_pty_existing(relay_id)     ← re-mounts PTY in new window
```

Relay multiplexes one subscriber per `pty_id`; detach source before attach destination is the mandatory ordering.

**Per-window persistence (V005):**

`pane_buffers` and `pane_relay_ids` now include `window_id` in their primary keys (`DEFAULT 'main'` for existing rows). Settings layout capture/restore thread the window id; each window captures/restores its own pane tree independently.

---

## Per-pane tabs — LeafTabs (mux-P4)

```
TerminalSplitTree
  └── panes: Vec<Option<LeafTabs>>      ← one LeafTabs per split leaf

LeafTabs
  ├── tabs: Vec<(Entity<TerminalView>, Observer)>
  ├── active: usize
  └── compact chip strip rendered when len > 1 or freshly split
        chip click → switch active tab
        '+' button / NewTabInPane (Cmd+Shift+T) → append new shell tab
```

**Cmd+W cascade:** per-pane tab → leaf → group tab (innermost wins).

**Persistence:** `PersistedSubPane.tabs` (Vec of persisted tab blobs); serde-default for existing single-tab leaves; no SQL migration required.

**Known v1 limits:** per-pane-tab relay reattach deferred (multi-tab leaves restore dormant); agent CLI PTYs not yet threaded with context env; cross-group multi-tab-drag repaint deferred.

---

## Post-paint PTY attach on restore

Restored terminal tabs no longer gate first paint behind daemon round-trips. The window-open path mounts every restored tab as a **pending** view over an in-process dormant grid (`spawn_pending_placeholder_grid` — never touches the relay), paints, then a detached reconcile task swaps live sessions in:

```
set_active_project (pre-paint, zero relay RPCs)
  └── build_project_panes → Vec<PendingAttach>     ← TerminalView::mount_pending per tab
        └── spawn_attach_reconcile (cx.spawn_in, detached)
              1. relay_state_snapshot()             ← ONE ListPtys, background executor
              2. compute_attach_hints / compute_leaf_attach_hints (unchanged liveness gate)
              3. per tab: attach_pty_existing(hint) | spawn_local_pty(cwd, env)
                   ← each RPC on the background executor (blocking Handle::block_on)
              4. TerminalView::adopt_live_session    ← main-thread delivery, per tab
```

**Invariants:**
- Every blocking relay call (`Handle::block_on` against the relay runtime) runs on the background executor; only `adopt_live_session` delivery touches the main thread.
- Input to a pending view is dropped quietly; `external_id()` reports `None` while pending (tear-off stays disabled) but `relay_id_for_capture()` answers with the persisted hint so a quit-save racing the reconcile keeps the row.
- Undeliverable sessions (tab closed mid-reconcile): re-attached PTYs are **detached** (daemon keeps them), fresh spawns are **closed**; both skipped under `APP_QUITTING`.
- The boot relay supervisor handshake stays intentionally blocking pre-paint (warm path ≈ 1 ms; the daemon survives quit, so cold spawns are reboot-only).
- Restored panes paint blank until the daemon's raw-byte replay arrives — serialized-grid replay was deliberately removed (reflow scrambles full-screen TUIs).

Boot timing is logged on every launch: `boot: db open + state hydrate`, `boot: relay supervisor handshake`, `project panes built (pre-paint, no relay RPCs)`, `post-paint pty attach reconcile done`.

### Disk scrollback checkpoints (cold restore after daemon death)

The relay daemon checkpoints each PTY's replay ring to `<runtime dir>/checkpoints/<pty_id>/{meta.json,scrollback.bin}` every 5 s (atomic tmp+rename, skipped when `bytes_out` hasn't moved) and **removes** the checkpoint on every clean end — deliberate `Close` and natural child exit. Whatever remains on disk is therefore an unclean death (daemon crash, SIGKILL, host reboot).

The wire protocol is untouched: the app reads checkpoints straight off disk. When the reconcile's warm attach fails and a slot cold-spawns, the background executor looks up the raw persisted hint's checkpoint and, after `adopt_live_session`, prefills the pane grid with: clear screen → alt-screen-truncated scrollback tail (≤ 512 KiB, advanced to the next line boundary when the cap cut mid-line) → dim `--- session restored ---` marker → terminal mode reset (`relay_cold_restore.rs`). A checkpoint with no replayable scrollback (alt-screen-only session, or a crash before the first tick) still restores the recovered cwd — blank grid, no marker. The checkpoint is deleted only after successful delivery, so a quit mid-reconcile can still restore on the next launch; orphans fall to the daemon's 7-day boot GC.

Right after the reconcile loop, the app re-captures `pane_relay_ids` for the window (reusing the snapshot's session id — no extra daemon round-trip), so cold-spawned PTYs' fresh ids hit SQLite immediately instead of waiting for the next quit/switch capture. Without this, an app crash after a reconcile would leave the table pointing at dead ids and strand the boot's live PTYs.

The replacement shell spawns at the checkpoint's recorded cols/rows (initial dims only — the pane's normal resize takes over after adopt), so its first prompt wraps for the same width as the restored content above it.

Two further resilience layers (same hardening pass): **daemon respawn retries** — `respawn_relay_after_death` rides out transient spawn failures with bounded exponential backoff (5 attempts, 500 ms → 8 s cap; version mismatch still exits immediately since another build's daemon may own the socket) — and a **15 s layout autosave** on each `WorkspaceRoot` that captures all projects' layouts plus relay ids (cached handshake session id, zero wire RPCs), bounding what an app crash can lose of mid-session tabs/splits to one tick. Idle ticks cost nothing: `save_persisted_tabs` skips byte-identical JSON via a content-hash gate recorded only after a successful write.

Each checkpoint also refreshes `meta.cwd` with the shell child's **live working directory**, resolved kernel-side from the child pid (`oximux-proc-cwd`, the shared `proc_pidinfo`/`PROC_PIDVNODEPATHINFO` resolver also used for split-pane cwd inheritance) — no OSC 7 cooperation from the shell required. The cold-spawn path revives the replacement shell at that recovered cwd (validated to still exist; falls back to the persisted layout cwd), so a crash puts the user back in the directory they were actually in.

The same meta carries the shell child's **OS pid** (seeded at spawn). Because the daemon and its children run on the same host as the app, `TerminalView::os_pid` falls back to reading it for daemon-backed sessions — giving splits from relay panes (and layout-snapshot cwd capture) the same kernel-true cwd inheritance as in-process panes, again with zero wire-protocol involvement.

This path is distinct from routine restore, which stays replay-free (the no-grid-replay invariant above is unchanged): cold restore trades reflow perfection for not losing the scrollback entirely, and the marker makes clear the content is history, not live state. `prefill_grid` ends with `clear_collected`, so query auto-replies recorded in the crashed session can't reach the new shell's stdin.

---

## Shell context env — SurfaceIds (mux-P4)

Every spawned terminal receives an `OXIMUX_*` env block minted at spawn time:

| Variable | Value |
|---|---|
| `OXIMUX_WORKSPACE_ID` | project root path |
| `OXIMUX_SURFACE_ID` | UUID minted per pane leaf |
| `OXIMUX_TAB_ID` | UUID minted per tab within leaf |
| `OXIMUX_SOCKET_PATH` | relay Unix socket path |
| `OXIMUX_PTY_ID` | injected by relay daemon at fork |

`SurfaceIds` (`shell/context_env.rs`) builds the env list. Ids persist in the per-pane layout blob (serde-default) and are re-injected on dormant respawn. Agent CLI PTYs excluded until a follow-on.

---

## Key architectural constraints

| Constraint | Enforcement |
|---|---|
| All git I/O off UI thread | `tokio::process` in `GitCmd`; 10s timeout |
| All PTY I/O off UI thread | watcher thread; bounded channel |
| No unbounded log buffers | `TerminalState` 5000-row scrollback cap |
| No monolith shell files | xtask file-size-lint: warn >500, fail >800 non-blank LOC |
| StatusPoller emits on change only | `send_if_modified` dedup in `poller.rs` |
| GPUI + gpui-component SHA pinned together | Cargo.lock owns rev; see `docs/gpui-pins.md` |
| No eager repo scan at startup | `Repository::open` is a single `git rev-parse`; no tree walk |

---

## Left-rail drag-to-reorder

Project groups and workspace rows support drag reorder. The interaction follows Zed's stateless GPUI idiom — no drag entity; state lives in the payload type.

```
on_drag(ProjectDragPayload { id, original_index })
  │
  drag_over(target row)
    insertion_side(pointer_y, row_bounds) → Above | Below
    paint_insertion_line(bounds, side, cx)   ← 2px accent, full-width
  │
on_drop
  reorder_slot_value(neighbors) → f64   ← midpoint between neighbors
  ProjectRepo::reorder_to(id, index)
    or WorkspaceRepo::reorder_to_target(id, neighbor_id, side)
    ↳ normalize_ranks() if float precision exhausted
```

**Sort order model**: `projects.sort_order REAL` (V014) and `workspaces.sort_order REAL` (V015); sparse-float (gaps of 1.0 between rows), narrowed by repeated insertions, renormalized to integer spacing when gap < f64::EPSILON. `ProjectRepo::list_ordered` replaces the old recency (`last_opened_at DESC`) query — project list is now **manual-sticky**; opening a project does not reorder it.

**Escape cancel**: `DismissOverlay` handler calls `cx.stop_active_drag` as its first branch, cancelling any in-flight drag before closing overlays.

**Constraints**: drop indicator is full-width only (GPUI `drag_over` is style-only; pixel-inset requires a separate overlay entity, deferred). Workspace drag disabled outside `Manual` sort mode; primary workspace row not draggable.

---

## Deferred / not in v1

| Feature | Deferred to |
|---|---|
| ACP agent protocol | v1.1 (ADR-004) |
| Side-by-side diff | Phase 6 |
| Blame, file history, commit graph | Phase 6 |
| Editor + LSP full integration | Phase 5 step 6+ (steps 1-5 shipped; step 6+ = keybindings, multi-file, LSP completions) |
| Markdown WYSIWYG / scroll-sync / TOC / keybinding | follow-on (basic preview shipped) |
| Remote http(s) image rendering in markdown preview | follow-on (FileHttpClient is file:// only) |
| `.mdx` preview | follow-on (kept as plain code editor) |
| SQLite persistence / session restore | Phase 4 |
| Multi-agent dashboard | Phase 7 |
| embeddable terminal library terminal backend | v2 (ADR in brief.md) |
| Per-pane-tab relay reattach on restore | follow-on (multi-tab leaves restore dormant for now) |
| Agent CLI PTYs with OXIMUX_* context env | follow-on |
| Cross-group multi-tab-drag repaint | follow-on |
