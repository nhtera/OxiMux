# OxiMux — Project Changelog

Entries are newest-first. Each entry links to the commit SHA and notes what shipped.

---

### 2026-05-22 — Phase 5 / Step 02 — Editor save round-trip + LSP textDocument lifecycle

**Status**: all automated gates pass (cargo check, clippy -D warnings, 21 editor tests, file-size-lint); smoke green  
**Code-review score**: 8/10  
**Cook report**: `plans/reports/cook-260522-1939-phase-05-step02-editor-save-roundtrip.md`  
**Touches**: `crates/editor/`, `crates/app/src/main.rs`

#### feat(editor): user-facing

- **Cmd+S** saves the buffer to disk (UTF-8, `std::fs::write`); reports error via `tracing::error` if write fails, dirty flag stays set so user can retry
- **Dirty badge**: window title shows ` •` suffix (`"OxiMux — main.rs •"`) when buffer diverges from disk; clears on successful save
- **Undo/redo with LSP sync**: Cmd+Z / Cmd+Shift+Z (gpui-component built-in) now keeps rust-analyzer in sync — `cx.observe` pattern catches silent undo/redo edits that bypass `InputEvent::Change`
- **LSP live edits**: rust-analyzer receives `textDocument/didChange` on every text change (squiggles update before save); `textDocument/didSave` on Cmd+S; `textDocument/didClose` when editor window closes

#### refactor(editor): internal

- **New module `crates/editor/src/lsp_bridge.rs`** (145 LOC): `spawn_attach_lsp` extracted from `editor_view.rs` to stay under file-size lint; handles handshake-completion catch-up `didChange` when buffer drifted during handshake window
- **LSP client API**: `did_change` / `did_save` / `did_close` accept `&lsp_types::Uri` (parse-once; eliminates per-keystroke URI allocation — H1 fix from code review)
- **`decide_change_propagation` pure fn**: extracted from observe callback; 3 unit tests; guards cursor-move no-ops and computes version increment
- **`SaveFile` action** declared in `oximux-editor` crate (crate-cycle workaround; bound in `app/src/main.rs`)
- **New integration tests** (`tests/lsp_notification_serialization.rs`, 4 tests): `didChange` full-sync JSON shape, `didSave`, `didClose`, version monotonic; no GPUI runtime needed

#### Known limitations (non-blocking for step 2)
- `dirty_set_on_change` behavioral test deferred — requires GPUI test harness (step 7)
- Edits made during LSP handshake window silently dropped until `set_lsp_client` completes; catch-up `didChange` covers the gap at handshake completion (Fix #3)
- `fs::write` is not atomic (no temp-file + rename); hardening deferred to step 8/14

---

### 2026-05-22 — Phase 5 / Step 01 — Editor + LSP spike

**Status**: code complete; 14 editor unit tests green; cargo check/clippy clean; go/no-go pending manual smoke  
**Cook report**: `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`  
**Touches**: `crates/editor/`, `crates/app/src/main.rs`, root `Cargo.toml`

- **EditorView** (`editor_view.rs`): GPUI entity wrapping `gpui-component` Input in `code_editor("rust")` mode; `attach_lsp` spawns rust-analyzer, installs `HoverProvider`, pumps `publishDiagnostics` to `WeakEntity<InputState>`.
- **LspClient** (`lsp/client.rs`): Content-Length framing; initialize/initialized/didOpen handshake; request timeout 5s; server-initiated requests answered `{result:null}` (prevents rust-analyzer hangs on `client/registerCapability`); captures `tokio::runtime::Handle` at spawn to bridge GCD ↔ tokio.
- **LspHoverProvider** (`lsp/providers.rs`): bridges `gpui::Task` ← tokio via `handle.spawn`; `Rc<LspHoverProvider>` scoped to local executor.
- **Transport** (`lsp/transport.rs`): read/write with `Content-Length` framing; 6 unit tests including EOF-mid-header.
- **`--editor-spike` flag** (`crates/app/src/main.rs`): short-circuits normal workspace boot; opens single editor window on `crates/app/src/main.rs`; self-aborts with clear message if tokio handle not in scope.
- **`url` crate** used for `path_to_file_uri` — percent-encodes paths with spaces (prevents silent diagnostic mismatches).
- Spike is **read-only**: no `didChange`, no save, no dirty flag (step 2 owns that).

**Manual smoke** (go/no-go gate — requires interactive macOS session):
```bash
cargo run -p oximux-app -- --editor-spike
```

---

### 2026-05-22 — Phase 5 / Step 07 — relay daemon hardening

**Status**: 10 sub-steps complete; 6 new relay integration tests + 4 supervisor unit tests, all green; clippy clean  
**Touches**: `crates/relay/`, `crates/relay-proto/`, `crates/app/`, `scripts/`

- **Graceful shutdown**: `Notify`-based SIGTERM/SIGINT handler in `server.rs`; `Request::Shutdown` wired to same `Notify`; `PidGuard` cleans up pid file on drop (mirrors `SocketGuard`).
- **Idle GC + Stats**: `spawn_idle_gc` task reaps sessions idle past `ServerConfig::idle_timeout`; `PtyRegistry` gains per-entry `AtomicU64` byte counters + `started_at`; new `Request::Stats` / `Response::StatsOk(Vec<PtyStats>)` proto messages expose live PTY metrics.
- **Structured logging + log rotation**: layered tracing subscriber — stderr text + daily-rolled JSON via `tracing-appender` + macOS oslog mirror; `OXIMUX_RELAY_TRACE=1` opens trace level; `purge_old_logs` sweeps `relay.log.YYYY-MM-DD` files older than 7 days at startup; `--pid-file` and `--log-dir` CLI flags.
- **Crash heartbeat + version guard**: `relay_supervisor.rs` adds `SupervisorError::VersionMismatch`; 1Hz `watch_pid` loop; on relay death calls `on_relay_died` (sqlite orphan cleanup + AppKit banner); `VersionMismatch` shows macOS notification and parks in degraded mode — no auto-respawn.
- **Install scripts**: `scripts/oximux-launchd-install.sh` (opt-in launchd agent; `plutil`-lints plist; refuses if token absent) + `scripts/oximux-uninstall.sh` (full hygiene).

---

### 2026-05-20 — Phase 3 Step 4 + CliRuntime — CustomCommandAdapter + first concrete AgentRuntime

**Status**: step 4 + CliRuntime code complete; 16 new tests, all green (510 workspace total)  
**Plan**: `plans/reports/cook-260520-1448-phase-03-step04-cli-runtime.md`  
**Code-review score**: DONE_WITH_CONCERNS — all Critical/High items resolved in-slice

#### What shipped

| File | Role |
|---|---|
| `crates/agents/src/cli/custom.rs` | `CustomCommandAdapter` — escape-hatch `CliAgentAdapter`; reads `custom_command: Option<(String, Vec<String>)>` from config; empty `status_patterns()` (StatusMachine defaults handle output→Running / silence→Idle / exit→Done/Failed); always-detects |
| `crates/agents/src/runtime_impl.rs` | `CliRuntime` — first concrete `AgentRuntime` impl; adapter registry (`HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>`); per-session `PortablePtyBackend` + 50ms tokio poll task + `watch::channel<AgentStatus>`; `cancel` does `drain_events+close` in `spawn_blocking` then awaits poll handle with timeout+abort |
| `crates/core/src/lib.rs` | `AgentAdapter::Custom` variant; `#[derive(Hash)]` for registry keying |
| `crates/agents/src/runtime.rs` | `AgentSessionConfig::custom_command` field; `cancel()` doc notes step-13 SIGTERM-grace deferral |

#### Design notes
- `CliRuntime` is the canonical `AgentRuntime`. Future ACP runtime (v1.1) will be a sibling impl; both expose identical `watch::Receiver<AgentStatus>` to UI.
- `StatusPattern` uses `regex::bytes::Regex` — PTY output is not guaranteed UTF-8.
- `MAX_SAFE_STDIN_SEED = 4096` — soft cap; `warn!` guard on larger seeds.
- `custom_command` field on `AgentSessionConfig` is acknowledged debt (M1) — step 10 launch dialog refactors to a typed `AdapterConfig` enum.

#### Resolved during review
- C1: `drain_events()` before `close()` inside `spawn_blocking` — eliminates cancel deadlock
- H1: `select!{handle / sleep}` + abort on timeout — poll task no longer leaks on cancel timeout
- H2: `cancel()` doc now explicitly notes step-13 SIGTERM-grace deferral (impl is SIGKILL today)
- H3: `MAX_SAFE_STDIN_SEED = 4096` warn guard — large seeds won't stall the `spawn_blocking` thread silently
- M2: test `subscribe_then_cancel_publishes_terminal_status` — UI contract: badge sees final state across cancel
- M3: test `double_cancel_second_call_errors` — session table is source of truth

#### Known limitations / deferred
- No natural-exit session-table cleanup — entry stays until `cancel()` called; step 9 (pane integration) owns reaping.
- SIGTERM-grace dance deferred to step 13; current `cancel()` is SIGKILL (reap-before-resolve honored).
- No detection registry yet — step 8; until then `register_adapter()` called manually.
- `pub mod runtime_impl` may expose internals (M4) — tighten to `pub(crate)` if a 2nd internal helper goes pub.

---

### 2026-05-20 — Phase 3 Foundation (steps 1-3) — Agent Runtime Traits

**Status**: steps 1-3 code complete; 18 new tests, all green (494 workspace total)  
**Plan**: `plans/reports/cook-260520-1242-phase-03-foundation.md`  
**Code-review score**: 7/10 — all High/Medium items resolved in-slice

#### What shipped

| File | Role |
|---|---|
| `crates/core/src/agent_session.rs` | `AgentSessionId(u64)` newtype (private field); `AgentStatus` enum (6 variants); `is_blocking()` / `is_terminal()` helpers |
| `crates/agents/src/runtime.rs` | `AgentRuntime` async trait (`async-trait`); `AgentSessionConfig`; `AgentStatusStream = watch::Receiver<AgentStatus>` (multi-subscriber fan-out) |
| `crates/agents/src/cli/adapter.rs` | `CliAgentAdapter` async trait; `CommandSpec`; `StatusPattern` (regex::bytes — raw PTY is not guaranteed UTF-8) |
| `crates/agents/src/status_machine.rs` | `StatusMachine`: 1 KiB ring buffer; `feed` / `tick` (5s idle decay) / `note_exit` / `force`; 18 unit tests |

#### Resolved during review
- H1: replaced `mpsc::Receiver` with `watch::Receiver` — multi-subscriber fan-out for badge + sidebar + dashboard
- H2: ring cleared on blocking-entry transition — stale bytes can't re-match after state clears
- M2: `AgentSessionId` field made private; forgery prevented
- L1: `force()` now rejects terminal states

#### Known limitations / deferred
- No concrete adapter yet — steps 4-7 downstream. Trait surface logic-tested only.
- `current_status()` returns `anyhow::Result`; typed `AgentError` deferred to Phase 4.
- Pre-existing `cargo fmt` drift in `crates/app` (not regressed here); needs a cleanup slice.

---

### 2026-05-20 — Right-Sidebar Phase 03 — Search Panel

**Status**: code complete; 476 workspace tests passing (0 failed)
**Plan**: `plans/260517-1821-right-sidebar-panels/phase-03-search-panel.md`

#### What shipped

New `crates/app/src/shell/search_panel/` module wired into the Search tab of `RightSidebar`.

| File | Role |
|---|---|
| `mod.rs` | `SearchPanel` GPUI entity; 300ms debounce; monotonic search-id cancellation; InputState event subscriptions |
| `search_state.rs` | pure `SearchOptions` (query, case/word/regex, include/exclude globs) |
| `rg_runner.rs` | async `tokio::process::Command` spawn of `rg --json`; NDJSON stream-parse; per-file cap (100) and global cap (2000); 30s hard timeout; `kill_on_drop` |
| `rows.rs` | pure `build_search_rows` interleaver: file headers + matches, collapse handling |
| `match_render.rs` | pure VSCode-style `truncate_before` (26-byte pre-match cap); multi-byte char safe via `is_char_boundary` |
| `header_render.rs` | query input + Aa/ab/.* toggle row + include/exclude glob fields + summary banner state |
| `result_row.rs` | paint file header rows (chevron + name + match count) and match rows (line# + highlighted span) |

#### Key behaviors
- Backend: shell-out to `ripgrep --json`. Detect at startup; show install hint banner if missing.
- 300ms debounce via `cx.background_executor().timer`; monotonic `latest_search_id` drops stale results.
- Cancellation: `tokio::process::Child::start_kill()` + `kill_on_drop(true)` — no zombie processes on tab switch.
- Virtualized via `gpui::uniform_list`; file rows 28px, match rows 20px.
- Left-truncation keeps match span visible at narrow widths (BEFORE_MAX = 26 bytes).
- Empty states: rg-missing / no query / no results.
- Click file row toggles collapse; click match row opens file via `open` (editor jump deferred to Phase 5).

#### Files modified outside `search_panel/`
- `shell/mod.rs` — added module export
- `shell/right_sidebar/mod.rs` — Search tab wiring; replaces "Phase 03" placeholder
- `crates/app/Cargo.toml` — adds `serde`, `serde_json`, `thiserror` workspace deps
- Tests: `tests/search_smoke.rs`, `tests/search_rg_runner.rs`, `tests/fixtures/search/` fixture dir.
- Fixed pre-existing smoke test failures by initializing `gpui_component::init` in test setup (file_explorer_smoke, right_sidebar_smoke).

#### New tests (+27 over baseline)
- 20 lib unit tests across `search_panel/*` (pure modules)
- 6 integration tests in `tests/search_rg_runner.rs` (real ripgrep against fixtures)
- 1 gpui smoke test in `tests/search_smoke.rs`

---

### 2026-05-19 — Right-Sidebar Phase 02 — File Explorer Panel

**Status**: code complete; 439 workspace tests passing (0 failed)
**Plan**: `plans/260517-1821-right-sidebar-panels/phase-02-file-explorer.md`
**Code-review score**: 7.5 → all High/Medium items addressed before merge

#### What shipped

New `crates/app/src/shell/file_explorer/` module wired into the Explorer tab of `RightSidebar`.

| File | Role |
|---|---|
| `mod.rs` | `FileExplorer` GPUI entity; state machine; action handlers; `cx.observe_window_activation` refresh |
| `tree_state.rs` | flat-row build (`flatten`), expand toggle, `should_include` filter (skips `.git`/`node_modules`/`target`) |
| `status_display.rs` | `BadgeStatus` enum, `STATUS_LABELS`/`STATUS_COLORS`, priority ladder, folder propagation (Deleted+Ignored excluded from folder badge) |
| `row_render.rs` | `build_row_plan` pure helper → `RowPlan` consumed by `uniform_list` |
| `fs_load.rs` | async `tokio::fs::read_dir` wrapper; 5s `tokio::time::timeout` per load; symlink skip; 12-deep recursion guard |

#### Key behaviors
- Virtualized via `gpui::uniform_list`; 24px row height, 16px/depth indent; targets 10k+ rows at 60 fps
- Lazy directory load with `loaded`/`loading` flags; per-repo expanded-set persistence
- Git status badges M/A/D/R/U/C right-aligned; folder propagation shows dominant child badge
- Ignored entries rendered italic+dim; Deleted entries excluded from folder propagation
- Focus-regain refresh via `cx.observe_window_activation` (reuses cached dirs, no full rescan)
- Click file → `open <path>` (macOS default app); editor integration deferred to Phase 5

#### Plan deviations (minor)
- Symlink skip and 12-deep guard added (not in original spec) — prudent safety bounds
- Deleted entries excluded from folder propagation (spec said only Ignored excluded) — UX improvement accepted in code review
- Focus-refresh mechanism clarified to reuse cache rather than rescan

#### Files modified outside `file_explorer/`
- `shell/mod.rs` — added module export
- `shell/right_sidebar/mod.rs` — Explorer tab wiring; `window: &mut Window` threaded through `new`
- `workspace_root.rs` — passes window to `RightSidebar::new`
- `crates/app/tests/right_sidebar_smoke.rs` + `file_explorer_*.rs` — 90+ new tests
- `shell/welcome_view.rs` — one-line clippy fix

**Test delta**: +90 tests (349 → 439 workspace total)

---

### 2026-05-18 — Shell Polish (5 phases, plan completed)

**Commits**: `9729baf` (P01), `745a3ba` (P02), `c951ab1` (P03), `8f9248d` (P04), `2237a94` (P05)
**Status**: complete; total 344 tests passing across workspace
**Plan**: `plans/260518-0025-shell-polish/`

| Phase | Commit | What shipped |
|---|---|---|
| 01 — Titlebar Chrome | `9729baf` | Transparent macOS-native titlebar with traffic-light inset at `point(12, 12)`; new `ToggleLeftSidebar` action (Cmd+B); `top_bar::view` rewritten with 56px gutter + lucide `PanelLeft/Right` icons; `density.h_top_bar` 36→40 |
| 02 — Left Rail Shell | `745a3ba` | New `shell/left_rail/` module (mod + nav_section + workspace_list_render + toolbar) replacing the 30-line `sidebar.rs` stub; Tasks/Automations/Agents/Search nav rows; WORKSPACES header with placeholder filter/sort/+; workspace list reuses `WorktreePanel` data via pure render helpers; new `Theme.git: GitDecorations` field + `density.w_left_rail` (250px); 22 unit tests |
| 03 — Welcome State | `c951ab1` | New `shell/welcome_view.rs` — logo + wordmark + tagline + 5 keyboard hint rows (incl. `cmd-p` / `cmd-shift-p` for Phase 05) + version footer; `main_area.rs` slimmed to a thin dispatcher; pure `should_show_welcome` predicate |
| 04 — Status Bar Polish | `8f9248d` | Right zone metric strip (`N TTY \| N agents \| N panes`); new pure helpers `tty_label` / `agent_label` / `pane_label` / `metric_color`; `density.h_status_bar` 22→24; `view()` gains `agent_count: usize` (Phase 7 wires it) |
| 05 — Command Palette + Quick Open | `2237a94` | New `shell/command_palette/` module (mod + entry + match_engine + palette_modal); `OpenQuickOpen` / `OpenCommandPalette` actions bound to Cmd+P / Cmd+Shift+P; pure fuzzy scorer (prefix > consecutive > subsequence) — no external crate; 11-entry `PALETTE_COMMANDS` static catalog with `fn() -> Box<dyn Action>` factories; modal mounted as last child of `WorkspaceRoot` for topmost z-layer (terminal_search_overlay precedent) |

**Net workspace deltas:** +344 tests (was ~280), 0 failures. `cargo fmt` / `clippy --tests -- -D warnings` / `file-size-lint` clean on every phase. Plan path: `plans/260518-0025-shell-polish/`.

---

### 2026-05-17 — Right-Sidebar Phase 01

- feat(app): replace fixed git column with tab-switchable RightSidebar entity (Explorer/Search/SourceControl)
- shell migration: GitMount → RightSidebar (workspace_root.rs 231→187 LOC)
- keybindings: cmd-l toggle, cmd-shift-e/f/g tab select
- tests: +6 (3 visible_tabs/derive + 3 smoke incl. no-repo fallback)

---

## 2026-05-17 — Phase 2 code-complete (steps 9-14)

**Commits**: `ff05dbb`, `0b39ee6`, `0c5cb93`, `a69481c`, `8022258`, `5e1cc77`  
**Status**: code complete; step 15 = dogfood gate (ADR-003: 7 consecutive daily-driver days, zero panics)  
**Tests**: 285 passed, 0 failed (workspace)

### What shipped

| Step | Commit | Description |
|---|---|---|
| 9 — diff view UI | `ff05dbb` | `DiffView` entity + `render.rs` pure render plan helpers; `DiffViewState` (Idle/Loading/Loaded/Failed); `expand()` removes large-diff cap |
| 10 — commit dialog | `0b39ee6` | `CommitDialog` entity + `prefix.rs` conventional-commit prefix list; `cycle_prefix()` + `submit()` → `Repository::commit` |
| 11 — confirm dialog | `0c5cb93` | `ConfirmDialog` entity + `logic.rs` pure `is_match`; type-to-confirm gate for all destructive ops |
| 12 — stash UI | `a69481c` | `StashPanel` entity + `list_render.rs` pure row label; apply/pop/request_drop wired to git stash ops |
| 13 — worktree UI | `8022258` | `WorktreePanel` entity + `list_render.rs` pure label/suggest-path helpers; create/list/remove wired |
| 14 — shell wiring | `5e1cc77` | `main.rs` boots tokio + opens `Repository` at cwd (`Option`); `WorkspaceRoot` gains `GitMount` substruct (poller + `GitPanel` + `DiffView` + mirrored `PollState`); `status_bar.rs` center zone shows branch + ↑ahead ↓behind + dirty count |

### New module structure (`crates/app/src/shell/`)

```
diff_view/      mod.rs + render.rs
commit_dialog/  mod.rs + prefix.rs
confirm_dialog/ mod.rs + logic.rs
stash_panel/    mod.rs + list_render.rs
worktree_panel/ mod.rs + list_render.rs
```

---

## 2026-05-17 — Phase 2 steps 1-8

**Commits**: see `plans/260515-2012-oximux-v1-build/plan.md` step notes  
**Tests**: 234 at end of step 8

Steps 1-8 landed the full git backend: `Repository::open`, porcelain-v2 parser, `StatusPoller`, unified diff parser, file+hunk staging, stash/branch/worktree/merge wrappers, commit, and the `GitPanel` changed-files skeleton.

---

## 2026-05-16 — Phase 1 code-complete (steps 1-9)

Terminal cockpit: `PortablePtyBackend`, `TerminalView`, color+cursor rendering, `MainPane` binary-split tree, `TabbedPane` with mini-tabs, scrollback search, render coalescing, blink suppression. 35 tests.

---

## 2026-05-15 — Phase 0 scaffold

Workspace, 8 crates, charcoal theme, GPUI shell, CI guards (xtask file-size-lint), ADRs 001-005. Dogfood gate pending.
