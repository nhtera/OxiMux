# OxiMux — Development Roadmap

**Updated**: 2026-06-06  
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
| 05 | Editor + LSP: gpui-component code editor, file tree, rust-analyzer | in progress — steps 1-5 done (spike + save round-trip + file tree backend + file tree UI + pane-as-editor-host); step 6+ next |
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

## Multiplexer enhancements plan — code complete 2026-05-29

Plan: `plans/260528-2153-terminal-multiplexer-enhancements/`  
All four phases code complete; runtime GPUI smoke pending.

| Phase | Capability | Status |
|---|---|---|
| mux-P1 | Agent attention & notifications (bell → attention ring, tab chip) | code complete |
| mux-P2 | Multi-client attach & reconnect hardening | code complete |
| mux-P3 | Multi-window & tear-off tab | code complete (runtime smoke pending) |
| mux-P4 | Per-pane tabs & context env | code complete (runtime smoke pending) |

**mux-P3 (Phase 3) shipped:**
- `WindowRegistry` GPUI global + stable persist ids (`"main"` / `"w{n}"`)
- `open_workspace_window` / `open_workspace_window_with` factory (`window_factory.rs`)
- `NewWindow` action (Cmd+N); last-window-close quits, non-last dismisses
- Storage migration V005: `window_id` column added to `pane_buffers` + `pane_relay_ids` (backward compatible `DEFAULT 'main'`)
- Boot reopens every persisted window; `capture_session` writes open-windows manifest
- `MoveTabToNewWindow` tear-off: relay detach + `attach_pty_existing` on destination; PTY stays alive

**mux-P4 (Phase 4) shipped:**
- `LeafTabs` per-leaf tab container in `TerminalSplitTree`; compact chip strip renders when leaf has > 1 tab
- `NewTabInPane` action (Cmd+Shift+T); Cmd+W cascades per-pane tab → leaf → group
- `SurfaceIds` context env (`context_env.rs`): `OXIMUX_WORKSPACE_ID`, `OXIMUX_SURFACE_ID`, `OXIMUX_TAB_ID`, `OXIMUX_SOCKET_PATH` injected into every spawned shell
- Per-pane-tab relay reattach + agent CLI context-env threading deferred

---

## Terminal emulator richness plan — code complete 2026-05-30

Plan: `plans/260529-2042-terminal-emulator-richness/`  
Closes the emulator-quality gap with the three GPUI reference terminals (all share OxiMux's `alacritty_terminal` + `portable-pty` backend). 11 of 12 phases fully complete; P12 ships slices 1+1.5 and defers 2+3.

| Phase | Capability | Status |
|---|---|---|
| trm-P1 | SGR text attributes (bold/italic/underline/strikethrough/dim) | code complete |
| trm-P2 | Mouse selection + gestures (double/triple/Shift-extend) | code complete |
| trm-P3 | Live scrollback scrolling (display_offset, snap-to-bottom) | code complete |
| trm-P4 | Input encoder (app-cursor/keypad, xterm modifiers, Alt-prefix) | code complete |
| trm-P5 | Mouse reporting to TUIs (SGR/UTF-8/X10) | code complete |
| trm-P6 | Cursor shapes + visibility (DECSCUSR, blink, focus dim) | code complete |
| trm-P7 | Hyperlinks + path-to-editor open (OSC 8 + plain-text detection) | code complete |
| trm-P8 | Shell integration + OSC extensions (7/52/133/633/9;4 + DSR replies) | code complete |
| trm-P9 | Terminal settings surface (terminal.toml + live-reload watcher) | code complete |
| trm-P10 | CJK wide-character layout (wide + wide_spacer + per-row force_width) | code complete |
| trm-P11 | Box-drawing vector rendering (PathBuilder strokes, gap-free merges) | code complete |
| trm-P12 | Inline agent terminal (extraction + send-to-agent + display-only backend) | partial (slices 1+1.5+2; slice 3 deferred) |

**Keybinds added**: Cmd+Shift+I sends terminal selection to active agent's input buffer; Cmd+Shift+O sends last completed command's output (bracketed by P8 marks).

**Display-only backend seam (slice 2)**: `TerminalBackend::write_output(id, bytes)` lets an external producer stream bytes into a `spawn_dormant` session's grid without a PTY child. Live sessions are rejected to avoid racing the watcher thread on the parser mutex. No app-layer consumer wired yet — slice 3 (`block_below_cursor`) is the natural next consumer.

---

## UI/UX batch — shipped to main 2026-06-06

Commits: `cdcbe65` `0817caa` `778160a` `e5dc89f` `d0c7ba1` `a2a6c95` `326464a`

| Feature | Keybind / entry point | Status |
|---|---|---|
| Settings panel modal (Terminal / Agents / Keybindings / Appearance / About) | `Cmd+,` / left-rail cog | shipped |
| Quick Open file index (`rg --files`, ranked top-50) | `Cmd+P` | shipped |
| Per-repo lifecycle scripts (setup / run / cleanup + auto_setup) | `.oximux/scripts.toml` / left-rail "…" menu | shipped |
| One-click Create PR + CI checks row (`gh` CLI) | SCM panel primary-action + status row | shipped |
| Floating PiP terminal (draggable/resizable, in-window, PTY-persistent) | `Cmd+Shift+T` | shipped |

---

## Dogfood ledger

Journal entries live in `docs/journals/`. Each entry records: used the binary? panic? what broke?  
A day with no entry does not count toward the gate.

| Phase | Days logged | Gate cleared |
|---|---|---|
| 00 | — | no |
| 01 | — | no |
| 02 | — | no (code complete 2026-05-17) |
