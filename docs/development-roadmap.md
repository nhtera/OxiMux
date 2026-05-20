# OxiMux — Development Roadmap

**Updated**: 2026-05-20  
**Gate rule**: each phase ships only after ≥7 consecutive daily-driver days with zero panics (ADR-003). Tests-passing alone is not sufficient.

---

## Phase table

| Phase | Capability | Status |
|---|---|---|
| 00 | Foundation: workspace, GPUI shell, CI guards | scaffold complete; dogfood pending |
| 01 | Terminal cockpit: multi-pane PTY, tabs, search, render perf | code complete; dogfood pending |
| **02** | **Git core: status, diff, stage, commit, stash, worktree UI** | **code complete (steps 1-14); step 15 = dogfood gate** |
| **03** | **CLI agent integration: Claude Code, Codex, Aider adapters** | **in progress (steps 1-4 + CliRuntime done; steps 5-14 pending)** |
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

- 2026-05-17: Right-sidebar Phase 01 shipped — RightSidebar entity replaces GitMount; 3-tab activity bar (Explorer/Search/SourceControl); 6 tests added.
- 2026-05-19: Right-sidebar Phase 02 shipped — FileExplorer entity on Explorer tab; virtualized uniform_list tree; lazy dir load with 5s timeout; git status badges M/A/D/R/U/C with folder propagation; focus-regain refresh; 90+ tests added (439 workspace total).

---

## Phase 3 — CLI agent integration (in progress)

Goal: run agent CLIs inside an OxiMux pane, with status detection (waiting / needs-approval badge).

**Steps 1-3 shipped 2026-05-20 (foundation):**
- `AgentRuntime` async trait + `AgentSessionConfig` + `watch::Receiver` fan-out stream
- `CliAgentAdapter` async trait + `CommandSpec` + `StatusPattern` (regex::bytes)
- `StatusMachine`: ring-buffer pattern scan, 5s idle decay, exit/force transitions; 18 tests

**Step 4 + CliRuntime shipped 2026-05-20:**
- `CustomCommandAdapter` — escape-hatch adapter; reads `custom_command` from config; empty status patterns fall through to StatusMachine defaults
- `CliRuntime` — first concrete `AgentRuntime`; per-session PTY + 50ms poll task + `watch::channel`; adapter registry keyed by `AgentAdapter` enum (now `Hash`-derived)
- 16 new tests (7 custom adapter + 9 runtime_impl); 510 workspace total

**Remaining steps (5-14):**
- Steps 5-7: concrete adapters (`ClaudeCodeAdapter`, `CodexAdapter`, `AiderAdapter`)
- Step 8: detection registry (`which $bin` scan at startup)
- Steps 9-12: GPUI integration (pane badge overlay, session lifecycle, launch dialog)
- Steps 13-14: process hygiene (SIGTERM-grace-SIGKILL), config wiring (`AdapterConfig` enum)

Blocked on: Phase 2 dogfood gate clearing (runtime steps can proceed in parallel).

---

## Dogfood ledger

Journal entries live in `docs/journals/`. Each entry records: used the binary? panic? what broke?  
A day with no entry does not count toward the gate.

| Phase | Days logged | Gate cleared |
|---|---|---|
| 00 | — | no |
| 01 | — | no |
| 02 | — | no (code complete 2026-05-17) |
