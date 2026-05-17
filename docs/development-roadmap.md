# OxiMux — Development Roadmap

**Updated**: 2026-05-17  
**Gate rule**: each phase ships only after ≥7 consecutive daily-driver days with zero panics (ADR-003). Tests-passing alone is not sufficient.

---

## Phase table

| Phase | Capability | Status |
|---|---|---|
| 00 | Foundation: workspace, GPUI shell, CI guards | scaffold complete; dogfood pending |
| 01 | Terminal cockpit: multi-pane PTY, tabs, search, render perf | code complete; dogfood pending |
| **02** | **Git core: status, diff, stage, commit, stash, worktree UI** | **code complete (steps 1-14); step 15 = dogfood gate** |
| 03 | CLI agent integration: Claude Code, Codex, Aider adapters | pending |
| 04 | Workspace persistence: SQLite + session restore | pending |
| 05 | Editor + LSP: gpui-component code editor, file tree, rust-analyzer | pending |
| 06 | Git review polish: side-by-side diff, blame, conflict UI | pending |
| 07 | Multi-agent cockpit: dashboard, approval detection, presets | pending |
| 08 | Ship v1: docs, packaging, beta, v1.0 tag | pending |

---

## Phase 2 detail — code complete 2026-05-17

All 14 implementation steps shipped. Step 15 is the dogfood usage gate.

**Backend (crates/git)**
- `Repository::open`, porcelain-v2 parser, `GitError` (6 variants)
- `StatusPoller`: 500ms tick, focus-gated, change-only emission
- Unified diff parser: `parse_unified_diff` → `Vec<FileDiff>`
- Stage/unstage file + hunk (`git apply --cached`)
- Commit, stash push/list/apply/pop/drop, branch list/create/switch
- Worktree add/list/remove, merge with auto-stash recovery

**UI (crates/app)**
- `GitPanel`: staged / unstaged / untracked sections
- `DiffView`: loads diff per file; large-diff cap + expand()
- `CommitDialog`: conventional-commit prefix cycle + submit
- `ConfirmDialog`: type-to-confirm gate for destructive ops
- `StashPanel`: list + apply/pop/drop with confirm gate
- `WorktreePanel`: list + create + remove with confirm gate
- `StatusBar` center zone: branch + ↑ahead ↓behind + dirty count
- `WorkspaceRoot` gains `GitMount` (poller + panels + mirrored state)
- `main.rs` boots tokio runtime; opens `Repository` at cwd (best-effort)

**Tests**: 285 passed, 0 failed

---

## Phase 3 preview — CLI agent integration

Goal: run Claude Code / Codex / Aider inside an OxiMux pane, with status detection (waiting / needs-approval badge).

Key work:
- `crates/agents/` `AgentRuntime` trait implementation
- Adapter per tool: `ClaudeCodeAdapter`, `CodexAdapter`, `AiderAdapter`
- Agent status heuristics (PTY output pattern matching)
- Pane badge overlay (waiting / needs-approval / running)

Blocked on: Phase 2 dogfood gate clearing.

---

## Dogfood ledger

Journal entries live in `docs/journals/`. Each entry records: used the binary? panic? what broke?  
A day with no entry does not count toward the gate.

| Phase | Days logged | Gate cleared |
|---|---|---|
| 00 | — | no |
| 01 | — | no |
| 02 | — | no (code complete 2026-05-17) |
