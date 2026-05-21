# OxiMux — System Architecture

**Updated**: 2026-05-20  
**Phase**: 3 — steps 1-4 + CliRuntime done; steps 5-14 pending

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
| Editor + LSP (rust-analyzer) | Phase 5 |
| SQLite persistence / session restore | Phase 4 |
| Multi-agent dashboard | Phase 7 |
| embeddable terminal library terminal backend | v2 (ADR in brief.md) |
