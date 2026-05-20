# OxiMux — Codebase Summary

**Updated**: 2026-05-20  
**Phase**: 3 foundation slice complete (steps 1-3); right-sidebar Phase 03 (Search Panel) complete  
**Tests**: 494 passed, 0 failed (workspace)

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
    ├── editor/             gpui-component editor stub (Phase 5)
    ├── storage/            SQLite stub (Phase 4)
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

## crates/agents — agent runtime (Phase 3 foundation)

```
src/
├── lib.rs
├── runtime.rs          AgentRuntime: Send+Sync+'static async trait (async-trait)
│                       AgentSessionConfig { adapter, worktree_path, prompt, model, effort, env, cols, rows }
│                       AgentStatusStream = watch::Receiver<AgentStatus>
│                       Methods: start_session / send_message / cancel / subscribe_status / current_status
├── status_machine.rs   StatusMachine: Arc<[StatusPattern]> + 1 KiB ring buffer
│                       feed(bytes, now) — first-match-wins; fallback Idle→Running on any output
│                       tick(now) — 5s idle decay (Running→Idle only; blocking/terminal immune)
│                       note_exit(code) — idempotent terminal transition
│                       force(status) — manual override; rejects terminal states
└── cli/
    ├── mod.rs
    └── adapter.rs      CliAgentAdapter: Send+Sync+'static async trait
                        CommandSpec { program, args, env, stdin_seed }
                        StatusPattern { regex::bytes::Regex, target_status } — bytes engine: raw PTY is not guaranteed UTF-8
```

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
