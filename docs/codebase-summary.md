# OxiMux — Codebase Summary

**Updated**: 2026-05-23  
**Phase**: 5 — relay hardening done; editor save round-trip + LSP lifecycle (step 2) + file tree backend (step 3) shipped  
**Tests**: 510+ passed (editor crate: 28 tests — 21 prior + 7 file_tree integration)

---

## Workspace layout

```
oximux/
├── Cargo.toml              workspace root; all crate deps declared here
├── xtask/                  CI helpers: file-size-lint, build checks
└── crates/
    ├── app/                GPUI shell — UI composition, action routing, rendering
    ├── core/               domain types with zero tokio/GPUI deps
    ├── pty/                portable-pty + alacritty_terminal backend
    ├── git/                git CLI wrappers, poller, diff parser
    ├── agents/             AgentRuntime async trait + CliAgentAdapter + StatusMachine (Phase 3 foundation)
    ├── editor/             gpui-component code editor + LSP client (Phase 5 spike)
    ├── storage/            SQLite via rusqlite — Db wrapper + migrations + V001 schema + 5 typed repos (Phase 4 step 3)
    └── settings/           TOML config, theme tokens, typography
```

---

## crates/app — module map

```
src/
├── main.rs                 boots tokio runtime; opens Repository at cwd (Option);
│                           registers keybindings; opens GPUI window
├── lib.rs                  re-exports for integration tests
├── actions.rs              all GPUI Action structs (SplitHorizontal, Search, etc.)
├── assets.rs              CompositeAssets — local SVGs (git-branch) + gpui-component bundle
├── workspace_root.rs       WorkspaceRoot entity — top-level layout host
│                           right_sidebar: Option<Entity<RightSidebar>>
│                           left_rail: Entity<LeftRail>; left_rail_open: bool (Cmd+B)
│                           palette: Entity<PaletteModal> (Cmd+P / Cmd+Shift+P)
│                           poll_state mirrored from RightSidebar for status bar
└── shell/
    ├── mod.rs
    ├── top_bar.rs          40px chrome: 56px traffic-light gutter + L/R panel toggles + wordmark
    ├── left_rail/          250px workspace + nav rail (replaces old sidebar stub)
    │   ├── mod.rs          LeftRail entity; owns WorktreePanel for state
    │   ├── nav_section.rs  NavItem (Tasks/Automations/Agents/Search) + pure bg/fg helpers
    │   ├── workspace_list_render.rs  build_workspace_row_plan (pure) + render
    │   └── toolbar.rs      Add Project + settings (stubs)
    ├── command_palette/    Cmd+P / Cmd+Shift+P modal overlay
    │   ├── mod.rs          PaletteModal entity (open/close/mode/query state)
    │   ├── entry.rs        PALETTE_COMMANDS (11 actions, fn-ptr factories) + QUICK_OPEN_STUBS
    │   ├── match_engine.rs pure scorer: prefix > consecutive > subsequence (no external crate)
    │   └── palette_modal.rs pure render: card + header chip + result list
    ├── welcome_view.rs     centered empty-state card (logo + wordmark + tagline + kbd hints)
    ├── main_pane.rs        pane binary-tree; split/close/focus actions
    ├── pane_tree.rs        pure PaneTree data structure (weight-aware)
    ├── pane_layout.rs      layout helpers
    ├── tabbed_pane.rs      TabbedPane entity: tab strip + active terminal
    ├── main_area.rs        thin dispatcher → welcome_view::view
    ├── status_bar.rs       left | center git zone | right metric strip (N TTY | N agents | N panes)
    │                       pure helpers: tty_label / agent_label / pane_label / metric_color
    ├── terminal_view.rs    TerminalView GPUI entity; poll task; blink; focus
    ├── terminal_row.rs     build_row / group_runs / effective_colors
    ├── terminal_palette.rs CellColor → Hsla resolver (charcoal + xterm-256)
    ├── terminal_search.rs  find_matches / visible_match_ranges (pure)
    ├── terminal_search_state.rs SearchState struct
    ├── terminal_search_overlay.rs overlay render
    ├── key_input.rs        Keystroke → PTY bytes (xterm escapes, C0, Alt-prefix)
    ├── cell_metrics.rs     character cell size constants
    ├── file_explorer/      FileExplorer entity; virtualized git-aware file tree (uniform_list, lazy load, git status badges)
    │   ├── mod.rs          FileExplorer entity; state machine; window-activation refresh trigger
    │   ├── tree_state.rs   flat-row build, expand toggle, should_include filter
    │   ├── status_display.rs BadgeStatus, STATUS_LABELS/COLORS, priority ladder, folder propagation
    │   ├── row_render.rs   build_row_plan pure helper → RowPlan
    │   └── fs_load.rs      async tokio read_dir; 5s timeout; symlink skip; 12-deep guard
    ├── right_sidebar/
    │   ├── mod.rs          RightSidebar entity; tab switching; hosts FileExplorer (Explorer) + SearchPanel (Search) + GitPanel+DiffView (SourceControl)
    │   ├── tab.rs          RightTab enum: Explorer | Search | SourceControl; icon_path() per tab
    │   ├── activity_bar.rs top tab bar (SVG icons) + persistent collapsed rail + PanelRight toggle
    │   └── layout.rs       layout constants
    ├── search_panel/       SearchPanel entity; ripgrep --json shell-out; debounced + cancellation
    │   ├── mod.rs          SearchPanel entity; debounce timer; monotonic search-id cancellation
    │   ├── search_state.rs pure SearchOptions (query, case/word/regex, include/exclude globs)
    │   ├── rg_runner.rs    async rg subprocess + NDJSON stream parse; per-file cap; rg-missing detection
    │   ├── rows.rs         pure build_search_rows: interleave file headers + matches; collapse handling
    │   ├── match_render.rs pure truncate_before (VSCode lcut port) — multi-byte safe
    │   ├── header_render.rs query input + Aa/ab/.* toggles + include/exclude glob fields + summary banner
    │   └── result_row.rs   paint file/match rows with highlight span
    ├── git_panel/
    │   ├── mod.rs          GitPanel entity; file-list render
    │   └── changed_files.rs partition helper (staged / unstaged / untracked)
    ├── diff_view/
    │   ├── mod.rs          DiffView entity; load(path, staged); expand()
    │   └── render.rs       pure hunk/line render plan helpers
    ├── commit_dialog/
    │   ├── mod.rs          CommitDialog entity; cycle_prefix(); submit()
    │   └── prefix.rs       conventional-commit prefix list (pure)
    ├── confirm_dialog/
    │   ├── mod.rs          ConfirmDialog entity; is_confirmed(); typed_matches()
    │   └── logic.rs        pure is_match helper
    ├── stash_panel/
    │   ├── mod.rs          StashPanel entity; refresh/apply/pop/request_drop
    │   └── list_render.rs  pure row label helper
    └── worktree_panel/
        ├── mod.rs          WorktreePanel entity; refresh/submit_create/pending_remove
        └── list_render.rs  pure label/suggest-path helpers
```

---

## crates/git — module map

```
src/
├── lib.rs          re-exports: Repository, StatusPoller, PollState, GitError
├── repository.rs   Repository::open (async, git rev-parse); owns StatusPoller
├── process.rs      GitCmd builder: kill_on_drop, 10s timeout, env sandbox
│                   (LANG=C, GIT_TERMINAL_PROMPT=0, GIT_CONFIG_NOSYSTEM=1)
├── error.rs        GitError enum (6 variants incl. NotInstalled, NotARepo)
├── status.rs       parse_porcelain_v2; GitStatus, FileStatus
├── poller.rs       StatusPoller: 500ms tick, focus-gated, AbortHandle drop
├── diff.rs         parse_unified_diff → Vec<FileDiff>; Repository diff methods
├── stage.rs        stage/unstage file + hunk (git apply --cached)
├── commit.rs       Repository::commit + commit_paths
├── stash.rs        push/list/apply/pop/drop + is_dirty precheck
├── branch.rs       list/create/switch
├── worktree.rs     add/list/remove (branch convention oximux/<slug>)
└── merge.rs        merge with auto-stash recovery; MergeOutcome
```

---

## crates/core — domain types

```
src/
├── lib.rs
├── git_state.rs    GitState, FileStatus, IndexStatus, WorktreeStatus, RenameInfo
│                   (serde-derived; zero tokio/GPUI deps)
├── git_diff.rs     FileDiff, DiffHunk, DiffLine, DiffStatus, DiffParseError
│                   parse_unified_diff (sync pure fn)
└── git_ops.rs      StageOp, MergeOutcome, MergeResult
```

---

## crates/agents — agent runtime (Phase 3)

```
src/
├── lib.rs
├── runtime.rs          AgentRuntime: Send+Sync+'static async trait (async-trait)
│                       AgentSessionConfig { adapter, worktree_path, prompt, model, effort, env,
│                         cols, rows, custom_command: Option<(String, Vec<String>)> }
│                       AgentStatusStream = watch::Receiver<AgentStatus>
│                       Methods: start_session / send_message / cancel / subscribe_status / current_status
│                       cancel() doc: SIGTERM-grace dance deferred to step 13; currently SIGKILL
├── runtime_impl.rs     CliRuntime — first concrete AgentRuntime impl
│                       Adapter registry: HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>
│                       Per-session state: own PortablePtyBackend behind Arc<Mutex<Box<dyn TerminalBackend>>>
│                         + tokio 50ms poll task draining PTY events into StatusMachine
│                         + watch::channel<AgentStatus> (multi-subscriber fan-out to badge / sidebar / dashboard)
│                       start_session: spawn_blocking(openpty/fork) + optional stdin_seed write
│                       cancel: spawn_blocking(drain_events+close) → await poll handle;
│                         select!{sleep/handle} — aborts handle on timeout
│                       MAX_SAFE_STDIN_SEED = 4096 — soft cap; warn-level guard on larger seeds
│                       Future ACP runtime (v1.1) will be a sibling impl with identical watch::Receiver surface
├── status_machine.rs   StatusMachine: Arc<[StatusPattern]> + 1 KiB ring buffer
│                       feed(bytes, now) — first-match-wins; fallback Idle→Running on any output
│                       tick(now) — 5s idle decay (Running→Idle only; blocking/terminal immune)
│                       note_exit(code) — idempotent terminal transition
│                       force(status) — manual override; rejects terminal states
└── cli/
    ├── mod.rs
    ├── adapter.rs      CliAgentAdapter: Send+Sync+'static async trait
    │                   CommandSpec { program, args, env, stdin_seed }
    │                   stdin_seed: caller owns trailing `\n`; runtime never appends (matches send_message contract)
    │                   StatusPattern { regex::bytes::Regex, target_status } — bytes engine: raw PTY not guaranteed UTF-8
    │                   pub(crate) const EMPTY_PATTERNS — shared by all zero-pattern adapters
    ├── detect.rs       pub(crate) async fn which_on_path(bin) -> bool
    │                   Shared helper: shells out to `which`, never panics, false on miss
    ├── claude_code.rs  ClaudeCodeAdapter — interactive PTY launch of `claude`
    │                   build_command: optional --model/--effort then prompt as trailing positional
    │                   status_patterns: 2 NeedsApproval rules (workspace-trust / tool-approval)
    │                   patterns omit leading `\b` so ANSI-SGR-prefixed prompts still match
    ├── codex.rs        CodexAdapter — interactive PTY launch of `codex`
    │                   build_command: optional -m <model> then prompt as trailing positional
    │                   cfg.effort silently ignored (no CLI analog); no --ask-for-approval / --sandbox
    │                   (user's ~/.codex/config.toml owns approval cadence per v0.9 retro)
    │                   status_patterns() intentionally empty — deferred to step-14 dogfood capture
    ├── aider.rs        AiderAdapter — interactive PTY launch of `aider`
    │                   build_command: optional --model <m> long flag
    │                   cfg.prompt → stdin_seed (Aider's REPL has no positional-prompt arg;
    │                     --message is one-shot incompatible with interactive PTY)
    │                   embedded `\n` submits each line as a discrete REPL prompt
    │                   no --yes / --auto-commits overrides (user's ~/.aider.conf.yml owns)
    │                   status_patterns() intentionally empty — deferred to step-14 capture
    └── custom.rs       CustomCommandAdapter — escape-hatch CliAgentAdapter impl
                        Reads custom_command: Option<(String, Vec<String>)> from AgentSessionConfig
                        status_patterns() intentionally empty — falls through to StatusMachine defaults
                        (output→Running, 5s silence→Idle, exit→Done/Failed); always-detects
```

`crates/core/src/lib.rs`:
- `AgentAdapter` enum gains `Custom` variant; `#[derive(Hash)]` added for registry keying

`crates/core/src/agent_session.rs` (domain types, zero tokio dep):
- `AgentSessionId(u64)` — field private; constructed via `new()`, read via `get()`. Unforgeable outside runtime.
- `AgentStatus` enum: `Idle | Running | WaitingForInput | NeedsApproval(String) | Done { code } | Failed(String)`
- Helpers: `is_blocking()`, `is_terminal()`

---

## crates/pty — terminal backend

| File | Role |
|---|---|
| `backend.rs` | `TerminalBackend` trait: spawn/write/resize/snapshot/search_grid |
| `portable_pty_backend.rs` | PTY via portable-pty; bounded sync_channel(256); watcher thread per session |
| `state.rs` | `TerminalState`: alacritty_terminal + vte processor; 5000-row scrollback |
| `snapshot.rs` | `TerminalSnapshot`: cells grid + cursor position |
| `events.rs` | `TerminalEvent` enum: Output/Exit/Resize/TitleChange |
| `close_grace.rs` | `term_step` SIGTERM-to-process-group + `close_with_grace(WatcherHandle, term_fn, kill_fn, GRACE, POLL)` orchestrator (5 s SIGTERM grace → SIGKILL fallback) |

---

## crates/storage — SQLite persistence (Phase 4)

| File | Role |
|---|---|
| `lib.rs` | Re-exports `Db`, `open`, `open_memory`, `StorageError`, `Migration`, `MIGRATIONS`, and 5 repository structs |
| `db.rs` | `Db(Arc<Mutex<Connection>>)` newtype; `open(path)` (file-backed, WAL) + `open_memory()` (test helper, `#[doc(hidden)]`); `with_conn(|c| …)` closure accessor; `set_pragmas` applies WAL/foreign_keys/busy_timeout=5000/synchronous=NORMAL/wal_autocheckpoint=1000 on every connection |
| `migrations.rs` | `Migration { version, name, sql }`; `run_migrations(conn, &[Migration])` per-migration transactions; bookkeeping in `__oximux_migrations(version, name, applied_at)`; downgrade-by-max ordered before gap detection; `strip_sql_comments` helper so `contains_transaction_keyword` guard ignores prose; `migration_ladder_matches_files` CI guard |
| `error.rs` | `StorageError` (thiserror): `Open`, `Pragma`, `Migration{version,source}`, `SchemaMigrationDowngrade{db_version,code_version}`, `Query`, `Conflict{table,constraint}` |
| `model.rs` | Row types (`ProjectRow`, `WorkspaceRow`, `AgentSessionRow`, `PaneSessionRow`) with `from_row(&rusqlite::Row)` + `From<XxxRow> for Xxx` impls returning `oximux-core` domain types; unknown `AgentStatus` slug degrades to `Interrupted` |
| `repositories/mod.rs` | Shared helpers: `now()` (RFC 3339), `new_id()` (UUIDv4), `classify_unique` (maps SQLite UNIQUE/PK extended codes 2067/1555 → `StorageError::Conflict`). Module doc locks the silent-ok-on-missing-row contract |
| `repositories/project.rs` | `ProjectRepo`: insert / get_by_id / list_recent (LIFO by last_opened) / update_last_opened_at / delete (FK CASCADE) |
| `repositories/workspace.rs` | `WorkspaceRepo`: insert (UNIQUE(project_id, slug) → `Conflict`) / get_by_id / list_for_project (excludes archived) / mark_archived / rename / delete (worktree-rollback contract) |
| `repositories/agent_session.rs` | `AgentSessionRepo`: insert (starts `Idle`) / get_by_id / list_for_workspace / update_status (3-column codec) / update_ended_at / list_running_at_shutdown (machine-checked vs `AgentStatus::Running.as_str()`) |
| `repositories/pane_session.rs` | `PaneSessionRepo`: insert / get_by_id / list_for_workspace (oldest-first restore) / update_grid_position / delete |
| `repositories/settings.rs` | `SettingsRepo`: get / set (upsert) / delete; caller enforces ≤ 64 KiB |
| `migrations/V001__init.sql` | 5 tables (projects, workspaces, agent_sessions [+ exit_code, status_detail for AgentStatus payloads], pane_sessions [ON DELETE SET NULL on agent FK], settings) + 3 FK-support indexes |

`r2d2` deliberately **not** adopted in v1 — single `Arc<Mutex<Connection>>` over WAL is sufficient for the single-writer model. Read-pool split (write conn + N readers) is the documented upgrade path if profiling shows contention.

---

## crates/editor — code editor + LSP (Phase 5)

> Step 1 spike (go): `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`  
> Step 2 (go): `plans/reports/tester-260522-1939-phase-05-step02-editor-save.md` — smoke green, 8/10 code review

```
src/
├── lib.rs                  re-exports EditorView, SaveFile action, lsp module
├── editor_view.rs          EditorView GPUI entity (step 1+2)
│                           Fields: file_path, uri (lsp_types::Uri, parse-once),
│                             state (Entity<InputState>), focus_handle,
│                             lsp_client (Option<Arc<LspClient>>),
│                             dirty (bool), doc_version (i32, starts at 1),
│                             last_sent_text (String), _observe_sub
│                           Step 1: gpui-component Input in code_editor("rust") mode;
│                             attach_lsp → HoverProvider + publishDiagnostics pump
│                           Step 2: cx.observe wired in new(); SaveFile action
│                             (declared here for crate-cycle reasons); on_save writes
│                             file via fs::write, sends didSave; window title shows
│                             " •" dirty badge; impl Drop sends didClose via sync mpsc
│                           Keyed by --editor-spike CLI flag
├── lsp_bridge.rs           spawn_attach_lsp (factored from editor_view.rs)
│                           Runs LSP handshake on tokio; calls set_lsp_client on
│                           EditorView entity; passes did_open_text for catch-up
│                           didChange when buffer drifted during handshake window
└── lsp/
    ├── mod.rs              module surface
    ├── transport.rs        Content-Length framing read/write; 6 unit tests
    ├── client.rs           LspClient: spawn child + handshake + request/notify +
    │                       dispatch; captures tokio::runtime::Handle for GCD-bridge;
    │                       server-initiated requests answered with {result:null};
    │                       REQUEST_TIMEOUT = 5s; 8 unit tests
    │                       Step 2: did_change / did_save / did_close — accept
    │                       &lsp_types::Uri (parse-once, no per-keystroke allocation)
    └── providers.rs        LspHoverProvider bridges gpui::Task ← tokio via
                            handle.spawn (Rc<LspHoverProvider> — local executor only)
```

```
tests/
├── lsp_notification_serialization.rs   4 integration tests (step 2): did_change
│                                       full-sync JSON shape, did_save, did_close,
│                                       version monotonic; no GPUI runtime needed
└── file_tree_tests.rs                  7 integration tests (step 3): load, expand,
                                        collapse, refresh, skip-names, watcher round-trip;
                                        tempfile::TempDir with .git marker (required for
                                        WalkBuilder gitignore engagement)
```

### file_tree/ — headless file-tree backend (Phase 5 step 3)

```
src/file_tree/
├── mod.rs      FileTree GPUI entity; TreeNodeId / FileTreeNode / FileTreeEvent
│               SKIP_NAMES const (shared filter list — prevents walker/watcher divergence)
│               remove_subtree: recursive eviction from nodes + open_dirs on re-expand
│               cx.spawn drives the watcher event loop
├── walker.rs   ignore-crate WalkBuilder::max_depth(1) + filter_entry(SKIP_NAMES)
│               returns (PathBuf, bool) direct children; sort_entries: dirs-first alpha
└── watcher.rs  spawn_watcher wraps notify_debouncer_full::new_debouncer (200ms)
                closure forwards DebounceEventResult into tokio mpsc
                is_ignored + find_node_to_invalidate are pure free fns
```

`FileTreeEvent` variants: `Loaded(TreeNodeId)`, `Refresh(TreeNodeId)`, `WatchError(String)`.  
Step 4 owns UI diffing; step 3 emits coarse `Refresh(id)` only.

Workspace deps: `lsp-types = "0.97"`, `url = "2"` (percent-encoding for file URIs).  
New action: `SaveFile` (in `oximux-editor`; bound in `app/src/main.rs` via `use oximux_editor::SaveFile`).

---

## crates/relay — relay daemon (Phase 5)

```
src/
├── main.rs         CLI entry: --pid-file, --log-dir flags
│                   Layered tracing: stderr text + daily-rolled JSON (tracing-appender, 7-day purge)
│                     + macOS oslog mirror; OXIMUX_RELAY_TRACE=1 → trace level
│                   Calls purge_old_logs at startup; boots tokio + Server
├── server.rs       Server: UnixListener accept loop; Notify-based graceful shutdown
│                   Handles Request::Shutdown via Notify signal; SIGTERM/SIGINT handler
│                   ServerConfig { pid_path, idle_timeout, idle_tick_interval, … }
│                   SessionGuard ref count + spawn_idle_gc task (reaps sessions idle > idle_timeout)
│                   PidGuard (mirrors SocketGuard pattern — unlinks pid file on drop)
└── registry.rs     PtyRegistry: per-Entry AtomicU64 counters (bytes_in / bytes_out) + started_at Instant
                    PtyRegistry::stats() → Vec<PtyStats>
```

**relay-proto** (`crates/relay-proto/src/messages.rs`) additions:
- `Request::Stats` — ask daemon for live PTY metrics
- `Response::StatsOk(Vec<PtyStats>)` — reply carrying per-PTY counters
- `PtyStats { pty_id, bytes_in, bytes_out, alive_secs }`

**crates/app** additions (`relay_supervisor.rs`):
- `SupervisorError` enum: `VersionMismatch` | `Other`
- `read_pid` / `watch_pid` — 1Hz `kill(pid, 0)` heartbeat loop
- `ExistingConnect` typed variant for attach-to-running-relay path
- `pid_alive` via `std::io::Error::last_os_error()` (signal-safe)
- `boot_relay_supervisor` takes `PaneRelayIdRepo`; spawns crash heartbeat;
  branches on `VersionMismatch` → macOS banner, no auto-respawn

**scripts** additions:
- `scripts/oximux-launchd-install.sh` — opt-in launchd agent installer; `plutil`-lints plist; refuses if token file absent
- `scripts/oximux-uninstall.sh` — full uninstall hygiene (socket, pid, log dir, launchd label)

---

## Key runtime flows

### Startup
`main.rs` boots tokio → `Repository::open(cwd)` (best-effort) → `cx.open_window` → `WorkspaceRoot::new`. If no git repo at cwd, `right_sidebar` is `None`; RightSidebar hidden.

### Git polling
`WorkspaceRoot` owns `right_sidebar: Option<Entity<RightSidebar>>`. `RightSidebar` holds the `StatusPoller` + `GitPanel` + `DiffView` (SourceControl tab). `StatusPoller` ticks every 500ms (pauses on window blur), calls `send_if_modified` — only emits when state actually changes. On change: `entity.update(cx, …)` → `cx.notify()` → status bar center zone re-renders via `RightSidebar::latest_poll_state()`.

### Terminal data flow
PTY process → watcher thread → `state.advance(&buf)` → `TerminalEvent::Output` → 16ms poll task drains channel → `cx.notify()` → `terminal_view` re-renders visible rows only.

---

## CI guards (xtask file-size-lint)

| Threshold | Action |
|---|---|
| > 500 non-blank LOC | warn |
| > 800 non-blank LOC | fail |

Applies to all `.rs` files in `crates/`. `shell/mod.rs` monolith pattern from v0.9 is blocked by this guard.
