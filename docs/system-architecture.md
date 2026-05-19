# OxiMux — System Architecture

**Updated**: 2026-05-19  
**Phase**: 2 code-complete + right-sidebar Phase 02 (File Explorer) complete

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
│  crates/pty   — TerminalBackend + PortablePtyBackend│
│  crates/git   — Repository, StatusPoller, git ops   │
│  crates/core  — shared domain types (no deps)       │
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
