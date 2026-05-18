# OxiMux — Project Changelog

Entries are newest-first. Each entry links to the commit SHA and notes what shipped.

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
