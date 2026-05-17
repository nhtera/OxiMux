# OxiMux — System Architecture

**Updated**: 2026-05-17  
**Phase**: 2 code-complete

---

## Layer overview

```
┌─────────────────────────────────────────────────────┐
│  GPUI UI layer  (crates/app)                        │
│  WorkspaceRoot → MainPane/TabbedPane/TerminalView   │
│                → GitMount (GitPanel + DiffView)     │
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

## WorkspaceRoot + GitMount wiring

```
WorkspaceRoot (GPUI entity)
├── fields
│   ├── main_pane: Entity<MainPane>
│   ├── git: Option<GitMount>
│   └── poll_state: Option<PollState>   ← mirrored for status bar
│
└── GitMount (substruct, present only when cwd is a git repo)
    ├── _repo: Arc<Repository>
    ├── _poller: StatusPoller           ← AbortHandle; drops → task stops
    ├── git_panel: Entity<GitPanel>
    └── diff_view: Entity<DiffView>
```

`WorkspaceRoot::new` calls `make_git_mount(repo, cx)` when `Repository::open` succeeds. `StatusPoller::spawn` wires the 500ms tick. On each non-duplicate status delta, the poller calls back into `WorkspaceRoot` via `entity.update` to mirror `poll_state`, then `cx.notify()` propagates to all subscribers including the status bar center zone.

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
