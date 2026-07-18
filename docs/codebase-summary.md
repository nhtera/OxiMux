# OxiMux — Codebase Summary

**Updated**: 2026-07-18  
**Phase**: 5 + multiplexer enhancements + UI/UX batch (settings modal, Quick Open index, lifecycle scripts, Create PR + CI checks, floating PiP terminal) + Agent Chat (round-7) shipped to main; Remote Control Phase 1-2 groundwork in progress on `feat/remote-control-headless-registry` (agent-core split + SessionRegistry, not yet wired into the view)  
**Tests**: workspace suite green; `oximux-app` lib 1258 tests (verified this session)

---

## Workspace layout

```
oximux/
├── Cargo.toml              workspace root; all crate deps declared here
├── xtask/                  CI helpers: file-size-lint, build checks
└── crates/
    ├── app/                GPUI host shell — UI composition, action routing, rendering
    │                       (modules foldered by concern: app_settings/ agent_glue/
    │                       session_restore/ platform/ loaders/ shell/terminal/)
    ├── ui/                 shared app-agnostic widgets (FloatingSurface, buttons,
    │                       confirm dialog); depends only downward — never on app
    ├── core/               domain types with zero tokio/GPUI deps
    ├── pty/                portable-pty + alacritty_terminal backend
    ├── git/                git CLI wrappers, poller, diff parser
    ├── agent-core/         oximux-agent-core — portable ThreadEvent vocabulary + stream-json
    │                       decoder + ChatThread fold, extracted from agents/ (serde/serde_json/
    │                       tracing only, no pty/rusqlite/ACP/gpui/tokio) so it cross-compiles
    │                       for a mobile Rust core; agents/ re-exports it under the original paths
    ├── agents/             AgentRuntime async trait + CliAgentAdapter + StatusMachine (Phase 3 foundation);
    │                       also SessionRegistry — gpui-free session event bus + command surface (not yet wired)
    ├── editor/             gpui-component code editor + LSP client (Phase 5 spike)
    ├── storage/            SQLite via rusqlite — Db wrapper + migrations + V001 schema + 5 typed repos (Phase 4 step 3)
    └── settings/           TOML config, theme tokens, typography
```

---

## crates/app — module map

```
src/
├── main.rs                 boots tokio runtime; opens Repository at cwd (Option);
│                           registers keybindings; opens GPUI window;
│                           installs FileHttpClient via cx.set_http_client(...)
├── lib.rs                  re-exports for integration tests
├── file_http_client.rs     FileHttpClient — file://-only gpui::HttpClient impl;
│                           required because gpui defaults to NullHttpClient (no image loads);
│                           overrides get() (http::Uri rejects file:///path empty authority);
│                           used by markdown preview to load local images
├── actions.rs              all GPUI Action structs (SplitHorizontal, Search, NewWindow,
│                           MoveTabToNewWindow, NewTabInPane, etc.)
├── assets.rs              CompositeAssets — local SVGs (git-branch) + gpui-component bundle
├── workspace_root/         WorkspaceRoot entity — top-level layout host (mod/ops/render)
│                           right_sidebar: Option<Entity<RightSidebar>>
│                           left_rail: Entity<LeftRail>; left_rail_open: bool (Cmd+B)
│                           palette: Entity<PaletteModal> (Cmd+P / Cmd+Shift+P)
│                           poll_state mirrored from RightSidebar for status bar
│                           diff_counts: HashMap<PathBuf, NumstatCounts> — per-worktree
│                             working-tree diff counts; refreshed by run_diff_refresh_round
│                             (2s periodic, focus-gated, concurrent git diff --numstat
│                             per worktree off-thread; pauses while window is unfocused)
├── window_registry.rs      WindowRegistry GPUI global — holds RegisteredWindow list (strong
│                           Entity<WorkspaceRoot> + stable persist_id per window); mints
│                           "main"/"w{n}" ids; PendingTearOff queue for cross-window tear-off;
│                           note_restored / remove helpers for app lifecycle
├── window_factory.rs       open_workspace_window (fresh window, persist_id "main" or minted)
│                           open_workspace_window_with (with optional PendingTearOff payload);
│                           registers window in WindowRegistry; last-window quit gate
└── shell/
    ├── mod.rs
    ├── agent_presentation.rs AgentVerb struct + agent_verb() — single source of truth mapping
    │                       AgentStatus + is_live flag to verb label + status-token color;
    │                       used by both the left-rail dot and the rich card line 2
    ├── top_bar.rs          40px chrome: 56px traffic-light gutter + L/R panel toggles + wordmark
    ├── left_rail/          250px workspace + nav rail (replaces old sidebar stub)
    │   ├── mod.rs          LeftRail entity; owns WorktreePanel for state; snapshots diff_counts
    │   ├── nav_section.rs  NavItem (Tasks/Automations/Agents/Search) + pure bg/fg helpers
    │   ├── workspace_row.rs WorkspaceCardPlan + build_workspace_card_plan (pure) + sum_numstat;
    │   │                   status_dot_color delegates to agent_verb for color parity
    │   ├── workspace_card.rs render_workspace_card — two-line card painter consuming WorkspaceCardPlan;
    │   │                   CARD_HEIGHT_MULT = 2.2 × h_row (design-guidelines approved exception)
    │   ├── project_group.rs renders project groups; threads diff_counts snapshot into card builder; on_drag/drag_over/on_drop wired for reorder
    │   ├── project_drag.rs drag payloads, insertion_side, paint_insertion_line (2px accent line),
    │   │                   SidebarDragPreview ghost chip, WorkspaceDragConfig, reorder_slot_value
    │   └── toolbar.rs      Add Project + settings (stubs)
    ├── agents_dashboard/   all-agents view rendered when the Agents nav item is active
    │   ├── model.rs        pure: AgentRow, attention_rank, sort_agent_rows, build_agent_rows,
    │   │                   widest_row_index — assembled from LeftRail's pushed-down snapshot
    │   ├── row_render.rs   single-row painter (project · branch · name · verb · diff)
    │   └── mod.rs          render_agents_dashboard — virtualized uniform_list + empty state;
    │                       row click → activate_workspace (cross-project focus)
    ├── command_palette/    Cmd+P / Cmd+Shift+P modal overlay (interactive: type-to-filter,
    │   │                   ↑/↓ nav, Enter/click dispatch, Esc close — mirrors project_picker)
    │   ├── mod.rs          PaletteModal entity; activate_item (dispatch + close) shared by
    │   │                   click + Enter; holds loaded custom commands; palette_filter (pure)
    │   ├── entry.rs        PALETTE_COMMANDS fn-ptr catalog + PaletteItem/PaletteItemAction
    │   │                   (Builtin fn | Custom prompt) + build_palette_items (merges customs)
    │   ├── file_index.rs   Quick Open file index (Cmd+P); async rg --files, cap 20k, ranked+
    │   │                   capped to 50; Enter opens editor tab; missing-rg hint; per-project
    │   │                   cache invalidated on project switch; replaces 3 hardcoded stubs
    │   ├── match_engine.rs pure scorer: prefix > consecutive > subsequence (no external crate)
    │   └── palette_modal.rs pure render: card + header chip + result list
    ├── settings_modal/     Cmd+, / left-rail cog — five-pane settings overlay
    │   ├── mod.rs          SettingsModal entity; pane routing; open/close
    │   ├── view.rs         top-level render: nav rail + active pane body
    │   ├── controls.rs     shared form control helpers (toggle, text field, select)
    │   ├── nav.rs          pane nav list (Terminal / Agents / Keybindings / Appearance / About)
    │   ├── pane_terminal.rs terminal settings form; writes terminal.toml via save()
    │   ├── pane_agents.rs  agent settings form; writes commit_message_ai.toml via save()
    │   ├── pane_keybindings.rs read-only keybinding display
    │   └── pane_about.rs   version + license info
    ├── floating_terminal.rs Cmd+Shift+T — in-window draggable/resizable PiP terminal card;
    │                       PTY persists across hide/show; close tears down; geometry
    │                       debounce-persisted to settings repo as JSON; NOT a second OS window
    ├── welcome_view.rs     centered empty-state card (logo + wordmark + tagline + kbd hints)
    ├── main_pane.rs        pane binary-tree; split/close/focus actions; each leaf holds
    │                       PaneContent enum (Terminal | Editor | Diff | Browser | Tasks); open_editor_in_focused_pane
    │                       replaces focused leaf content; same-path short-circuit
    ├── pane_tree.rs        pure PaneTree data structure (weight-aware)
    ├── pane_layout.rs      layout helpers
    ├── tabbed_pane.rs      TabbedPane entity: tab strip + active terminal
    ├── main_area.rs        thin dispatcher → welcome_view::view
    ├── status_bar.rs       left | center git zone | right metric strip (N TTY | N agents | N panes)
    │                       pure helpers: tty_label / agent_label / pane_label / metric_color;
    │                       git zone mounts the SCM panel's cached PrimaryAction as a one-click
    │                       smart button (click → SourceControlPanel::trigger_primary_action)
    ├── terminal_view/      TerminalView GPUI entity (mod/lifecycle/input/state/render); poll task; blink; focus;
    │                       last_completed_command_output (P8 mark bracket → snapshot
    │                       rows_text band); send-to-agent action handlers dispatch
    │                       SendTextToActiveAgent payload up to WorkspaceRoot
    ├── terminal_canvas.rs  paint_grid (bg quads → selection → shape_line → box-drawing
    │                       vector pass → cursor); group_runs replaces box-drawing chars
    │                       with space so the font glyph never paints; per-row force_width
    │                       hedge for rows mixing narrow + CJK wide
    ├── terminal_row.rs     legacy row builder; canvas path is primary
    ├── terminal_palette.rs CellColor → Hsla resolver (charcoal + xterm-256)
    ├── terminal_search.rs  find_matches / visible_match_ranges (pure)
    ├── terminal_search_state.rs SearchState struct
    ├── terminal_search_overlay.rs overlay render
    ├── terminal_context_menu.rs grid right-click menu (Copy/Paste/Select All/Clear/
    │                       link/send-to-agent/split/tab ops); WorkspaceRoot-owned entity
    │                       holding WeakEntity<TerminalView>, opened via OpenTerminalContextMenuAt
    ├── box_drawing/        U+2500–U+257F vector rendering (segments lookup +
    │                       PathBuilder stroke paint); replaces the font face for
    │                       continuous TUI borders, gap-free at cell joins;
    │                       diagonals U+2571–U+2573 fall through to the font
    ├── key_input.rs        Keystroke → PTY bytes (xterm escapes, C0, Alt-prefix)
    ├── cell_metrics.rs     character cell size constants
    ├── file_tree_view.rs   FileTreeView GPUI entity (step 4); subscribes to Entity<FileTree> from oximux-editor;
    │                       lazy expand via placeholder child (RowKind::Placeholder sentinel → real rows on Loaded event);
    │                       on_open: Arc<dyn Fn(PathBuf,…)> fires on file click; wired to WorkspaceRoot::open_file_in_active_pane (step 5);
    │                       file rows emit `FilePathDragPayload` (shell/pane_group/file_drag.rs) on .on_drag — dropped on
    │                       pane bodies via project_panes::leaf_body → ProjectPanes::open_file_in_group / split_and_open_file;
    │                       right-click dispatches OpenFileTreeContextMenuAt → FileTreeContextMenu (Open / Open to the Side /
    │                       Copy Path / Copy Relative Path / Reveal in Finder; reduced item set for directories)
    ├── file_tree_context_menu.rs Right-click overlay for Files-tab rows. WorkspaceRoot owns one shared entity (mirrors
    │                       tab_context_menu pattern). Open / Open to the Side dispatch OpenFileFromContextMenu →
    │                       routes through the same open_file_in_group / split_and_open_file as the file-drag flow.
    ├── context_env.rs      SurfaceIds struct; builds OXIMUX_* env var list for every spawned shell:
│                       OXIMUX_WORKSPACE_ID (project root path), OXIMUX_SURFACE_ID,
│                       OXIMUX_TAB_ID (minted UUIDs), OXIMUX_SOCKET_PATH; ids persisted
│                       in per-pane layout blob (serde-default; no SQL migration);
│                       restored stably and re-injected on dormant respawn
    ├── file_explorer/      FileExplorer entity; virtualized git-aware file tree (uniform_list, lazy load, git status badges)
    │   ├── mod.rs          FileExplorer entity; state machine; window-activation refresh trigger
    │   ├── tree_state.rs   flat-row build, expand toggle, should_include filter
    │   ├── status_display.rs BadgeStatus, STATUS_LABELS/COLORS, priority ladder, folder propagation
    │   ├── row_render.rs   build_row_plan pure helper → RowPlan
    │   └── fs_load.rs      async tokio read_dir; 5s timeout; symlink skip; 12-deep guard
    ├── right_sidebar/
    │   ├── mod.rs          RightSidebar entity; tab switching; hosts FileExplorer (Explorer) + SearchPanel (Search) + GitPanel+DiffView (SourceControl) + FileTreeView (Files)
    │   ├── tab.rs          RightTab enum: Explorer | Search | SourceControl | Files; icon_path() per tab
    │   │                   Files tab: always visible (no repo gate); hosts FileTreeView; SelectFilesTab action bound to Cmd+Shift+T
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
    ├── pane_group/
    │   ├── layout_presets.rs pure apply_preset(tree, Preset) → stacked / horizontal / bottom-terminal
    │   │                   reshape over PaneTree (rebuilds from leaves; content preserved)
    │   ├── mod.rs          pane group tree entity
    │   ├── sub_pane.rs     TerminalSplitTree; each split leaf is a LeafTabs tab container;
    │   │                   compact chip strip (chips + '+') renders when leaf has > 1 tab;
    │   │                   PersistedSubPane.tabs persists tab list (serde-default, backward compatible)
    │   ├── render.rs       rendering helpers
    │   ├── file_drag.rs    FilePathDragPayload for file-drop into panes
    │   ├── tab_drag.rs     tab drag payload + state
    │   └── tab_drag_zones.rs drop-zone hit detection
    ├── source_control/     Source Control panel (SourceControl tab of RightSidebar)
    │   ├── mod.rs          SourceControlPanel entity; primary-action state machine
    │   ├── primary_action.rs PrimaryAction resolver (CreatePR / Push / Sync / Commit / etc.)
    │   ├── pr_ops.rs       tokio→GPUI bridge: gh pr create --fill, open PR in browser
    │   ├── ci_status.rs    CI checks row from gh pr checks; compact ✓N ✗N ●N summary;
    │   │                   30 s throttle refresh, only while a PR is open
    │   ├── commit_area.rs  commit message input + submit
    │   ├── branch_commits.rs branch commit log panel
    │   ├── branch_picker.rs branch switch UI
    │   └── [other SCM sub-modules]
    └── worktree_panel/
        ├── mod.rs          WorktreePanel entity; refresh/submit_create/pending_remove
        └── list_render.rs  pure label/suggest-path helpers
```

**Tier-1 foldering (2026-06):** the formerly-flat top-level modules are grouped
one folder deep for traversal (each folder re-exports its submodules at the
crate root, so `crate::<name>::…` paths are unchanged):
- `app_settings/` — terminal/motion/scm_layout/keybindings/commit_message_ai/agent_launch settings (host-level; distinct from the `oximux-settings` crate)
- `agent_glue/` — agent_awake, agent_hooks_global, agent_status_hooks
- `session_restore/` — relay_cold_restore, relay_supervisor, restore_fallback, persisted_terminals, git_state_cache (several are `impl WorkspaceRoot`, so they stay in `app`)
- `platform/` — app_nap, single_instance, window_factory, window_registry, menu
- `loaders/` — custom_commands_loader, `project_scripts_loader` (reads `.oximux/scripts.toml`), browser_profiles, file_http_client
- `shell/terminal/` — the ~18 terminal-surface modules (terminal_view/canvas/row/links/palette/scrollbar/search*/context_menu/key_input/mouse_report/cell_metrics/box_drawing/adapter_picker/floating_terminal*)

At `crates/app/src/` root: `lib.rs`, `main.rs`, `actions.rs`,
`state.rs`, `assets.rs`, `workspace_root/` (split into mod/ops/render),
`left_rail_layout.rs`, `project_panes_factory.rs`, plus the `keymap_registry/`
and `notifier/` folders.

---

## crates/ui — shared widgets (oximux-ui)

App-agnostic widget layer, extracted from `app/src/ui/` in 2026-06. Depends only
downward (`gpui`, `gpui-component`, `oximux-settings`) and **never** on
`oximux-app`; the host re-exports it as `crate::ui` (`pub use oximux_ui as ui`).

```
src/
├── lib.rs              pub mod buttons/overlay/confirm_dialog; re-exports danger_ghost + FloatingSurface
├── overlay.rs          FloatingSurface — overlay/surface chrome recipe (.floating_chrome())
├── buttons.rs          button variant wrappers (danger_ghost, …)
└── confirm_dialog/     ConfirmDialog modal (prompt + callback; pure is_match in logic.rs)
```

Generic dialogs that **stay in `app`** because they reach host state: `toast`
(uses `crate::motion_settings::active`) and `divider` (uses
`crate::shell::pane_tree::Axis`) — moving either would create a forbidden
`oximux-ui → oximux-app` edge.

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
├── merge.rs        merge with auto-stash recovery; MergeOutcome
└── gh.rs           GhCmd wrapper for gh CLI: available / is_github_remote / has_open_pr /
                    pr_create (--fill, opens browser) / pr_checks / CheckRun;
                    serde+serde_json used for CheckRun deserialization
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
│                       AgentStatusStream = watch::Receiver<AgentSnapshot>  (AgentSnapshot {status, detail: Option<SidebandDetail>})
│                       Methods: start_session / send_message / cancel / subscribe_status / current_status (still returns bare AgentStatus)
│                       cancel() doc: SIGTERM-grace dance deferred to step 13; currently SIGKILL
├── runtime_impl.rs     CliRuntime — first concrete AgentRuntime impl
│                       Adapter registry: HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>
│                       Per-session state: own PortablePtyBackend behind Arc<Mutex<Box<dyn TerminalBackend>>>
│                         + tokio 50ms poll task: poll_helpers::process_poll_events runs the
│                           AgentOscScanner (osc_sideband.rs, OSC-9999 status sideband) before the
│                           regex StatusMachine on OSC-stripped bytes, then drains into it
│                         + watch::channel<AgentSnapshot> (multi-subscriber fan-out to badge / sidebar / dashboard)
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

### crates/agents/src/thread/ — structured chat protocol layer (separate from the raw-PTY `cli/` runtime above)

Backs the **Agent Chat** view (`crates/app/src/shell/agent_chat/`): three provider adapters (Claude, Codex, ACP) each decode their own wire protocol into one `ThreadEvent` vocabulary. Full adapter-coverage matrix in `docs/system-architecture.md` → "Agent Chat adapters".

**Agent-core split (2026-07-18):** the pure fold + wire vocabulary + stream-json decoder (`event.rs`, `state.rs`, `entry.rs`, `tool_call.rs`, `question.rs`, `background_task.rs`, `tool_detail.rs`, `turn_diff.rs`, `context_chip.rs`, `stream_json.rs`) now live in `crates/agent-core` (`oximux-agent-core`), a dependency-minimal crate (serde/serde_json/tracing only) so the same `ChatThread` fold can cross-compile for a mobile Rust core. `crates/agents/src/thread/mod.rs` re-exports every module under its original `crate::thread::*` path, so all downstream import sites are unchanged. Provider adapters (`connection.rs`, `claude_stream_json.rs`, `codex/`, `acp/`, `connect.rs`) and the codex/pi import fixtures stay in `oximux-agents` — they need pty/ACP/tokio.

| File | Role |
|---|---|
| `event.rs` (agent-core) | `ThreadEvent` — transport-agnostic event enum all three adapters emit |
| `state.rs` (agent-core) | `ChatThread::apply` folds `ThreadEvent`s into `Vec<ThreadEntry>` the view renders; also tracks `last_summary` (Claude `post_turn_summary`, cleared each new turn) and `last_known_context_window` (stashed from settled `TurnUsage` + `LiveUsage`, feeds the context meter) — both in-memory only, not yet in `PersistedChatTranscript` (round-9 gap: a restored chat loses the context-bar denominator until its next turn settles) |
| `connection.rs` | `AgentConnection` trait (now `Sync`-supertrait, so a handle can hold it as a shared `Arc` across threads) + `AgentCapabilities` (incl. `supports_rewind`, `rewind_is_server_side()`) |
| `stream_json.rs` (agent-core) | Claude `stream-json` hand-parsed decoder; decodes `input_json_delta` fragments (live tool-input streaming) onto the tool card opened by `content_block_start`, ahead of the finalized `tool_use` block; routes `ExitPlanMode` requests (tagged `PermissionKind::Plan`); promotes `system/status` `compacting` → `CompactionStarted` (the in-progress compaction indicator) |
| `claude_stream_json.rs` | Claude connection; `set_mode` writes a `set_permission_mode` control_request on stdin (the Agent SDK's wire) so a composer mode change applies in place — no `--resume` respawn |
| `tool_call.rs` | `ToolCall` (incl. `subagent_log` — the buffered child-agent action log shown in the parent card) + `PermissionKind` enum (`Tool` / `Plan` / `Mode` / `Mcp` / `Other`) tagging every permission request for the card router |
| `codex_session_import.rs` | `import_codex_rollout` — reads a finished `~/.codex` rollout (Responses-API format) into a `ThreadEntry` transcript so a Codex session reopens as a chat (parallels Claude's on-disk log import) |
| `codex/protocol.rs` | Codex `app-server` JSON-RPC v2 message builders; `thread/fork` (server-side rewind, `lastTurnId`-addressed) — `thread/rollback` is the deprecated upstream alternative, not used; `account/read` + `account/login/start` (ChatGPT-OAuth sign-in) |
| `codex/mod.rs` | Codex connection; `AgentCapabilities::supports_rewind = true`; `pending_elicitations` map for MCP `mcpServer/elicitation/request` (distinct `{action}` reply shape from tool approvals); `codex_approval_policy` / `codex_sandbox` `FeatureControl` selects applied per-turn via `turn/start` overrides, persisted on the session struct; browser-OAuth sign-in via `account/login/start` (fire-and-forget worker emits `AuthUrl`) |
| `codex/map.rs` | Notification → `ThreadEvent` mapper; in-session turn ledger maps user-message ordinals to turn ids for rewind addressing; collab child-thread activity buffered + replayed into the parent tool card; unauthorized turn error → `AuthRequired` |
| `codex/approvals.rs` | Approval + elicitation decision encoding (`to_codex_elicitation`) |
| `acp/` | `agent-client-protocol` 1.2 adapter — generic tail for Cursor/Amp/other ACP agents |

**`crates/agents/src/session_registry.rs` (2026-07-18, groundwork, not yet wired into the view):** a process-wide, gpui-free `SessionRegistry` mapping `session_id → SessionHandle` — the event bus + command surface a remote (network) layer will subscribe to and command off the GPUI thread, built for the Remote Control plan (`plans/260717-2037-oximux-remote-control/`). Each `SessionHandle` holds the shared `Arc<dyn AgentConnection>`, a seq-indexed bounded backlog replayable via `events_since` (reconnect gap-fill; the live `broadcast` channel alone can't replay past a lagging receiver), and an atomic idempotent-resolve gate so one `request_id` is decided through exactly one path. seq-assignment, backlog append, and broadcast happen under one lock so producers can't reorder the backlog. This is what forced `AgentConnection` to gain the `Sync` supertrait and the agent-chat view to switch from `Box<dyn AgentConnection>` to `Arc<dyn AgentConnection>` (`crates/app/src/shell/agent_chat/mod.rs`) — pure ownership change, no behavior change, so the registry can share the same connection the view drives.

`crates/app/src/shell/agent_chat/` (GPUI views, not yet foldered into the Tier-1 map above): `plan_approval_card.rs` (Claude `ExitPlanMode` 3-way approval card), `tool_card.rs` (per-kind tool card renderer; `⤢` expand affordance on substantial cards), `tool_bodies.rs` (per-kind body renderers with a `full: bool` size-mode — inline caps vs lifted sheet caps), `tool_sheet.rs` (fullscreen tool-payload overlay: virtualized-diff `uniform_list` or capped-lifted text body, Copy + Esc/backdrop/✕ dismiss, reads the tool call live by id), `rewind_menu.rs` (shared rewind/fork UI; branches on `rewind_is_server_side()` for Claude disk-fork vs Codex connection-fork; "Fork from here" to a new tab reads Claude's on-disk `~/.claude` session log and is hidden for Codex, which has no equivalent log), `apply_patch.rs` (Codex `apply_patch` tool call's `changes` array → the shared `DiffLine` stream `diff_card.rs` renders, so a Codex edit shows the same colored diff card as Claude/ACP instead of raw JSON; diff rows are syntax-highlighted and memoized per `(path, line)` since the transcript is not virtualized and stream-delta batching repaints at ~20Hz), `session_detail.rs` (read-only popover of what a session advertised at `system/init` — model, cwd, tools, MCP servers + status, subagents — cached with the transcript via `SessionMeta` so a restored chat answers before its resumed process speaks; hidden entirely when the backend advertises nothing, which is Codex and ACP today). Attention notifications live in `crates/app/src/notifier/` (macOS `UNUserNotificationCenter` banners + dock badge, per-tab coalesced, focus-cleared, per-event toggles in Settings → Notifications); a real codesigning identity (`scripts/bundle-macos.sh --sign` / `OXIMUX_CODESIGN_IDENTITY`) is required for the OS to honor the notification-authorization grant — the default ad-hoc-signed dev bundle silently drops it. **Worktree-per-agent, first-class `Workspace`** (`roster.rs` + `workspace_root/render.rs`): the New Agent composer's worktree pill + slug input stages the send, then routes up rather than creating a git-only worktree — the leaf carries no `WorkspaceRepo` (Orca thin-leaf). `roster.rs` emits `AgentChatEvent::WorktreeWorkspaceRequested{slug}`, `pane_group/tabs.rs` dispatches the `CreateWorktreeWorkspaceForActiveChat{slug}` action, and `workspace_root/render.rs`'s handler resolves the active chat view (`active_agent_chat_view` on `pane_group/state.rs` + `project_panes/state.rs`, captured synchronously before the async step), runs the existing `workspace_ops::create_workspace_with_rollback` (git worktree **+** DB `Workspace` row insert, full rollback on failure), calls `mark_rail_dirty`, then hands the outcome back through `AgentChatView::on_worktree_create_outcome` (rebinds `cwd`, resumes the staged send). The worktree agent now gets a sidebar workspace card + `⌘J` entry and is removable via the Worktree panel; inline failure banner still offers Retry or "continue without a worktree"; hidden for non-git projects. The git-only `create_agent_chat_worktree` helper was retired.

**Composer control placement** (`composer.rs`): the draft's controls split by how often they change. Session **context** — the worktree pill (`render_worktree_picker`) and Import session — sits in `render_context_row` **above** the input; session **behavior** — attach, permission mode, model, effort, feature controls — sits in the toolbar **below**. Context is bound at first send and immutable after; behavior is retuned mid-session. Both rows live inside the composer's centered `max_w(CONTENT_MAX_W)` column, which is what keeps them aligned with the transcript at any window width. The worktree pill drives a `Popover` directly (not `render_dropdown_shell`, whose `PopupMenu` rows cannot host the slug's text field) and emits `ComposerEvent::WorktreeIsolationPicked(bool)` — the desired state, not a flip. **`ComposerView` keeps its own `unbound` flag**, pushed down by the parent: `sync_unbound_composer` owns the draft shape, and `sync_composer`'s bound branch must explicitly clear anything draft-only (agent picker, worktree pill) or it lingers against a live session.

`crates/app/src/shell/session_history/` — the `⌘⇧H` session-history / import modal (centered overlay, sibling of `command_palette`). `mod.rs` is the thin view (chip row, list, preview, keys); `picker.rs` holds the pure, unit-tested logic: `session_row_*` labels, the fuzzy filter, the `AgentTypeFilter` segment (`All | Claude | Codex | Copilot | OpenCode | Pi`, cycled by `Tab`) + `filter_sessions_typed` (type gate ∘ query), and `entry_slug` (the row's registry slug for icon + resume routing). Default import (`↵` / click) is surface-aware via `AgentLaunchSettings::opens_as_chat(id)` — the single routing gate shared with the new-agent launcher (`workspace_root`): chat tab when the adapter's resolved open mode is `Chat` + chat-capable, else terminal resume. `⇧↵` forks, `⌘↵` force-opens as chat — including OpenCode/Pi rows, which open as a transcript-only import-bridge tab (round-7; see below) rather than a live chat. The index (`oximux-agents` `session_log/session_index.rs`) sources Claude from `~/.claude/projects/**/*.jsonl` and Codex by scanning the CLI rollout store `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (the same files `codex resume` lists) — each rollout's `session_meta` head yields id/cwd/git-branch/start-time and its first non-injected user turn the title; both rollout shapes (newer `response_item`+`session_id`, older top-level `message`+`id`) are handled. Three more providers are indexed by `session_log/import_provider_index.rs` directly from their own stores: **OpenCode** (`~/.local/share/opencode/opencode.db` `session` table), **Copilot** (`~/.copilot/session-store.db` `sessions`+`turns`), and **Pi** (`~/.pi/agent/sessions/**/*.jsonl`). These ride `AgentAdapter::Custom` + a `SessionEntry.preset_id` slug (not new core adapter variants); resume spawns a `Custom` PTY via `oximux-settings::import_resume_command` (`opencode --session <id>` / `copilot --resume=<id>` / `pi --session <file>`). Every source scopes by recorded cwd, degrades to "absent" on a missing/foreign store, and SQLite is opened read-only. The preview pane (`session_preview.rs`) renders a short blurb for all five providers via `load_import_provider_preview` (`opencode_preview`/`copilot_preview`/`pi_preview` in `import_provider_index.rs`, alongside Claude/Codex's own preview paths).

**Round-6 full-transcript mappers, wired to open-as-chat in round-7** (`session_log/import_transcript_opencode.rs`, `session_log/import_transcript_pi.rs`, dispatched by `load_import_provider_transcript` in `import_provider_index.rs`): map OpenCode's SQLite `message`/`part` rows and Pi's JSONL `message` lines into the same `Vec<ThreadEntry>` shape the Claude/Codex chat importers build. Consecutive text parts fold into one bubble; reasoning/thinking folds into the assistant entry's `thinking` field; unrecognized parts degrade to a plain notice row (never raw JSON). `⌘↵` on an OpenCode/Pi row now opens a chat tab seeded from this mapper — `pane_group/tabs.rs`'s `open_import_bridge_chat` builds an `AgentChatView::new_import_bridge` (`connect_now:false`, no live connection) and swaps the composer for a "Resume in terminal" footer (`render_import_bridge_footer`) that re-dispatches the existing `ResumeAgentSession` PTY resume via `import_resume_command`; default `↵`/click is unchanged (still resumes in a terminal). Bridge tabs are excluded from tab persistence (no live session id to restore against) and re-open from Session History, deduped on `(preset_id, session_id)`. Copilot's `session-store.db` `turns` table was confirmed to hold readable transcript text too, but Copilot has no chat surface in OxiMux to seed, so it stays resume-only with no transcript mapper.

---

## crates/dictation — offline voice dictation

Local speech-to-text for ANY focused pane — chat composer, terminal, or code
editor (⌘E). No GPUI dependency (mirrors `crates/pty`): the app consumes a
channel-based handle and never sees a cpal or sherpa type. Vietnamese-first —
Whisper is the default; a dedicated Vietnamese zipformer + Parakeet (English) +
SenseVoice (CJK) round out the catalog.

| File | Role |
|---|---|
| `capture.rs` | cpal CoreAudio capture on a dedicated thread (owns the `!Send` stream). Selectable input device (`run(device)`, `list_input_devices()`; cpal 0.18 → device name is its `Display`). Device-default rate (never request 16 kHz — macOS silence pitfall), downmix to mono, shared `SessionBuffer`, throttled RMS level events, 2-min hard cap. |
| `resample.rs` | Linear any-rate → 16 kHz mono resampler + downmix. Every buffer passes through before the engine (sherpa aborts on inconsistent rates). |
| `engine.rs` | sherpa-rs wrapper: `WhisperRecognizer` / `TransducerRecognizer` (NeMo Parakeet) / `ZipFormer` (icefall, incl. Vietnamese) / `SenseVoiceRecognizer` (single-file, CJK). CPU-only. Silence gate (int16 peak < 300), 30 s chunk splitter at the quietest 100 ms window (SIGTRAP #7925 mitigation). `ModelPaths.model` = single-file slot; `require_files` is kind-aware. |
| `vad.rs` | Silero VAD (sherpa's bundled `silero_vad`, no new dep): `keep_speech` segments a 16 kHz buffer into speech spans and drops silence before decode (also kills whisper's silence hallucination). `ensure_downloaded` fetches the ~629 KB `silero_vad.onnx` on demand (atomic temp+rename, cleaned on failure); missing/offline → caller falls back to the peak gate. |
| `custom_words.rs` | Pure fuzzy dictionary correction: `apply(transcript, &[word], threshold)` over 1–3-word windows, self-contained Levenshtein + length prefilter + exact-key shortcut, case/punct preserved. **No soundex** (coarse phonetic collisions over-corrected common words). |
| `text_filter.rs` | Pure transcript cleanup: `filter(text, language, enabled)` — language-gated filler removal (en/vi/auto; unknown langs remove none), bracketed/music-note strip, 3+-repeat stutter collapse, whole-output-only whisper hallucination guard. |
| `controller.rs` | Session state machine (Idle→Recording→Transcribing) on one worker thread + a warm **engine** cache (the VAD is NOT cached — sherpa-rs 0.6.8 has no full reset, so a fresh detector is built per recording). Idle teardown is the caller's `model_unload_timeout` policy, passed per session: `start(paths, device, vad_enabled, unload)` where `None` = never unload and `Some(ZERO)` = unload right after the decode; with nothing warm (or "never") the worker blocks on `recv()` instead of polling. `run_session` trims with `maybe_vad_trim` (best-effort) before the silence gate. Emits `DictationEvent` over a `futures::mpsc`. `Drop` only signals — never joins (no main-thread block). |
| `feedback.rs` | Optional start/stop capture cues (`audio_feedback_enabled`). Plays stock macOS system sounds (`Tink`/`Pop`) via `afplay` — deliberately **no audio-playback dep and no bundled assets**. Fire-and-forget (never blocks the GPUI main thread) but reaps its child in a short-lived thread so no zombies accumulate; no-op off macOS. |
| `model_catalog.rs` | 10-model catalog across 4 families (`Whisper`/`Transducer`/`Zipformer`/`SenseVoice`): whisper small(default)/turbo/large-v3/base/tiny, zipformer-vi + 30M lite, parakeet v3/v2, sense-voice. URLs HEAD/range-verified against k2-fsa `asr-models`; sha256 unpinned (`None`). `recommended_model_id(language)` drives the Voice pane's "recommended" badge: a pinned `vi`→`zipformer-vi` (measured 10.2% WER vs whisper-small's 31.7%), `en`→parakeet v2, else the multilingual `DEFAULT_MODEL_ID` (`auto` implies code-switching, so no single-language model is suggested). |
| `model_manager.rs` | Download (resumable HTTP Range) → optional SHA-256 verify → `tar` extract → file-existence gate → Ready. Disk is source of truth on init. Pull (`status`/`readiness`) + push (`ModelEvent`) APIs. (VAD's single-file `.onnx` uses `vad.rs`'s own fetch, not this archive path.) |

App glue: `shell/agent_chat/dictation_service.rs` (process-wide `Global`: one
`DictationController` + `ModelManager`; routes events to a `DictationTarget`
enum — the recording composer, or the global HUD for terminal/editor panes —
via `WeakEntity`; shared `prepare_start` pre-flight), `dictation_hud.rs` (the
floating "Listening…" pill + terminal/editor sink, mounted at `WorkspaceRoot`),
`dictation_ui.rs` (UI state + `WaveformBuffer` ring + smart-space helper),
`dictation_waveform.rs` (shared waveform renderer), composer recording bar +
mic device/hold dropdown, `WorkspaceRoot::dictate_focused_pane` (⌘E → focused
pane → sink), `terminal_view/input.rs::insert_dictation_text`,
`platform/mic_permission.rs` (`AVCaptureDevice` TCC), `settings_modal/pane_voice.rs`
+ `settings/dictation.rs` (`DictationSettings{input_device, mode, vad_enabled,
custom_words, word_correction_threshold, filler_filter_enabled}` →
`dictation.toml`; full-Whisper language table in `settings/dictation_languages.rs`).
`dictation_service::route_event` post-processes the Final transcript once
(`text_filter::filter` → `custom_words::apply`) at the shared choke point so
history + every target pane get the cleaned text. The Voice pane's custom-words
editor is an `InputState` on the modal (persists on blur/Enter, not per-key).

---

## crates/pty — terminal backend

| File | Role |
|---|---|
| `backend.rs` | `TerminalBackend` trait: spawn/write/resize/snapshot/search_grid; dormant lifecycle (`spawn_dormant` / `promote_to_live` / `prefill_grid`); display-only `write_output(id, bytes)` streams external producer bytes into a dormant session's grid without a PTY child |
| `portable_pty_backend.rs` | PTY via portable-pty; bounded sync_channel(256); watcher thread per session; `write_output` override accepts dormant sessions only (refuses live to avoid racing the watcher on the parser mutex) |
| `state.rs` | `TerminalState`: alacritty_terminal + vte processor; user-tunable scrollback (default 5000) |
| `snapshot.rs` | `TerminalSnapshot`: cells grid + cursor; grid-text extractors `rows_text` / `get_content` / `last_n_non_empty_lines`; `abs_line_to_screen_row` maps OSC 133/633 mark lines into the visible viewport |
| `events.rs` | `TerminalEvent` enum: Output/Exit/Resize/TitleChange/Bell/CwdChanged/CommandMark/Progress/Clipboard/PtyReply |
| `osc7.rs` | `OscScanner`: OSC 7 (cwd) + OSC 133/633 (command marks) + OSC 9;4 (progress) sequence extractor — events alacritty doesn't expose |
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
| `migrations/V005__per_window_persistence.sql` | Adds `window_id TEXT NOT NULL DEFAULT 'main'` to `pane_buffers` and `pane_relay_ids`; rebuilds PKs to include `window_id`; backward compatible with existing rows |
| `migrations/V014__project_sort_order.sql` | Adds `sort_order REAL` to `projects`; backfilled at `db::open` from display order |
| `migrations/V015__workspace_sort_order.sql` | Adds `sort_order REAL` to `workspaces`; same backfill pattern. Migration ladder now at 15. |

`r2d2` deliberately **not** adopted in v1 — single `Arc<Mutex<Connection>>` over WAL is sufficient for the single-writer model. Read-pool split (write conn + N readers) is the documented upgrade path if profiling shows contention.

---

## crates/editor — code editor + LSP (Phase 5)

> Step 1 spike (go): `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`  
> Step 2 (go): `plans/reports/tester-260522-1939-phase-05-step02-editor-save.md` — smoke green, 8/10 code review

```
src/
├── lib.rs                  re-exports EditorView, SaveFile action, lsp module
├── editor_view.rs          EditorView GPUI entity (step 1+2 + markdown preview)
│                           Fields: file_path, uri (lsp_types::Uri, parse-once),
│                             state (Entity<InputState>), focus_handle,
│                             lsp_client (Option<Arc<LspClient>>),
│                             dirty (bool), doc_version (i32, starts at 1),
│                             last_sent_text (String), _observe_sub,
│                             is_markdown (bool), md_mode (MarkdownViewMode)
│                           Step 1: gpui-component Input in code_editor("rust") mode;
│                             attach_lsp → HoverProvider + publishDiagnostics pump
│                           Step 2: cx.observe wired in new(); SaveFile action
│                             (declared here for crate-cycle reasons); on_save writes
│                             file via fs::write, sends didSave; window title shows
│                             " •" dirty badge; impl Drop sends didClose via sync mpsc
│                           Keyed by --editor-spike CLI flag
│                           Markdown: is_markdown_path() detects .md/.markdown (not .mdx);
│                             header toggle renders Source/Preview/Split segmented buttons;
│                             body dispatches on md_mode (Source=Input / Preview / Split)
├── markdown_preview.rs     MarkdownViewMode enum (Source | Preview | Split);
│                           mode_toggle() — segmented button row (right-aligned, markdown-only);
│                           render_preview() — gpui-component text::markdown GFM renderer;
│                           absolutize_image_paths(text, file_path) — pure fn: rewrites
│                             repo-relative ![](path) → file:// URIs for local image loads
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
| > 1500 non-blank LOC | warn |
| > 3000 non-blank LOC | fail (unless allowlisted) |

Thresholds were raised to GPUI reality in 2026-06 (large render/impl files are
idiomatic). A ratchet allowlist (`xtask/file-size-allow.txt`) grandfathers the
handful of files still over the hard cap at their recorded LOC: an allowlisted
file may shrink freely but fails the moment it grows past its budget, so the
debt can only go down (a `STALE` notice nudges dropping a row once its file
falls under the cap). Applies to all `.rs` files in `crates/`.
