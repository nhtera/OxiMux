# OxiMux — System Architecture

**Updated**: 2026-05-23  
**Phase**: 5 — relay hardening done; editor+LSP steps 1-3 shipped (save round-trip + LSP lifecycle + file tree backend)

---

## Layer overview

```
┌─────────────────────────────────────────────────────┐
│  GPUI UI layer  (crates/app)                        │
│  WorkspaceRoot → MainPane/TabbedPane/TerminalView   │
│                → RightSidebar (tab-switched panel)  │
│                   Explorer tab: FileExplorer (uniform_list, lazy, git badges)│
│                   SourceControl tab: GitPanel+DiffView│
│                → StatusBar (git zone)               │
├─────────────────────────────────────────────────────┤
│  Domain / backend layer                             │
│  crates/pty    — TerminalBackend + PortablePtyBackend│
│  crates/git    — Repository, StatusPoller, git ops  │
│  crates/agents — AgentRuntime trait + CliRuntime    │
│                  CliAgentAdapter + StatusMachine     │
│  crates/core   — shared domain types (no deps)      │
├─────────────────────────────────────────────────────┤
│  Async runtime                                      │
│  tokio multi-thread (booted in main.rs before GPUI) │
└─────────────────────────────────────────────────────┘
```

---

## WorkspaceRoot + RightSidebar wiring

```
WorkspaceRoot (GPUI entity)
├── fields
│   ├── main_pane: Entity<MainPane>
│   └── right_sidebar: Option<Entity<RightSidebar>>   ← None when no git repo
│
└── RightSidebar (GPUI entity, present only when cwd is a git repo)
    ├── active_tab: RightTab  (Explorer | Search | SourceControl)
    ├── activity_bar: 40px strip; single-letter glyph buttons
    ├── _repo: Arc<Repository>
    ├── _poller: StatusPoller              ← AbortHandle; drops → task stops
    ├── file_explorer: Entity<FileExplorer> ← shown on Explorer tab
    │     uniform_list virtualized tree; lazy load; git status badges M/A/D/R/U/C
    │     5s tokio timeout per dir load; focus-regain refresh via observe_window_activation
    ├── git_panel: Entity<GitPanel>        ← shown on SourceControl tab
    └── diff_view: Entity<DiffView>        ← shown on SourceControl tab
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
│     tokio 50ms poll task
│       drain TerminalEvent::Output → StatusMachine::feed / tick
│       on TerminalEvent::Exit    → StatusMachine::note_exit
│       exits when saw_exit || status_tx.is_closed()
│     watch::channel<AgentStatus> — cloned to all subscribers (badge / sidebar / dashboard)
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

Future ACP runtime (v1.1) will be a sibling `AgentRuntime` impl with identical `watch::Receiver<AgentStatus>` contract — UI code subscribes to the trait, not the impl.

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

## Deferred / not in v1

| Feature | Deferred to |
|---|---|
| ACP agent protocol | v1.1 (ADR-004) |
| Side-by-side diff | Phase 6 |
| Blame, file history, commit graph | Phase 6 |
| Editor + LSP full integration | Phase 5 step 4+ (steps 1-3 shipped; step 4 = file-tree UI render) |
| SQLite persistence / session restore | Phase 4 |
| Multi-agent dashboard | Phase 7 |
| embeddable terminal library terminal backend | v2 (ADR in brief.md) |
