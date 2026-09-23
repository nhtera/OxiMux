# OxiMux — System Architecture

**Updated**: 2026-08-05  
**Phase**: 5 + multiplexer enhancements + UI/UX batch (settings modal, Quick Open, lifecycle scripts, Create PR + CI, floating PiP terminal, markdown preview) shipped; external-CLI auto-provisioning (bundled ripgrep + one-click verified `cua-driver` install) shipped; desktop auto-update shipped; `oximux` CLI + `oximux serve` (headless host) shipped

---

## Layer overview

```
┌─────────────────────────────────────────────────────┐
│  GPUI UI layer  (apps/desktop)                        │
│  WindowRegistry (global) — one WorkspaceRoot / window│
│  WorkspaceRoot → MainPane (grid of pane leaves)     │
│                   each leaf: LeafTabs (per-pane tabs)│
│                     each tab: PaneContent::Terminal  │
│                     | Editor | Diff | Browser | Tasks│
│                → RightSidebar (tab-switched panel)  │
│                   Explorer tab: FileExplorer         │
│                   Files tab:    FileTreeView         │
│                   Search tab:   SearchPanel          │
│                   SourceControl tab: GitPanel +      │
│                     DiffView + pr_ops + ci_status    │
│                → StatusBar (git zone + smart button) │
│  Overlays (mounted last-child in WorkspaceRoot)      │
│    PaletteModal   — Cmd+P / Cmd+Shift+P              │
│    SettingsModal  — Cmd+,  / left-rail cog           │
│    FloatingTerminal — Cmd+Shift+T (PiP, draggable)  │
├─────────────────────────────────────────────────────┤
│  Shared widget layer  (crates/ui — oximux-ui)       │
│  FloatingSurface overlay chrome, button variants,   │
│  ConfirmDialog. App-agnostic; app depends on ui,    │
│  ui never on app. Host re-exports as crate::ui.     │
├─────────────────────────────────────────────────────┤
│  Domain / backend layer                             │
│  crates/pty    — TerminalBackend + PortablePtyBackend│
│  crates/git    — Repository, StatusPoller, git ops, │
│                  GhCmd (gh CLI wrapper)              │
│  crates/agent-core — portable ThreadEvent vocabulary │
│                  + stream-json decoder + ChatThread  │
│                  fold (serde-only, mobile-portable)  │
│  crates/agents — AgentRuntime trait + CliRuntime    │
│                  CliAgentAdapter + StatusMachine     │
│                  SessionRegistry (event bus, WIP)    │
│  crates/remote-proto — Remote Control wire vocab:   │
│                  postcard envelope, HostEvent,       │
│                  PairingTicket, Transport (WIP)      │
│  crates/settings — terminal.toml, commit_message_ai.toml,│
│                    ProjectScripts (.oximux/scripts.toml)  │
│  crates/core   — shared domain types (no deps)      │
├─────────────────────────────────────────────────────┤
│  Async runtime                                      │
│  tokio multi-thread (booted in main.rs before GPUI) │
└─────────────────────────────────────────────────────┘
```

---

## Source map — where things live

Folder-level index (coarse by design — it rots slowly). Logic lives in the
backend crates (`core`/`git`/`agents`/`pty`/…); GPUI views live in
`apps/desktop/src/shell/<domain>/`. To find a feature, grep the domain column.

### Crates (`crates/<dir>/` → package → entry)

| Crate dir | Package | Entry | Purpose |
|---|---|---|---|
| `core` | `oximux-core` | `src/lib.rs` | shared domain types, no deps |
| `pty` | `oximux-pty` | `src/lib.rs` | `TerminalBackend` + `PortablePtyBackend` |
| `proc-cwd` | `oximux-proc-cwd` | `src/lib.rs` | resolve a process's working dir |
| `proc-tree` | `oximux-proc-tree` | `src/lib.rs` | walk a process's descendants and read their argument vectors — how a plain terminal is known to be running an agent CLI (see *Ambient agent detection*). Sibling of `proc-cwd` for the same reason: dependency-light kernel introspection (`proc_listpids` + `KERN_PROCARGS2` on macOS, `/proc` on Linux, a Toolhelp snapshot on Windows) whose consumers share nothing else |
| `owner-only` | `oximux-owner-only` | `src/lib.rs` | restrict a file or directory to the account that created it — `0600`/`0700` on unix, a stated *protected* DACL on Windows; shared by the desktop app, relay daemon, and remote host so the SID lookup exists once |
| `no-window` | `oximux-no-window` | `src/lib.rs` | `CREATE_NO_WINDOW` for every child spawned on Windows, so a console-subsystem child (git, rg, an agent CLI's node) doesn't flash an empty console; a no-op applied unconditionally on every other platform |
| `job-object` | `oximux-job-object` | `src/lib.rs` | Windows-only child-tree kill via a Job Object (`KILL_ON_JOB_CLOSE`) — terminating or crash-losing the host reaps every descendant, the tree semantics a unix process-group signal already gives for free; compiles to nothing off Windows |
| `single-instance` | `oximux-single-instance` | `src/lib.rs` | non-blocking advisory file lock (`flock` / `LockFileEx` via `fd-lock`) deciding which process owns a per-data-directory role — extracted from the desktop's GUI singleton guard so `oximux serve` contends for the same roles (GUI singleton, schedule ticker) with identical semantics |
| `shell-env` | `oximux-shell-env` | `src/lib.rs` | the default shell to spawn a new terminal with, and what its environment needs — shared by `oximux-pty`'s in-process backend and the relay daemon so the rule exists once, not twice out of sync |
| `computer-use` | `oximux-computer-use` | `src/lib.rs` | `cua-driver` discovery/verify, permission gate, MCP declaration, one-click installer (`install/`) — never starts/supervises the daemon |
| `macos-trust` | `oximux-macos-trust` | `src/lib.rs` | shared codesign/spctl verification + crash-safe `renamex_np` bundle swap, extracted from `computer-use` so it and `auto-update` don't fork their own copies |
| `auto-update` | `oximux-auto-update` | `src/lib.rs` | desktop app self-update: GitHub release feed, download/mount/stage/verify pipeline, `UpdateStatus` state machine — swap is staged only, never live |
| `git` | `oximux-git` | `src/lib.rs` | `Repository`, `StatusPoller`, git ops, `GhCmd` |
| `agent-core` | `oximux-agent-core` | `src/lib.rs` | portable `ThreadEvent` vocabulary + stream-json decoder + `ChatThread` fold (serde/serde_json/tracing only, no pty/rusqlite/ACP/gpui/tokio — mobile-portable); re-exported by `agents` under `crate::thread::*` |
| `agents` | `oximux-agents` | `src/lib.rs` | `AgentRuntime` trait, `CliRuntime`, `StatusMachine`; `SessionRegistry` (gpui-free session event bus + command surface, built for Remote Control, not yet wired into the view) |
| `remote-proto` | `oximux-remote-proto` | `src/lib.rs` | transport-free Remote Control wire vocabulary — postcard RPC envelope, `HostEvent` stream frame, `PairingTicket` codec, `Transport` trait seam; `remote-host`, `remote-session`, `remote-iroh`, and the `oximux` CLI all speak it |
| `remote-local` | `oximux-remote-local` | `src/lib.rs` | the same-machine control transport: the owner-only unix socket / named pipe the `oximux` CLI uses to reach a host (desktop app or `oximux serve`), behind `remote-proto`'s `Transport` seam — both the listener a host binds and the dial the CLI makes live here |
| `remote-host` | `oximux-remote-host` | `src/lib.rs` | the remote-control host core: transport-agnostic RPC dispatcher, two-key pairing/auth handshake + ACL, host identity; serves `agents`' `SessionRegistry` over `remote-proto`. Used by both hosts (`apps/desktop` and `apps/cli`'s `serve`) — never ships to mobile |
| `remote-session` | `oximux-remote-session` | `src/lib.rs` | the client-side remote-control session (the phone's Rust core) — pure Rust, no FFI, unit-testable over the in-memory loopback against the real `remote-host` dispatcher; `mobile-core` wraps it |
| `remote-iroh` | `oximux-remote-iroh` | `src/lib.rs` | the production iroh P2P (QUIC) `Transport`/`Connector` impls beneath `remote-session` and `remote-host`; its `host` feature — off by default in this workspace alias, opted into by both hosts (`apps/desktop` and `apps/cli`) — adds the accept loop, so the mobile core never links the PTY-spawning code |
| `mobile-core` | `oximux-mobile-core` | `src/lib.rs` | uniffi binding wrapping `remote-session` + `remote-iroh` into a typed async + streamed-callback surface for the React Native app; builds `cdylib` (Android) / `staticlib` (iOS) alongside a normal `lib` |
| `editor` | `oximux-editor` | `src/lib.rs` | gpui-component editor wrapper + LSP glue |
| `dictation` | `oximux-dictation` | `src/lib.rs` | offline voice dictation: cpal mic capture → 16kHz resample → sherpa-onnx decode (Whisper for Vietnamese, Parakeet for English); channel-based `DictationController`, no GPUI dependency |
| `storage` | `oximux-storage` | `src/lib.rs` | SQLite + migration ladder + CI guard |
| `settings` | `oximux-settings` | `src/lib.rs` | theme tokens, density, typography, TOML config |
| `relay-proto` | `oximux-relay-proto` | `src/lib.rs` | wire protocol shared by daemon + client |
| `relay` | `oximux-relay` | `src/lib.rs` + `src/main.rs` | out-of-process PTY relay daemon |
| `relay-client` | `oximux-relay-client` | `src/lib.rs` | in-app client for the relay daemon |
| `relay-supervisor` | `oximux-relay-supervisor` | `src/lib.rs` | ensures an `oximux-relay` daemon is alive and hands back a connected `RelayClient` — extracted from the desktop app so `oximux serve` supervises the same daemon with identical detach recipes and version-mismatch handling |
| `relay-terminals` | `oximux-relay-terminals` | `src/lib.rs` | `remote-host`'s `TerminalSource` seam implemented over the relay daemon — extracted from the desktop so `oximux serve` exposes the same terminals the same way, gap semantics included |
| `ui` | `oximux-ui` | `src/lib.rs` | app-agnostic widgets (`FloatingSurface`, buttons, `ConfirmDialog`); re-exported as `crate::ui` |
| `xtask/` | `xtask` | `src/main.rs` | repo lint orchestrator (`file-size-lint` etc.) |

### Apps (`apps/<dir>/` → package → bin → purpose)

| App dir | Package | Cargo bin | Installed as | Purpose |
|---|---|---|---|---|
| `desktop` | `oximux-app` | `oximux` | `oximux` | GPUI cockpit; all views (the 73%-LOC crate) |
| `cli` | `oximux-cli` | `oximux-cli` | `oximux` | scriptable client of a running host (desktop app or `oximux serve`) — every verb, plus `oximux serve` itself and self-update |

The cargo bin target and the installed command name differ for `apps/cli`
deliberately: `apps/desktop` already owns the bin name `oximux`, and two
packages producing one bin name would make `cargo build --workspace`
overwrite whichever built second. The installer still places `apps/cli`'s
binary on `PATH` under the name users actually type, `oximux`.

### `oximux serve` topology (headless host)

`apps/cli/src/serve/` runs the same `Dispatcher` / `SessionRegistry` /
storage / relay stack the desktop app hosts, minus every view. It binds two
listeners: the owner-only local control socket (`remote-local`) the `oximux`
CLI dials, and the iroh endpoint (`remote-iroh`) paired devices reach.

Stdout carries exactly one line, the readiness JSON, and nothing else — all
logging goes to stderr. By default serve shares the desktop app's data
directory, so sessions, transcripts, and pairings are one set regardless of
which host is running, and it contends for a single-owner advisory lock
(`single-instance`) so only one process fires the data dir's schedule ticker.
Each agent serve spawns is granted its own session-scoped local credential,
confining it to its own conversation instead of the operator's full scope.

### `apps/desktop/src/` — top-level (non-view)

| File / folder | Holds |
|---|---|
| `workspace_root/` | `WorkspaceRoot` — one per window; owns panes + sidebar (`mod`/`ops`/`render`, plus `stash_dialogs` for the stash section's modals) |
| `project_panes_factory.rs` | manifest save/load, pane-buffer load, attach-reconcile |
| `actions.rs` / `state.rs` / `left_rail_layout.rs` | GPUI actions, app state, rail layout |
| `agent_glue/` | app-side agent wiring (bridges `oximux-agents` ↔ views) |
| `app_settings/` | in-app settings store + persistence |
| `keymap_registry/` | keybinding registration |
| `loaders/` | startup data loaders |
| `notifier/` | OS notifications |
| `platform/` | macOS-specific glue (App Nap, single-instance) |
| `session_restore/` | cold/warm session restore orchestration |
| `updater.rs` | `UpdaterState` global; 6h background-check ticker; boot sweep; quit-time swap (`apply_pending_at_quit`, called from `on_app_quit`); user-initiated restart |

### `apps/desktop/src/shell/<domain>/` — GPUI views

One folder per cockpit zone: `agent_ui`, `agents_dashboard`, `browser_view`,
`chrome`, `command_palette`, `commit_dialog`, `compose_bar`, `diff_view`,
`file_explorer`, `forge`, `git_panel`, `left_rail`, `onboarding`, `pane_group`,
`panes`, `pr_dialog`, `project_panes`, `right_sidebar`, `search_panel`, `session_history`,
`settings_modal`, `source_control`, `stash_panel`, `tasks_view`, `terminal`,
`usage`, `welcome`, `workspace`. Each re-exports its modules
so existing `crate::shell::<name>::…` paths resolve regardless of folder.
A small set of cross-cutting glue files (`context_env.rs`, `openable_text_file.rs`,
`open_url.rs`, `cwd_resolver.rs`) stays loose at `shell/` root by design.

---

## WorkspaceRoot + RightSidebar wiring

```
WorkspaceRoot (GPUI entity)
├── fields
│   ├── main_pane: Entity<MainPane>        ← grid of pane leaves (Terminal | Editor | Diff | Browser | Tasks)
│   ├── right_sidebar: Option<Entity<RightSidebar>>   ← None when no git repo
│   │     open_file_in_active_pane(path, window, cx)
│   │       → MainPane::open_editor_in_focused_pane(path, window, cx)
│   ├── palette: Entity<PaletteModal>      ← Cmd+P / Cmd+Shift+P overlay
│   ├── settings_modal: Entity<SettingsModal> ← Cmd+, / left-rail cog overlay
│   ├── onboarding: Entity<OnboardingWizard>  ← first-run welcome wizard (boot-gate mailbox; palette "Show Welcome Wizard")
│   └── floating_terminal: Option<Entity<FloatingTerminal>> ← Cmd+Shift+T PiP overlay
│         draggable/resizable; PTY persists across hide; geometry debounce-persisted
│
└── RightSidebar (GPUI entity, present only when cwd is a git repo)
    ├── active_tab: RightTab  (Explorer | Search | SourceControl | Files)
    ├── activity_bar: 40px strip; single-letter glyph buttons
    ├── _repo: Arc<Repository>
    ├── _poller: StatusPoller              ← AbortHandle; drops → task stops
    ├── file_explorer: Entity<FileExplorer> ← shown on Explorer tab
    │     uniform_list virtualized tree; lazy load; git status badges M/A/D/R/U/C
    │     5s tokio timeout per dir load; focus-regain refresh via observe_window_activation
    ├── file_tree_view: Entity<FileTreeView> ← shown on Files tab (always visible; no repo gate)
    │     on_open callback → WorkspaceRoot::open_file_in_active_pane
    ├── source_control: Entity<SourceControlPanel> ← shown on SourceControl tab
    │     primary_action resolver: CreatePR (in-sync + GitHub + no open PR) /
    │       Push / Sync-ahead / Commit / Stage All / Pull
    │     pr_ops.rs: gh pr create --fill → opens browser
    │     ci_status.rs: gh pr checks → ✓N ✗N ●N row; 30s throttle; only while PR open
    ├── git_panel: Entity<GitPanel>        ← changed-files list
    └── diff_view: Entity<DiffView>        ← hunk diff render
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
  → StashSelectedRequested { paths, needs_untracked, rename_pairs,
                             untracked_count }
      built by stash_selection::plan_stash_selection from the live GitState:
        · a selected path absent from GitState is dropped (a pathspec git
          does not know fails the WHOLE push, not just that path)
        · a STAGED rename contributes its original path too, plus a pair to
          unstage first — after `git mv`, the original is in HEAD alone and
          no pathspec can reach it
        · -u is forced by any untracked path OR any rename (whose target is
          untracked once unstaged); without it git refuses the pathspec and
          stashes nothing at all
        · refused above ARGV_BUDGET_BYTES — a stash is one commit, so the
          argv cannot be chunked the way stage/unstage can
      → host mounts the push dialog scoped to those paths, then runs the op
        on StashPanel so the stash list refreshes as part of it

DiffView (GPUI entity)
  ← DiffView::load(path, staged, cx)
      → Repository::diff_for_path (tokio)
      → parse_unified_diff
      → render hunk lines (capped at LARGE_DIFF_LINE_THRESHOLD; expand() removes cap)

CommitDialog (GPUI entity)
  ← cycle_prefix() → conventional-commit prefix dropdown
  ← submit() → Repository::commit (tokio)

ConfirmDialog (GPUI entity)
  ← one-click confirm/cancel gate for destructive ops (revert, stash drop, worktree remove)
  ← optional secondary action for three-way prompts (Save / Discard / Cancel)

StashPanel (GPUI entity)
  ← refresh() / force_refresh() → git stash list (15 s read-TTL)
  ← apply/pop → git stash ops, each resolving its target by sha
                (painted stash@{N} passed as a tiebreaker: a sha is not
                 unique on the stack)
  ← toggle_expanded(sha) → git stash files, cached per sha until refresh
  → DropStashRequested → host confirm dialog → drop_confirmed()
  → ShowStashFileRequested → PaneGroupTabKind::StashFile { sha, path }
                             Tracked   → range <sha>^..<sha>
                             Untracked → <sha>^3 with NO base (diff_in_rev)
  ← row action cluster is hidden until hover; the guaranteed path to every
    verb is the right-click menu, which is why the two shipped together
    Five glyphs, in the reference cockpit's order: Apply / Pop / Open All
    Changes / Rename… / Drop. Apply and Pop are one glyph with one
    difference — a lidded box with its contents rising out, and the same box
    with its far wall replaced by an × — because that is the only difference
    between the verbs. The cluster is absolutely positioned, so a glyph costs
    coverage of the metadata on hover, never a verb pushed off a 220px panel;
    Branch from Stash stays menu-only because it asks for a name AND consumes
    the stash, which no one-glyph control should start
  ← push_paths(msg, -u, paths, rename_pairs, on_success) — the partial stash
    fired from GitPanel. Runs HERE, not there, so the stash list refresh is
    part of the op. Sequence: unstage_paths(rename_pairs) → stash_push, and
    on failure stage_paths(rename_pairs) to put the staging back before the
    error is toasted. on_success is deferred (the cross-entity hop out of a
    leased entity) and only clears the file list's selection when the push
    actually succeeded.
  → ShowStashAllRequested → PaneGroupTabKind::StashAll { sha }
                            DiffView::load_stash → repo.stash_all_files(sha)
                            = commit_files(sha) ++ commit_files(sha^3)
                            commit_files alone is --first-parent and drops the
                            untracked half; it is composed, never modified,
                            because it also serves every commit-detail tab
  → BranchFromStashRequested → host name dialog → branch_from_stash()
      is_valid_branch_name (git check-ref-format --branch, read-only) runs
      FIRST, so a bad name never reaches a mutating call. On success
      git stash branch CONSUMES the stash and restores the staged/unstaged
      split (it applies with --index; the new branch is at the stash's base,
      so there is never an index to conflict with). On failure the branch may
      ALREADY exist and be checked out, which is why the host hook is
      on_done, not on_success — see OpHooks in stash_panel/ops.rs.
  ← apply_file(sha, path) → repo.stash_apply_file — ONE file out of a stash,
      as a patch. No confirm dialog and no event: plain `git apply` writes the
      whole patch or nothing at all, so there is no outcome to warn about.
      Worktree only, index untouched (the toast says "unstaged"), stash kept.
      Patch = git show --format= --binary --no-renames [--first-parent] <rev>
      -- :(literal)<path>, where rev is <sha> for a tracked file and <sha>^3
      for an untracked one — so this reaches the untracked rows restore cannot.
      NOT --3way: it would succeed more often but writes conflict markers into
      the file on a real conflict, which is the outcome this verb exists to
      avoid. --no-renames so the patch touches only the path the row named;
      with detection on, a rename would also DELETE the old path. An empty
      patch is an explicit error — `git apply` exits 0 on empty input, so the
      "nothing happened" case would otherwise toast success
  → RestoreStashFileRequested → host confirm dialog → restore_file_confirmed()
      git checkout <sha> -- <path> via run_pathspec_op (a bare pathspec makes
      restoring a[1].rs also overwrite a1.rs). Writes the INDEX as well as the
      worktree, so the dialog says "and staged". Tracked files only —
      untracked ones live in ^3, not in the stash commit's tree, and the
      checkout can only fail; the panel refuses to emit for them and the menu
      omits the item.
  → RenameStashRequested { sha, painted, message, depth } → host message
      dialog → rename_confirmed()
      Git has NO `stash rename`. Repository::stash_rename composes one out of
      drop + store: everything from the top down to and including the target
      comes off, the target is re-stored under its new message, and the rest
      go back in reverse order. Same shas, same positions, same dates —
      `store` writes a reflog pointer, never a commit. `depth` is how many
      entries above the target get taken off and put back, and the dialog's
      copy names the number rather than hedging.
      The target is chosen with the SAME tiebreaker every other op uses: the
      painted address when it still holds that sha, else the first match.
      `rename_confirmed` resolves the row first and hands the ANSWER down.
      Not decoration — `git stash store` duplicates a sha that is on the
      stack but not on top (it is a no-op only for `stash@{0}`), so two rows
      can share one commit, and picking the first match renamed a row the
      user never clicked while reporting success. Found in review of #28.
      Four properties that are not optional:
        · the FULL recovery sequence is tracing::warn!-ed BEFORE the first
          drop (sha per line; the message is a structured field, never
          interpolated into a fake command line — it legally holds quotes
          and `$`)
        · step 3 re-resolves the index BY SHA on every iteration, so a
          `git stash push` landing mid-loop from a terminal or a sibling
          worktree SURVIVES. It is reordered (it settles below the restored
          entries), not deleted. Documented, not eliminated — there is no
          lock around a 2N+2-subprocess sequence
        · a failed `store` in step 5 is recorded and the loop CONTINUES;
          aborting there would strand the entries below the failure too.
          The op reports that as a Warning toast, not a success
      The `.git/logs/refs/stash` rewrite was considered and rejected: no
      refs/stash.lock, wrong path in a linked worktree (--git-common-dir),
      absent under git 2.45+'s reftable backend, hostile to the Windows port.

StashPanel keyboard cursor (stash_panel/keyboard.rs)
  ← cursor: Option<StashCursor> — PANEL state, keyed by sha, never focus
    state: Cmd+Backspace opens a dialog that takes focus away, and a
    focus-backed cursor would be gone by the time Escape came back.
    Cleared only when the row it names leaves the stack (falls back to the
    first row, so the keyboard path does not die with a dropped stash).
  ← visible_rows() derives the walk from list + expansion + file cache +
    collapsed folders on demand; folder rows are NOT cursor targets
  ← painted_index() is a SECOND walk that counts folder rows and the one-line
    loading/failed/empty notes too, because those occupy vertical space. It
    feeds scroll_cursor_into_view(), without which the arrow keys are unusable
    past the first screenful — the body is bounded, so the cursor simply walks
    off the bottom. Arithmetic, not ScrollHandle::scroll_to_item (that sees
    only the container's direct children — one wrapper column here), and exact
    only because EVERY row this body paints is density.h_row tall
  ← ↑/↓ move (clamped, never wrapping) · → expand · ← collapse, or climb
    from a file row to its stash · Enter opens the diff / toggles ·
    Cmd+Backspace emits DropStashRequested — the same confirm gate
  ← bindings are CONTEXT-SCOPED (key_context "StashPanel"), installed from
    keybindings_settings::install alongside the terminal's and the session
    history modal's. NOT registry entries: keymap_registry builds every
    binding with no context, so a registry row for bare `up` would shadow
    the arrow keys application-wide. Cost: these five are not rebindable
    from the settings pane. Keymap actions rather than on_key_down because
    macOS delivers Cmd-modified keys via performKeyEquivalent:, which
    bypasses element key listeners.
  ← a left-click on a row also sets the cursor; the list takes focus on a
    DEFERRED window.focus from the scroll body's mouse-down (sync focus inside
    a mouse handler is clobbered)
  ← OPENING A DIFF HANDS FOCUS TO THE TAB, so the arrow keys go quiet until
    the panel is focused again. Intended (it is what opening a tab means
    everywhere), but it looks exactly like a broken binding from the outside —
    `↑` "works" from a stash row and "fails" from a file row purely because
    the file row just opened a tab
  ← the context + the focus handle sit on the LIST element, not on the
    section container: the keyboard resize rail is a focusable sibling whose
    own on_key_down owns Arrow/Shift+Arrow/Home/End, and a shared parent
    context would have one arrow keystroke matching a cursor binding AND
    reaching the rail

StashPanel list/tree layout (stash_panel/tree_view.rs)
  ← header toggle, list-tree.svg ⇄ list-collapse.svg, same pair and wording
    as the CHANGES toolbar
  ← persisted GLOBALLY (scm_layout_settings::KEY_SCM_STASH_VIEW_MODE), not
    in the per-worktree V006 row: refs/stash lives in the git common dir, so
    every worktree shows one stack and a per-worktree key would paint it two
    different ways in sibling windows
  ← a SHIM maps StashFile → FileStatus and reuses TreeSection::Staged, which
    is being used as a carrier for "read the index column". No
    TreeSection::Stash variant — the three existing ones are claims about
    the index, and a stash has no index to be on one side of
  ← collapsed folders are per-sha (two stashes can both hold src/) and
    survive a flat⇄tree round trip; adopt_list drops them with the file
    cache they were derived from

oximux_ui::menu — MenuRow + separator + ROW_PADDING_X
  ← ONE row builder for every context menu (stash, commit, tab, terminal,
    file tree, git-panel row). They painted identically — same height,
    padding, radius, type size, hover — but had drifted into four call
    shapes: explicit `fg` for a destructive tint, `enabled`-derived fg, a
    trailing shortcut column, and `impl Into<SharedString>` labels
  ← builder, so a caller asks only for what it varies: .fg() .shortcut()
    .enabled(), then .build(handler). Handler is mouse-DOWN, not click — an
    overlay that closes on outside-mouse-down would race its items on mouse-up
  ← SHORTCUT_GAP is a literal 12.0, NOT density.gap_inline (6.0): that is what
    the terminal menu used, and a refactor must not move pixels
  ← ROW_PADDING_X was a private const in THIRTEEN files, all 10.0
  ← the menu CARD stays per-menu — width, cursor/chip anchoring, scrim and
    close-on-outside-click genuinely differ
  ← NOT folded in: status_bar's divider (takes typography), the schedules
    dropdown row (carries selected state), branch_picker's separator_row
    (returns AnyElement). They look alike and are not

stash_panel/form.rs — the three stash modals' shared chrome
  ← PIECES, not a body builder: form_card / form_title / form_subtitle /
    form_field_label / form_note / form_buttons. The push form's body carries
    a dynamic title, a path list, a checkbox with forced-and-disabled logic
    and a conditional note, so a fixed-order builder would have been forked
    immediately. Each dialog writes its own body
  ← form_subtitle is fg_muted, form_note is fg_subtle, and the split is
    load-bearing: a subtitle names the SUBJECT and must read at a glance, a
    note explains the CONSEQUENCE and should recede
  ← confirm_disabled is passed, not derived — true for branch/rename, false
    for push (its message is optional), and each dialog's `try_confirm` has
    to agree with what it passes

StashContextMenu (GPUI entity, mounted on WorkspaceRoot)
  ← OpenStashContextMenuAt { x, y, sha, index, message, relative, branch,
                             file_path, file_untracked }
      file_path == None → the stash row, in three groups:
        ways back — Apply / Pop / Apply with index (git stash apply --index,
          restores the staged split from the stash commit's ^2) /
          Branch from Stash… (an apply that also names a branch, and the only
          one that consumes the stash) → emits BranchFromStashRequested
          Rename… (a mutation of the whole prefix of the stack, so it sits
          with the mutations) → emits RenameStashRequested
        read-only — Open All Changes · Copy Message / Copy SHA
        Drop… → emits DropStashRequested, the same confirm gate the row uses
      file_path == Some → a file inside an expanded stash:
        Apply Changes (fires at once — see apply_file) · Open Changes (panel
        resolves StashFileOrigin from its own cache) · Copy Relative Path ·
        Restore This File… (absent when file_untracked; origin has to travel
        in the payload because the menu decides at RENDER time, and only the
        row that painted the file knows)
        Apply and Restore are both "bring this file back" and are deliberately
        two items at opposite ends of the menu: apply merges and refuses
        rather than overwrite, restore overwrites and stages. The rule and the
        destructive tint between them are the difference being chosen
  ← WeakEntity<StashPanel>; dismisses silently after a workspace switch
  ← an item that could only fail is omitted, never disabled

SourceControlPanel::refresh_graph_if_head_moved (picker_wiring.rs)
  ← called from the poll observer on EVERY tick, before git_state is replaced
  ← the other half of refresh_after_branch_change: that one covers the HEAD
    movers the panel performs itself, this one covers every mover it does not —
    a commit, amend, reset, rebase or checkout from the user's terminal or a
    sibling worktree. Those fire nothing in the app, so the graph kept painting
    history that no longer had HEAD at its tip
  ← free: GitState already carries head_oid and the poller already delivers it,
    so this is a comparison, not a query. Self-heals within one tick
  ← `head_oid_seen` distinguishes "HEAD is genuinely None" (a repo with no
    commit) from "we have not looked yet", so boot is not read as a move
  ← ONLY the graph reloads. PR/CI is left alone — HEAD moving on the same
    branch does not invalidate its PR, and dropping it would make every
    terminal commit re-spend the forge round-trip

SourceControlPanel state observer (source_control/state_observer.rs)
  ← everything that happens BECAUSE a poll arrived, in order: graph-if-HEAD-
    moved, git_state, the commit area's staged snapshot, the rebase base, the
    in-progress-op banner, and (throttled) forge PR/CI
  ← split out of mod.rs, which sits on the file-size warn line and had already
    been trimmed once for it (Phase 8 moved set_poller + refresh_after_branch_
    change to picker_wiring.rs)

SourceControlPanel::refresh_after_branch_change (picker_wiring.rs)
  ← called by switch_to_branch AND by the branch-from-stash on_done hook
  ← commit_graph.refresh() — the one thing the StatusPoller never covered.
    The branch name, the file list and "Committed on Branch" all arrive on
    the poll channel and self-heal in one 500 ms tick; the graph's only other
    refresh sites are a completed commit op and the two manual buttons, so it
    kept painting the previous branch's history indefinitely. Pre-existing
    gap, found while wiring branch-from-stash, fixed for both callers.
  ← PR/CI state dropped (it belongs to the branch just left) + poller.kick()

SourceControlPanel::sections (the stash section + the graph)
  ← both heights persisted separately; fit_sections() splits one budget
  ← sync_section_budget() on every render — the only place that sees both
  ← each section frame is flex_shrink + min_h, so neither can be pushed
    out of the column whatever the chrome above costs

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
│     tokio 50ms poll task → poll_helpers::process_poll_events
│       TerminalEvent::Output → AgentOscScanner::feed (strip OSC-9999 → cleaned + SidebandEvent)
│                               → StatusMachine::feed on cleaned bytes; sideband applies last via feed_sideband→force
│       on TerminalEvent::Exit → StatusMachine::note_exit / note_interrupted
│       exits when saw_exit || status_tx.is_closed()
│     watch::channel<AgentSnapshot> — cloned to all subscribers (badge / sidebar / dashboard)
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

`PiAdapter` is the third branded adapter: builds `pi [--session ID] [--model M] [<prompt>]` with the prompt as a trailing positional. It is also a chat backend of its own (`pi --mode rpc`) — the same dual terminal/chat nature Claude and Codex have. Empty `status_patterns()` with the same calibration deferral. `OmpAdapter` is the fourth: `omp` is a Pi fork with the same dual nature — `omp [--resume ID] [--model M] [<prompt>]` in a PTY, and a native chat backend over `omp --mode rpc-ui` (`thread/omp/`: `ready`/`negotiate_protocol{2}` versioned handshake, inbound `rpc_chunk` reassembly for >1MiB frames, and — the piece Pi lacks — per-call tool approval delivered as `extension_ui_request` select dialogs, mapped to the standard permission cards; the `--approval-mode` posture flag is ALWAYS passed explicitly because omp's own default is `yolo`). Resume is by full canonical session UUID only — omp's resolver prefix-matches with a silent cross-project fallback. (`CommandSpec::stdin_seed` remains a reserved path for adapters whose REPL takes no positional prompt; no built-in adapter currently uses it.)

The status channel carries `AgentSnapshot { status: AgentStatus, detail: Option<SidebandDetail> }` (not a bare `AgentStatus`): the regex `StatusMachine` path publishes `detail: None`, while an OSC-9999 sideband event (`osc_sideband::AgentOscScanner`) attaches structured `tool` / `tool_input` / `msg` detail. This closes the Codex/Pi `EMPTY_PATTERNS` blindness — an agent (or hook) emitting `ESC]9999;{"v":1,"state":"needs_approval",...}BEL` drives status with no regex pattern needed. `current_status()` still returns a bare `AgentStatus` for the common lifecycle-only consumer.

Future ACP runtime (v1.1) will be a sibling `AgentRuntime` impl with identical `watch::Receiver<AgentSnapshot>` contract — UI code subscribes to the trait, not the impl.

### Ambient agent detection (a hand-typed agent in a plain terminal)

An agent the user launched by typing `claude`/`codex`/… into an ordinary terminal has no `AgentRuntime`. Three signals combine to give it a rail row anyway, and they answer **different** questions — conflating them is what made this Claude-only and streaming-only:

| Signal | Answers | Covers | Lives for |
|---|---|---|---|
| Process tree (`oximux-proc-tree` → `agents::agent_process`) | *which* CLI is running, and *that* one is | every CLI | as long as the process |
| OSC-9999 sideband (`ambient_agent_scan`) | what it is doing, in detail | only CLIs OxiMux installs hooks for (Claude Code) | one event, 30-min TTL |
| OSC 0/2 title (`agents::agent_title`) | what it is doing, coarsely | only CLIs that write a title — several do not by default | one event |

**Presence is the process, never the output.** `PaneGroup::ambient_agents` treats a live agent process as the row's existence and lets the other two refine its status; an agent that has reported nothing is `Idle`, which is the common case (a CLI waiting at its prompt emits no hook, and most write no title). `TerminalView::poll_agent_process` runs the walk ahead of `tick`'s no-output early return — an idle agent produces no events, so a check downstream of that return could never see one arrive or leave — and `cx.notify()`s on change, which is what carries the row into the rail (rebuilt from the top of the workspace render).

**Identity is `argv[0]`, never the executable name.** Two measurements forced this. The kernel names a process for the binary it *resolved*, so a CLI installed as a symlink is named for the link's target — Claude Code's points at a file named for its version, which matches nothing. And the executable name was observed reporting an agent's name for a process whose arguments showed it was a search tool. `argv[0]` keeps the invoked path, and carries a script CLI's identity through its interpreter (`node …/gemini`). The executable name is the fallback only where arguments cannot be read (Windows, where the symlink convention does not apply either).

**What an agent SAID needs the agent's own hooks.** Presence and coarse activity come from the process and the title, but only the agent can report its reply — which is why a Codex row read `Codex · Idle` where a Claude row carried the actual answer. Codex has the same kind of lifecycle hooks as Claude, in `$CODEX_HOME/hooks.json`, in the same `{matcher?, hooks:[{type, command}]}` shape, delivering event JSON on stdin — so `oximux agent-status` serves both and only the field names differ (`crate::codex_status_hooks`, selected with `--format codex`).

Two constraints shape that install, both learned the hard way:
* Codex's `notify` config is deliberately **not** used. It is a single program rather than a list, so writing it would replace whatever the user already has there; `hooks.json` is a separate file with per-event arrays that ours can merge into.
* A Codex hook entry carries **no bookkeeping marker and no `async`** — Codex rejects fields it does not define, and a rejected file would silence the user's own hooks along with ours. With nothing to stamp, our entries are found again by their command (`agent_hooks_global::Dialect`), which is also how the reference cockpit does it. Claude's file keeps its marker: it tolerates unknown keys, and a marker can never mistake a hand-written hook that calls the same CLI for one of ours.

Hook **trust** is left to Codex, which holds a hashed ledger and asks the user to approve a new hook in its own TUI. Writing the file is a request, not a side effect: until the user says yes, nothing fires and the rail behaves exactly as it did before.

`agent_title::AGENT_LABELS` is the single naming vocabulary both detectors draw from, so a process-detected and a title-detected agent resolve to the same icon and adapter slug rather than rendering as two differently-styled rows for one agent.

**Where a detected agent is drawn.** A *single*-agent workspace is named on the card itself; a multi-agent one defers to the `N agents` disclosure below it (`project_group.rs`, gated on `rows.len() > 1`), which renders in both card layouts. The disclosure's collapsed chips and expanded sub-rows both draw one mark per row's `adapter_id`, so a worktree running three different CLIs shows three different brands rather than three copies of one.

The unit throughout is the **pane**, not the tab: `ambient_agents` yields one row per PTY, so a tab split into two panes running two agents is two agents. `ambient_agent_count` counts panes for the same reason — counting tabs made the status bar say "1 agent" under a rail that listed two rows. The catch is the **compact** layout: it drops line 2 to fit one row, and line 2 is where `Codex · Ready` would be — leaving the status dot as the only agent signal, and a dot says how an agent is doing, never which one it is. `workspace_card::compact_agent_glyph` fills that gap with the CLI's brand mark beside the name (generic glyph when no mark is bundled), so identity survives compact without duplicating what detailed and the disclosure already say.

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

**App-side supervisor** (`apps/desktop/src/relay_supervisor.rs`):

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

## Editor + LSP (Phase 5)

### Step 1 — spike (go)

> Cook report: `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`

```
apps/desktop/src/main.rs
  --editor-spike flag
    └── run_editor_spike()
          tokio Handle check (self-aborts with clear message if no reactor in scope)
          cx.open_window("OxiMux — Editor Spike")
            └── EditorView::new(file_path, cx)       crates/editor
                  gpui-component Input (code_editor("rust") mode)
                  attach_lsp("rust-analyzer", "rust", workspace_root, cx)
                    ├── LspClient::spawn(program, workspace_root, handle)
                    │     Content-Length framing (transport.rs)
                    │     initialize + initialized + didOpen handshake
                    │     dispatch loop: responses → pending_requests map
                    │                   server-initiated requests → {result:null}
                    │                   publishDiagnostics → WeakEntity<InputState>
                    │
                    └── register HoverProvider → LspHoverProvider
                          hover(pos, cx):
                            handle.spawn(async { LspClient::hover(pos) })  ← tokio worker
                            gpui::Task awaits JoinHandle                    ← GCD thread
```

Key design decisions locked in spike:
- One `LspClient` per (workspace_root, language); no pooling in v1.
- `tokio::runtime::Handle` captured at spawn, passed into GPUI tasks — avoids `rt.enter()` thread-local vs GCD-worker mismatch.
- `REQUEST_TIMEOUT = 5s` via `tokio::time::timeout`; no `$/cancelRequest`.
- Missing binary → `tracing::warn` + editor renders without LSP; no panic.
- Spike is read-only: no `didChange`, no save round-trip (step 2 owns that).

### Markdown rendered preview

`.md` / `.markdown` files open in a rendered view; `.mdx` keeps the plain code editor.

```
EditorView::new(path, cx)
  is_markdown_path(path) → true for .md / .markdown (not .mdx)
  md_mode: MarkdownViewMode  (Source | Preview | Split)  default = Preview
  │
  ├── header: mode_toggle() — segmented button row, right-aligned, markdown-only
  │
  └── body match md_mode:
        Source  → gpui-component Input (normal code editor)
        Preview → render_preview()
        Split   → h_resizable(source_pane | preview_pane)

render_preview()
  absolutize_image_paths(text, file_path)
    rewrites repo-relative ![](path) → file:// URI (pure fn)
  gpui-component text::markdown (GFM renderer)
    headings / bold / italic / inline-code / tables / task lists /
    blockquotes / links / fenced code blocks / images
```

**FileHttpClient** (`apps/desktop/src/file_http_client.rs`):

gpui defaults to `NullHttpClient` — its image element loads image URLs through the http client, never the filesystem. `FileHttpClient` is a `file://`-only `gpui::HttpClient` impl installed at app startup via `cx.set_http_client(Arc::new(FileHttpClient))`. It overrides `get()` because `http::Uri` rejects `file:///path` (empty authority) in the default path. HTTP/HTTPS requests are not forwarded (rejected).

```
main.rs
  cx.set_http_client(Arc::new(FileHttpClient))   ← installed once at boot
    └── FileHttpClient::get(file:///abs/path)
          read file bytes → Response with Content-Type image/*
```

**Deferred** (not in v1): WYSIWYG editing, scroll-sync, TOC, mode-cycle keybinding, remote http(s) image rendering, `.mdx` preview.

---

### Step 2 — save round-trip + LSP textDocument lifecycle (go, smoke green)

> Tester report: `plans/reports/tester-260522-1939-phase-05-step02-editor-save.md`  
> Code review: `plans/reports/code-review-260522-1939-phase-05-step02-editor-save.md` (8/10)

**EditorView lifecycle (step 2 additions):**

```
EditorView::new(path, cx)
  ├── uri: lsp_types::Uri  — parse-once from path_to_file_uri; reused for all LSP calls
  ├── dirty = false, doc_version = 1, last_sent_text = file_content
  └── cx.observe(&state, callback) → _observe_sub (keeps observer alive)
        callback fires on every cx.notify() from InputState (incl. silent undo/redo)
        decide_change_propagation(last_sent_text, current_text, doc_version)
          → None if text unchanged (cursor moves, scroll) — no-op
          → Some(prop) if text differs: last_sent_text = prop.text,
                                        dirty = true,
                                        doc_version = prop.new_version,
                                        client.did_change(&uri, version, text)

attach_lsp(program, lang, workspace_root, cx)
  └── lsp_bridge::spawn_attach_lsp(...)
        tokio: LspClient::spawn → initialize/initialized/didOpen (version=1)
               reads file again → did_open_text
        GPUI: entity.update → editor.set_lsp_client(client, did_open_text)
               if last_sent_text ≠ did_open_text  ← buffer drifted during handshake
                 doc_version += 1; client.did_change(catch-up)  ← Fix #3

on Cmd+S (SaveFile action)
  ├── fs::write(file_path, text)
  ├── if Ok: dirty = false; client.did_save(&uri); cx.notify()
  └── if Err: tracing::error; dirty stays true; cx.notify()

impl Drop for EditorView
  └── client.did_close(&uri)  — sync UnboundedSender::send; non-blocking; safe in Drop
```

**Why `cx.observe` not `cx.subscribe(InputEvent::Change)`:**  
gpui-component's undo/redo calls `replace_text_in_range_silent` which bypasses `InputEvent::Change`. `cx.observe` catches every `cx.notify()` from `InputState`; the `decide_change_propagation` guard discards cursor-move noise at zero allocation cost.

**Key invariants:**
- `dirty` and `doc_version` live on `EditorView`, not `InputState` — no gpui-component fork needed.
- `doc_version` is a plain `i32` (GPUI main-thread only; no atomic needed); strictly monotonic across normal edits + undo/redo.
- `didChange` uses full-sync (entire buffer text, `range: None`) per LSP §3.17.2; 0ms debounce.
- LSP calls are no-ops when `lsp_client` is `None` (editor-without-LSP degraded path preserved).
- `SaveFile` action declared in `oximux-editor` (not `oximux-app::actions`) to break the `oximux-app → oximux-editor → oximux-app` circular crate dependency.

---

### Step 3 — file tree backend (headless entity)

```
FileTree (GPUI entity)
  ├── load(root_path, cx)
  │     cx.background_executor().spawn → walker::walk_dir(root, SKIP_NAMES)
  │       WalkBuilder::max_depth(1) + filter_entry → Vec<(PathBuf, bool)>
  │       sort_entries: dirs first, then ASCII-lowercase alpha
  │     → inserts children into nodes map; emits FileTreeEvent::Loaded(id)
  │
  ├── expand(node_id, cx)
  │     remove_subtree(id) — recursive evict from nodes + open_dirs (prevents phantom Refresh)
  │     then re-runs walker for that dir → Loaded(id)
  │
  ├── collapse(node_id) — marks closed; children stay in map (lazy re-walk on next expand)
  │
  └── cx.spawn → watcher event loop
        spawn_watcher(root, tx) wraps notify_debouncer_full::new_debouncer(200ms)
          closure: DebounceEventResult → tx.send(paths)
        loop: rx.recv() → is_ignored(path, SKIP_NAMES)
                        → find_node_to_invalidate(path, nodes)
                        → emits FileTreeEvent::Refresh(ancestor_id)
```

**Key invariants:**
- Single FSEvents watch from root; userspace `is_ignored` filter in debounce callback (not kernel-level).
- `SKIP_NAMES` const shared by walker + watcher — one place to add `node_modules` etc.
- `notify` accessed via `notify_debouncer_full::notify` re-export; avoids two-versions-of-notify compile failure (debouncer 0.4 pins notify 7).
- `Refresh(id)` is coarse — step 4 diffs against UI state; step 3 does not diff.

---

### Step 4 — file tree UI (FileTreeView)

`FileTreeView` lives in `oximux-app/shell/file_tree_view.rs` and subscribes to `Entity<FileTree>` (from `oximux-editor`) via `cx.subscribe_in` registered before the first `expand()` call so no `Loaded` event can be missed.

**Lazy expand pattern:**
```
dir click → tree.expand(id)           oximux-editor entity
  immediately: RowKind::Placeholder    italic "…" sentinel shown
  Loaded(id) event fires later
    → rebuild_rows() swaps sentinel for real children
```

**Click flows:**
- Dir click: toggles `expanded_ids: HashSet<TreeNodeId>` on view; calls `tree.expand(id)` if newly expanded
- File click: fires `on_open: Arc<dyn Fn(PathBuf, &mut Window, &mut App)>` callback

**Key invariants:**
- `expanded_ids` (view) is separate from `FileTreeNode.loaded` (model) — chevron direction reads `expanded`, not `loaded`
- Raw `uniform_list` used, not `gpui-component::Tree` — matches `FileExplorer` precedent; avoids auto-expand-on-click conflict with lazy walker model
- `build_display_rows` is a pure fn extracted for unit testing (8 tests in `app/tests/file_tree_view_unit.rs`)

---

### Step 5 — pane as editor host (workspace wiring)

`MainPane` workspace grid is no longer terminal-only. Each leaf now holds a `PaneContent` enum.

```
PaneContent::Terminal(TerminalSplitTree)
PaneContent::Editor(Entity<EditorView>)
PaneContent::Diff(Entity<DiffView>)        // read-only patch view
PaneContent::Browser(Entity<BrowserView>)  // inline webview
PaneContent::Tasks(Entity<TasksView>)      // main-panel issue/PR table, singleton tab

MainPane::open_editor_in_focused_pane(path, window, cx)
  same-path short-circuit → no-op if focused leaf already shows that file
  else → replace focused leaf content with EditorView::new(path, cx)

RightSidebar (Files tab — always visible, no repo gate)
  FileTreeView on_open callback
    → OnOpenFile event
    → WorkspaceRoot::open_file_in_active_pane(path, window, cx)
    → MainPane::open_editor_in_focused_pane(path, window, cx)
```

**EditorView focus parity (step 5 addition):**
- `focused: bool` field mirrored by `cx.on_focus` / `cx.on_blur` — matches `TerminalView` observer pattern
- `set_window_title` removed from `EditorView::render`: multi-leaf editors cannot share a single window title

**Key invariants:**
- Files tab (`RightTab::Files`): always present regardless of whether cwd is a git repo
- `SelectFilesTab` action bound to `Cmd+Shift+T`
- Editor leaves persist for the running session; silently dropped on app quit or project switch (no editor restore in v1)

---

## Multi-window architecture (mux-P3)

```
WindowRegistry (GPUI global, app-lifetime)
  ├── windows: Vec<RegisteredWindow>
  │     each: window_id (GPUI WindowId) + persist_id ("main" | "w{n}") + Entity<WorkspaceRoot>
  │
  ├── mint_persist_id() → "main" (first) | "w2", "w3", … (subsequent)
  │
  └── pending_tearoffs: Vec<PendingTearOff>
        pushed by source window before calling open_workspace_window_with;
        consumed by destination window's WorkspaceRoot::new
```

**Window lifecycle:**
- `NewWindow` (Cmd+N) → `open_workspace_window(cx, repo, app_state)` → mints id → `open_workspace_window_with`
- Last-window close → `cx.quit()`; non-last close dismisses only that window (single quit observer guards)
- Boot: `capture_session` manifest → `open_workspace_window_with` per persisted window_id

**Cross-window tear-off (`MoveTabToNewWindow`):**
```
source WorkspaceRoot
  1. TerminalBackend::detach(pty_id)   ← releases relay attachment; PTY stays alive
  2. removes tab from its group
  3. pushes PendingTearOff { dest_window_id, relay_id, … }
  4. open_workspace_window_with(cx, …, dest_window_id)

destination WorkspaceRoot::new
  consume_pending_tearoff(dest_window_id)
  → attach_pty_existing(relay_id)     ← re-mounts PTY in new window
```

Relay multiplexes one subscriber per `pty_id`; detach source before attach destination is the mandatory ordering.

**Per-window persistence (V005):**

`pane_buffers` and `pane_relay_ids` now include `window_id` in their primary keys (`DEFAULT 'main'` for existing rows). Settings layout capture/restore thread the window id; each window captures/restores its own pane tree independently.

---

## Per-pane tabs — LeafTabs (mux-P4)

```
TerminalSplitTree
  └── panes: Vec<Option<LeafTabs>>      ← one LeafTabs per split leaf

LeafTabs
  ├── tabs: Vec<(Entity<TerminalView>, Observer)>
  ├── active: usize
  └── compact chip strip rendered when len > 1 or freshly split
        chip click → switch active tab
        '+' button / NewTabInPane (Cmd+Shift+T) → append new shell tab
```

**Cmd+W cascade:** per-pane tab → leaf → group tab (innermost wins).

**Persistence:** `PersistedSubPane.tabs` (Vec of persisted tab blobs); serde-default for existing single-tab leaves; no SQL migration required.

**Known v1 limits:** per-pane-tab relay reattach deferred (multi-tab leaves restore dormant); agent CLI PTYs not yet threaded with context env; cross-group multi-tab-drag repaint deferred.

---

## Post-paint PTY attach on restore

Restored terminal tabs no longer gate first paint behind daemon round-trips. The window-open path mounts every restored tab as a **pending** view over an in-process dormant grid (`spawn_pending_placeholder_grid` — never touches the relay), paints, then a detached reconcile task swaps live sessions in:

```
set_active_project (pre-paint, zero relay RPCs)
  └── build_project_panes → Vec<PendingAttach>     ← TerminalView::mount_pending per tab
        └── spawn_attach_reconcile (cx.spawn_in, detached)
              1. relay_state_snapshot()             ← ONE ListPtys, background executor
              2. compute_attach_hints / compute_leaf_attach_hints (unchanged liveness gate)
              3. per tab: attach_pty_existing(hint) | spawn_local_pty(cwd, env)
                   ← each RPC on the background executor (blocking Handle::block_on)
              4. TerminalView::adopt_live_session    ← main-thread delivery, per tab
```

**Invariants:**
- Every blocking relay call (`Handle::block_on` against the relay runtime) runs on the background executor; only `adopt_live_session` delivery touches the main thread.
- Input to a pending view is dropped quietly; `external_id()` reports `None` while pending (tear-off stays disabled) but `relay_id_for_capture()` answers with the persisted hint so a quit-save racing the reconcile keeps the row.
- Undeliverable sessions (tab closed mid-reconcile): re-attached PTYs are **detached** (daemon keeps them), fresh spawns are **closed**; both skipped under `APP_QUITTING`.
- The boot relay supervisor handshake stays intentionally blocking pre-paint (warm path ≈ 1 ms; the daemon survives quit, so cold spawns are reboot-only).
- Restored panes paint blank until the daemon's raw-byte replay arrives — serialized-grid replay was deliberately removed (reflow scrambles full-screen TUIs).

Boot timing is logged on every launch: `boot: db open + state hydrate`, `boot: relay supervisor handshake`, `project panes built (pre-paint, no relay RPCs)`, `post-paint pty attach reconcile done`.

### Disk scrollback checkpoints (cold restore after daemon death)

The relay daemon checkpoints each PTY's replay ring to `<runtime dir>/checkpoints/<pty_id>/{meta.json,scrollback.bin}` every 5 s (atomic tmp+rename, skipped when `bytes_out` hasn't moved) and **removes** the checkpoint on every clean end — deliberate `Close` and natural child exit. Whatever remains on disk is therefore an unclean death (daemon crash, SIGKILL, host reboot).

The wire protocol is untouched: the app reads checkpoints straight off disk. When the reconcile's warm attach fails and a slot cold-spawns, the background executor looks up the raw persisted hint's checkpoint and, after `adopt_live_session`, prefills the pane grid with: clear screen → alt-screen-truncated scrollback tail (≤ 512 KiB, advanced to the next line boundary when the cap cut mid-line) → dim `--- session restored ---` marker → terminal mode reset (`relay_cold_restore.rs`). A checkpoint with no replayable scrollback (alt-screen-only session, or a crash before the first tick) still restores the recovered cwd — blank grid, no marker. The checkpoint is deleted only after successful delivery, so a quit mid-reconcile can still restore on the next launch; orphans fall to the daemon's 7-day boot GC.

Right after the reconcile loop, the app re-captures `pane_relay_ids` for the window (reusing the snapshot's session id — no extra daemon round-trip), so cold-spawned PTYs' fresh ids hit SQLite immediately instead of waiting for the next quit/switch capture. Without this, an app crash after a reconcile would leave the table pointing at dead ids and strand the boot's live PTYs.

The replacement shell spawns at the checkpoint's recorded cols/rows (initial dims only — the pane's normal resize takes over after adopt), so its first prompt wraps for the same width as the restored content above it.

Two further resilience layers (same hardening pass): **daemon respawn retries** — `respawn_relay_after_death` rides out transient spawn failures with bounded exponential backoff (5 attempts, 500 ms → 8 s cap; version mismatch still exits immediately since another build's daemon may own the socket) — and a **15 s layout autosave** on each `WorkspaceRoot` that captures all projects' layouts plus relay ids (cached handshake session id, zero wire RPCs), bounding what an app crash can lose of mid-session tabs/splits to one tick. Idle ticks cost nothing: `save_persisted_tabs` skips byte-identical JSON via a content-hash gate recorded only after a successful write.

Each checkpoint also refreshes `meta.cwd` with the shell child's **live working directory**, resolved kernel-side from the child pid (`oximux-proc-cwd`, the shared `proc_pidinfo`/`PROC_PIDVNODEPATHINFO` resolver also used for split-pane cwd inheritance) — no OSC 7 cooperation from the shell required. The cold-spawn path revives the replacement shell at that recovered cwd (validated to still exist; falls back to the persisted layout cwd), so a crash puts the user back in the directory they were actually in.

The same meta carries the shell child's **OS pid** (seeded at spawn). Because the daemon and its children run on the same host as the app, `TerminalView::os_pid` falls back to reading it for daemon-backed sessions — giving splits from relay panes (and layout-snapshot cwd capture) the same kernel-true cwd inheritance as in-process panes, again with zero wire-protocol involvement.

This path is distinct from routine restore, which stays replay-free (the no-grid-replay invariant above is unchanged): cold restore trades reflow perfection for not losing the scrollback entirely, and the marker makes clear the content is history, not live state. `prefill_grid` ends with `clear_collected`, so query auto-replies recorded in the crashed session can't reach the new shell's stdin.

### Agent resume on cold restore

No process survives a reboot; what differs is what gets resumed. A cockpit agent tab respawned with `SessionResumption::None` would be a brand-new conversation under the old title, so restore carries one field end to end: the agent's **own** session id (what `claude --resume <id>` / `codex resume <id>` / `pi --session <id>` / `omp --resume <id>` take), distinct from the relay PTY id and from OxiMux's `AgentSessionId`.

```
CLI hook stdin {session_id,…} ──oximux agent-status──▶ OSC 9999 {"v":1,"state":..,"session_id":..}
  (agent_hook_dialects::session_id; Pi/omp extension reads ctx.sessionManager.getSessionId())
  ──▶ AgentOscScanner ──▶ poll_helpers: carried across every snapshot like `last_message`
  ──▶ AgentSnapshot.detail.session_id on the tab's status_rx
  ──▶ snapshot: PersistedAgentTab.provider_session (validated [A-Za-z0-9_.-]{1,64}; serde-default)
        ── reboot / relay death ──
  ──▶ restore_agent_tab: warm re-attach unchanged; cold spawn uses
        agent_resume::restore_resumption(adapter, id) → Resume{id} | None
```

**Three markers, one style** (`relay_cold_restore::RestoreMarker`, dim, CRLF-framed): `--- session restored ---` (plain terminal scrollback, byte-identical to before), `--- resuming previous session ---` (cockpit tab spawned on the persisted id), `--- previous session unavailable, started fresh ---` (no id, `Custom` adapter, or fallback). A warm re-attach prints nothing. The marker is prefilled inside the same update closure that mounts the tab, as early as the mount allows (the grid takes bytes as the backend pumps them; a CLI's first frame is normally hundreds of milliseconds out). Some CLIs erase it on start-up (omp wipes screen and scrollback with its first frame, Pi on every resize, Claude Code erases every visible line when a background tab is first sized), and writing it back into a screen the CLI owns would be torn by its next repaint. So the pane also arms an off-grid notice with the same words (`terminal_view::restore_notice`): a resize or scrollback wipe (every measured erase follows one) schedules a single search of the whole buffer for the dim, framed marker once the repaint settles; if it is gone the words show as a strip over the pane until the user's first input. The notice retires 60 s after its first such event, and plain output never triggers a search.

**Fallback rule** (`agent_resume::should_fallback`, `forward_until_verdict`): a resumed CLI's non-zero exit (`Failed(_)` / `Done{code≠0}`) counts as a refused id when it comes **before the CLI has reported any hook event** (a prompt, a tool or a reply — output alone never counts), however long that takes: a CLI can paint a startup menu (Codex's update notice, a trust prompt) before it reads the id and exit only once that menu is dismissed. The runtime then cancels it, spawns once with `None`, and `PaneGroup::replace_agent_session` swaps the session behind the same tab (slot, label, colour, pin kept; `TerminalView::replace_live_session` re-arms the drain) under the "started fresh" marker. Silence never triggers, a clean exit is the user quitting, and after the first hook every exit is the user's to see. A 10-minute ceiling (`RESUME_VERDICT_CEILING`) closes the window for a CLI that never reports (status hooks off). The trade-off: a non-zero exit the user chose before the first hook (quitting at a startup menu with an error code) also respawns fresh once, and with hooks off so does a genuine crash within those 10 minutes. If the fallback spawn itself fails, the withheld exit is published after all, so the tab and row read failed. Every reader of a cold-resumed tab — the tab chip, its watcher (toast + OS banner) and the rail row — reads a **proxy** of the session's status stream; the watchdog forwards into it and holds back only the refusing exit, so a refused resume never reads as "failed" anywhere, and the fallback carries on the same proxy (same `agent_sessions` row, no duplicate). The row claim (`spawn_for_session(.., restoring = true)`) waits up to 5 s (`ROW_CLAIM_GRACE`) so a quick refusal lets the fresh session claim it directly; a later one repoints the claimed row (`WorkspaceRoot::repoint_live_agent`). Codex resumes with `resume`, never `fork`.

**Hand-typed agents in a plain terminal** have no cockpit tab, but the per-PTY ambient record (`ambient_agent:<pty_id>`) carries the process-scan label and the hooks' `session_id`. The record is readable for the rail for 30 min (unchanged) and for a resume offer for **7 days** (`ambient_state::load_for_resume`, matching checkpoint GC). On a cold spawn whose dead PTY had such a record, the pane prefills a dim hint under the restored marker and pre-types the resume line (`agent_resume::resume_shell_line`) at the fresh prompt via `TerminalView::queue_input_on_first_output` — delivered once the shell has produced output and then stayed quiet for 400 ms (so it lands at the prompt, not inside rc-file chatter), or after 3 s for a shell that prints nothing; never with a newline; dropped by any earlier keystroke. The record is dropped when the process scan sees the agent leave the shell, and the hook reading is reset when the scan sees a different agent, so a label and an id from two different agents are never paired. The record is forgotten once the offer is delivered, like the checkpoint.

Every restored agent tab logs `agent restore` with `warm`, `resumed` and `fallback` flags. Resume does not depend on the relay's shutdown: the id lives in the app's persisted layout, not in a relay checkpoint. The relay's final checkpoint pass on SIGTERM (`server.rs`, best-effort) only freshens the scrollback shown above the marker; a relay killed before it runs still restores from the last 5 s periodic checkpoint.

---

## Shell context env — SurfaceIds (mux-P4)

Every spawned terminal receives an `OXIMUX_*` env block minted at spawn time:

| Variable | Value |
|---|---|
| `OXIMUX_WORKSPACE_ID` | project root path |
| `OXIMUX_SURFACE_ID` | UUID minted per pane leaf |
| `OXIMUX_TAB_ID` | UUID minted per tab within leaf |
| `OXIMUX_SOCKET_PATH` | relay Unix socket path |
| `OXIMUX_PTY_ID` | injected by relay daemon at fork |

`SurfaceIds` (`shell/context_env.rs`) builds the env list. Ids persist in the per-pane layout blob (serde-default) and are re-injected on dormant respawn. Agent CLI PTYs excluded until a follow-on.

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

## Left-rail drag-to-reorder

Project groups and workspace rows support drag reorder. The interaction follows Zed's stateless GPUI idiom — no drag entity; state lives in the payload type.

```
on_drag(ProjectDragPayload { id, original_index })
  │
  drag_over(target row)
    insertion_side(pointer_y, row_bounds) → Above | Below
    paint_insertion_line(bounds, side, cx)   ← 2px accent, full-width
  │
on_drop
  reorder_slot_value(neighbors) → f64   ← midpoint between neighbors
  ProjectRepo::reorder_to(id, index)
    or WorkspaceRepo::reorder_to_target(id, neighbor_id, side)
    ↳ normalize_ranks() if float precision exhausted
```

**Sort order model**: `projects.sort_order REAL` (V014) and `workspaces.sort_order REAL` (V015); sparse-float (gaps of 1.0 between rows), narrowed by repeated insertions, renormalized to integer spacing when gap < f64::EPSILON. `ProjectRepo::list_ordered` replaces the old recency (`last_opened_at DESC`) query — project list is now **manual-sticky**; opening a project does not reorder it.

**Escape cancel**: `DismissOverlay` handler calls `cx.stop_active_drag` as its first branch, cancelling any in-flight drag before closing overlays.

**Constraints**: drop indicator is full-width only (GPUI `drag_over` is style-only; pixel-inset requires a separate overlay entity, deferred). Workspace drag disabled outside `Manual` sort mode; primary workspace row not draggable.

---

## Agent Chat adapters (structured chat view)

Separate from the raw-PTY terminal runtime above, the **chat** view runs a structured conversation model (`crates/agents/src/thread/`). Three provider adapters each decode their own wire protocol into one transport-agnostic vocabulary — `ThreadEvent` (`event.rs`) — which `ChatThread::apply` (`state.rs`) folds into a `Vec<ThreadEntry>` the view renders. Each adapter satisfies the `AgentConnection` trait (`connection.rs`); the factory in `connect.rs` picks one. The view never learns which backend produced an event.

**Agent-core split (2026-07-18):** the pure fold + `ThreadEvent` vocabulary + stream-json decoder now live in `crates/agent-core` (`oximux-agent-core`) — serde/serde_json/tracing only, no pty/rusqlite/ACP/gpui/tokio — so the same fold can cross-compile for a mobile Rust core; `oximux-agents` re-exports the modules under their original `crate::thread::*` paths. Groundwork for this: `crates/agents/src/session_registry.rs` adds a process-wide, gpui-free `SessionRegistry` (event bus + off-thread command surface, keyed by `session_id`) that a future remote/network layer can subscribe to and command without a `gpui::Context`; it is built but not yet wired into the view. It required `AgentConnection` to gain a `Sync` supertrait, and the agent-chat view now holds its connection as `Arc<dyn AgentConnection>` (was `Box`) so the registry can share the same connection object the view drives — pure ownership change, no behavior change. This is Phase 1-2 groundwork for the Remote Control feature (control OxiMux from a phone over Iroh P2P; plan at `plans/260717-2037-oximux-remote-control/`) — no remote transport exists yet.

**Wire vocabulary (`crates/remote-proto`, `oximux-remote-proto`, 2026-07-18):** the transport-free RPC surface shared by the future desktop host and the phone's Rust core — an append-only postcard `Request`/`Response` envelope (`PROTOCOL_VERSION = 1` at this writing; v23 as of 2026-09-04 — the pin test in `remote-proto/src/proto/tests.rs` names each version's wire change, and events the peer's declared version predates are downgraded to a `Notice` in the same seq rather than skipped), the `HostEvent` stream frame, a `PairingTicket` codec (`oximux://connect?ticket=` deep link, base64url-encoded postcard, `handshake_secret` redacted in `Debug`), and a transport-agnostic `Transport` trait (framed bidirectional seam; iroh will be one impl, an in-memory loopback drives tests today). `async-trait` is the crate's only async dependency — no tokio, no gpui — so it stays mobile-portable. postcard is non-self-describing and can't deserialize `serde_json::Value`, but `ThreadEvent` and `PermissionDecision` carry `Value` fields (and are already on the persisted-JSON path), so rather than fork a shadow event type those payloads ride the wire as a `serde_json` string nested inside the postcard envelope (`HostEvent.event_json`, `ResolvePermissionReq.decision_json`) — no on-disk format change. This required additive `Serialize`/`Deserialize` derives on the reachable agent-core event types (`ThreadEvent`, `TurnUsage`, `AuthMethodInfo`, `AuthMethodKind`, `PlanEntryLite`, `PermissionDecision`). No host, client, or transport impl consumed this crate at this writing; today `remote-host` (both hosts), `remote-session` (mobile core), `remote-iroh`, and the `oximux` CLI all speak it — see the crate table above.

- **Claude** — hand-parsed `stream-json` (`stream_json.rs`); no official Rust SDK, so the taxonomy is tracked manually. Native surface (AskUserQuestion, effort/modes, background tasks) kept — not wrapped as ACP.
- **Codex** — `codex app-server` JSON-RPC v2 (`codex/`); shapes verified via `generate-json-schema`.
- **ACP** — `agent-client-protocol` 1.2 crate (`acp/`); the generic tail for other agents (Cursor, Amp, …).
- **Pi family** — pi's newline-JSON `--mode rpc` (`pi/`) and omp's `--mode rpc-ui` (`omp/`, `Transport::OmpRpc`) share an untyped NDJSON core (`ndjson_transport.rs` — LF-only framing, id correlation, stderr fold, EOF drain) and the snapshot→delta suffix-diff helpers (`snapshot_diff.rs`); each keeps its own typed command/event layer. Neither joins the coverage matrix below — their event taxonomy maps onto the same `ThreadEvent` vocabulary the matrix already describes.

**Adapter coverage matrix** (✓ handled · — n/a for that protocol · ○ not yet):

| Capability | Claude | Codex | ACP |
|---|---|---|---|
| assistant text / thinking (+ deltas) | ✓ | ✓ | ✓ |
| tool call + result | ✓ | ✓ | ✓ |
| rich tool card by kind | ✓ (by name) | ✓ (renamed→name) | ✓ (via `ToolKind`) |
| server/MCP tool calls | ✓ (as `tool_use`) | ✓ | ✓ |
| 6× `*_tool_result` variants | ✓ (fwd-compat) | — | — |
| inline tool-result image | ✓ | ○ | ✓ |
| message resource-link / image | — | — | ✓ (link→md, image→placeholder) |
| slash-command descriptions + arg hints | ○ | — | ✓ (`available_commands_update` desc + `input` hint) |
| live tool-output streaming | ○ | ✓ (`commandExecution/outputDelta` only — `fileChange` emits none, verified on 0.144.3) | ○ |
| plan panel | ✓ (`TodoWrite`) | ✓ (`turn/plan/updated`) | ✓ (`Plan`) |
| clarifying question card | ✓ (`AskUserQuestion`) | ✓ (`requestUserInput`) | ○ |
| plan-approval card | ✓ (`ExitPlanMode` → dedicated 3-way card) | — | — |
| permission card | ✓ (`PermissionKind`-tagged) | ✓ (+ MCP elicitation card) | ✓ (+ allow-kind option pills) |
| live tool-input streaming | ✓ (`input_json_delta`) | ○ | ○ |
| subagent activity → parent tool card | ✓ (sidechain `parent_tool_use_id`) | ✓ (collab child-thread ids, buffered+replayed) | — |
| conversation rewind | ✓ (disk session-file fork) | ✓ (server-side `thread/fork`) | — |
| stop-reason surfacing | ✓ | ✓ | ✓ (`Refusal`/`MaxTokens`/`MaxTurnRequests` → banner) |
| cancel resolves pending permission | ✓ | — | ✓ (drain → `Cancelled` outcome) |
| image prompt input | ✓ | — | ✓ (gated on `prompt_capabilities.image`) |
| reasoning-effort picker | ✓ (`--effort`, respawn) | ✓ (respawn) | ✓ (`ThoughtLevel` config, in-session) |
| permission-mode switch (in-place) | ✓ (`set_permission_mode` control_request, no respawn) | — (posture selects instead) | ✓ (`session/set_mode`) |
| approval-policy / sandbox picker | — | ✓ (2 composer `FeatureControl` selects; per-turn override, no respawn) | — |
| reopen session as chat (import) | ✓ (on-disk `~/.claude` log) | ✓ (`~/.codex` rollout → transcript) | — (no list API) |
| session restore | ✓ (`--resume`) | ✓ (`--resume`) | ✓ (`session/load` + replay-suppress; else fresh + notice) |
| auth flow | — (own `/login`) | ✓ (`AuthRequired` → structured card → ChatGPT OAuth `account/login/start` → browser → auto-retry) | ✓ (`AuthRequired` → method card → `authenticate`, same conn) |
| compaction divider | ✓ (`compact_boundary`) | ✓ (`thread/compacted`) | — |
| compacting-in-progress indicator | ✓ (`system/status` `compacting` → spinner, cleared by boundary/turn-end) | — | — |
| usage footer | ✓ | ✓ | ○ |
| embedded terminal | — | — | ✓ (`terminal/*` → inline `TerminalView`) |

**View-level chat features** (provider-agnostic, not adapter rows): **attention notifications** — a turn finishing / erroring / needing a permission while the app is unfocused posts a macOS `UNUserNotificationCenter` banner + dock-badge (`notifier/`), coalesced per-tab and cleared on focus; individually toggleable in Settings → Notifications. **Tool-payload fullscreen sheet** — a `⤢` on any tool card with a substantial payload (a diff, or a result over 600 chars) opens a full-height overlay (`agent_chat/tool_sheet.rs`) showing the whole payload: a large diff virtualized via `uniform_list` (fixed `h_row` rows), or a long shell/read/fetch body via the shared inline renderer with its row/char caps lifted (a `full: bool` size-mode threaded through `tool_bodies.rs`), with a Copy button and Esc / backdrop / ✕ dismiss. The sheet reads its tool call live by id each render, so a still-running tool grows in place.

The `ThreadEvent` seam is the normalization point: e.g. Claude `compact_boundary`, Codex `thread/compacted`, and the import path all emit `CompactBoundary`; Claude `TodoWrite`, Codex `turn/plan/updated`, and ACP `Plan` all emit `PlanUpdated`; Claude and ACP tool-result images both emit `ToolResultImages`. Tool cards likewise normalize through one classifier: `ToolDetail::classify` (`tool_detail.rs`) maps a Claude name, a Codex renamed name, or an ACP `ToolKind` into a single archetype (`Shell`/`Read`/`Edit`/`Search`/`Fetch`/…) the renderer switches on, so an ACP `Execute` renders the same shell card as a Claude `Bash` instead of a generic key:value fallback. ACP threads its kind to the card via a follow-up `ToolKind` event (Claude/Codex classify by name, leaving `ToolCall.kind` unset). ACP embedded terminals invert the crate boundary: the domain-layer `terminal/*` handlers delegate to an app-installed `AcpTerminalHost` that owns the real PTY (relay/in-process) and the inline `TerminalView`. The round-2 correctness pass (`plans/260710-2327-acp-round2-correctness-ux/`) closed the remaining client-side gaps: turn stop-reasons now surface as error banners; a Stop mid-permission drains the parked responder so the agent gets a `Cancelled` outcome (no wedge); attached images ride the prompt when the agent advertises `prompt_capabilities.image`; a restored tab resumes via `session/load` (with a `replaying` gate that drops the agent's history replay since OxiMux repaints its own persisted blob) and falls back to a fresh session with a visible notice when the agent lacks `loadSession`; a logged-out agent (`AuthRequired`/-32000) renders an auth-method card (Agent pill / Terminal inline login / EnvVar secret form) whose `authenticate` retries the session; and permission requests surface the agent's extra allow-kind options as pills. The `agent-client-protocol` dep enables the `unstable_auth_methods` feature for the env-var/terminal `AuthMethod` variants (schema 1.4.0 gates them). The Agent/Terminal methods authenticate on the same connection; the **EnvVar** method takes masked secret values in the card and **respawns** the agent with them in its environment (via `spawn_with_env` → `AcpAgent::from_args`, so the credentials ride the child's env, not argv, and never reach the persisted transcript), then auto-authenticates the seeded method — the only sign-in that works when the credential is delivered by environment. Deferred: a usage footer.

**Round-4 P0 parity pass** (`crates/agents/src/thread/stream_json.rs`, `event.rs`, `tool_call.rs`, `state.rs`; `codex/{protocol.rs,mod.rs,map.rs,approvals.rs}`; `app/src/shell/agent_chat/{plan_approval_card.rs,tool_card.rs,rewind_menu.rs}`):

- **Claude live tool-input streaming.** `stream_json.rs` now decodes `content_block_delta` events of type `input_json_delta`, accumulating `partial_json` fragments onto the tool card opened by the matching `content_block_start` — the card's argument view fills in live, ahead of the finalized `tool_use` block that used to be the only source. A `PermissionKind` enum (`tool_call.rs`) now tags every permission request as `Tool` / `Plan` / `Mode` / `Mcp` / `Other`, replacing the single undifferentiated permission shape and letting the card router pick a renderer per kind.
- **Claude plan-approval card.** Claude's `ExitPlanMode` rides the same `can_use_tool` control channel as any tool call, so the decoder tags it `PermissionKind::Plan` (its `description` carries the plan markdown). `plan_approval_card.rs` renders it as a dedicated card — plan body via the assistant markdown renderer, plus the CLI's own three choices (approve → `acceptEdits` mode, approve → `default` mode, or keep planning/deny) — instead of the generic key:value permission card. Approve echoes the request's own input plus a `setMode` suggestion so the CLI flips session permission mode and continues the turn; the composer's mode chip updates optimistically since Claude sends no mode echo.
- **Codex server-side rewind.** `AgentCapabilities::supports_rewind` is now `true` for Codex. Rewind forks the *live* connection via `thread/fork` (`codex/protocol.rs`), addressed by `lastTurnId` from an in-session turn ledger mapping user-message ordinals to turn ids (`codex/map.rs`) — no on-disk session log involved, unlike Claude's file-fork path. `thread/rollback` is the deprecated upstream alternative and is deliberately not used. `AgentConnection::rewind_is_server_side()` lets the shared `rewind_menu.rs` UI branch between Claude's disk-fork-then-respawn flow and Codex's fork-on-current-connection-then-respawn flow without duplicating the confirm/restore UI.
- **Codex MCP elicitation.** `mcpServer/elicitation/request` (`codex/approvals.rs`) now surfaces as a permission card — tagged `PermissionKind::Mcp`, labeled "MCP · \<server\>" — instead of being auto-declined. Because an elicitation's reply uses a different JSON-RPC shape (`{action}`) than a tool approval, pending elicitations are tracked in a separate `pending_elicitations` map (`codex/mod.rs`) so `resolve_permission` can route the reply through `to_codex_elicitation` rather than the tool-approval encoder.
- **Codex approval-policy + sandbox controls.** Two composer `FeatureControl` selects (`codex_approval_policy`, `codex_sandbox`; `codex/mod.rs`) expose Codex's posture directly — previously owned entirely by `~/.codex/config.toml`. The chosen posture seeds `thread/start`/`thread/resume` and is re-sent as a per-turn override on every `turn/start`, so switching either select mid-session takes effect on the next send with no respawn; the posture also persists on the session struct across turns.
- **Fork-to-new-tab stays Claude-only.** "Fork from here" (`agent_chat/mod.rs`) reads the on-disk `~/.claude` session log to write a new file cut at the target message. Codex has no equivalent on-disk log — its rewind is connection-only — so the action is gated off (`!fork_to_tab_server_side`) and hidden for Codex threads.

---

## Round-6 close-out (`plans/260714-0248-agent-chat-round6-closeout-worktree/`)

Three independent closeouts, verified against a fresh bundled build of HEAD:

- **Signed-bundle notifications.** `scripts/bundle-macos.sh` gained an opt-in
  `--sign <identity>` flag / `OXIMUX_CODESIGN_IDENTITY` env (ad-hoc default
  unchanged, `verify_signature` fails the build if the requested identity
  doesn't show in the sealed bundle's Authority chain). Ad-hoc-signed bundles
  silently drop the `UNUserNotificationCenter` one-time authorization grant on
  some macOS versions, so round-5's attention banners never appeared even
  after accepting the permission prompt; a real identity (or a local
  self-signed codesigning cert) gives the grant a stable identity to stick to.
- **Import transcript preview mappers.** `session_log/import_transcript_{opencode,pi}.rs`
  map OpenCode's SQLite `message`/`part` rows and Pi's JSONL `message` lines
  into the same `Vec<ThreadEntry>` shape Claude/Codex chat imports build,
  dispatched by `load_import_provider_transcript` (`import_provider_index.rs`).
  Picker preview blurbs for all five import providers (including these two)
  already shipped in round-5 (`99b4650`); these new mappers produce the
  *full* chat transcript but were **not yet wired into open-as-chat** at the
  time — both still resumed as a terminal PTY. **Wired in round-7** below:
  `⌘↵` on an OpenCode/Pi row now opens a transcript-only chat tab seeded from
  these mappers.
- **Worktree-per-agent** (reference-tool parity). The New Agent composer's
  worktree toggle (`agent_chat/roster.rs`) created a *git-only* worktree
  (`Repository::add_worktree`, no `WorkspaceRepo` write — the agent-chat view
  has no storage-layer handle) before the first send, rebinding the chat's
  `cwd` and tab label to it. Inline failure banner offers Retry or "continue
  without a worktree"; hidden for non-git projects. **Superseded in round-7**
  below: the git-only helper was retired in favor of routing up to a real
  `Workspace` insert, so the worktree agent gets a sidebar card + `⌘J` entry.

---

## Round-7 close-out (`plans/260714-1020-agent-chat-round7-worktree-workspace-import-wiring/`)

Closes the two round-6 follow-ups above. Phases 1–2 code-complete + tested
(`oximux-app --lib` 1254/0); Phase 3 (signed-bundle notifications, Codex
OAuth, live MCP elicitation, live GUI walk-through) stays user-gated.

- **New-Agent worktree → first-class `Workspace`.** `ChatWorktreeOutcome`
  (`workspace_ops.rs`) now maps down from the richer `CreateOutcome` returned
  by the existing `create_workspace_with_rollback` — the same git-worktree
  **+** DB `Workspace`-row-insert (with full rollback) the create dialog
  uses — instead of standing up a git-only path with no DB row. The route-up chain: `roster.rs` emits
  `AgentChatEvent::WorktreeWorkspaceRequested{slug}` → `pane_group/tabs.rs`
  dispatches the new `CreateWorktreeWorkspaceForActiveChat{slug}` action →
  `workspace_root/render.rs`'s handler resolves the active chat view (new
  `active_agent_chat_view` accessors on `pane_group/state.rs` +
  `project_panes/state.rs`, captured **synchronously** before the async create
  so a tab switch mid-create can't misroute the callback), runs
  `create_workspace_with_rollback`, calls `mark_rail_dirty`, and hands the
  outcome back through `AgentChatView::on_worktree_create_outcome` (rebinds
  `cwd`, resumes the staged send). The row is enumerable by the sidebar's
  `list_for_project` gather, so the worktree agent now gets a sidebar card +
  `⌘J` entry and is removable via the Worktree panel — the git-only
  `create_agent_chat_worktree` helper (and its tests) was retired; coverage
  moved to `workspace_create_rollback.rs`, which gained an enumerability
  assertion.
- **OpenCode/Pi open as a chat (transcript bridge).** `⌘↵` on an OpenCode/Pi
  row in the `⌘⇧H` picker (`entry_opens_as_chat` now also matches
  `preset_id` `opencode`/`pi`) builds a chat tab via
  `pane_group/tabs.rs::open_import_bridge_chat`: `AgentChatView::new_import_bridge`
  (new `ImportBridge` struct + field) seeds the transcript through the round-6
  `load_import_provider_transcript` dispatcher with `connect_now: false` — no
  live connection, `send_text` guarded to a no-op — and the composer is
  swapped for a `render_import_bridge_footer` "Resume in terminal" action that
  re-dispatches the existing `ResumeAgentSession` PTY resume via
  `import_resume_command`. `OpenChatSession`/`entry_opens_as_chat` gained a
  `preset_id`. Default `↵` / click is unchanged — still resumes in a terminal;
  the bridge is only reachable via the explicit force-open. Bridge tabs carry
  no live session id, so they are excluded from tab persistence (a
  resumed-chat restore would spawn a live subprocess and drop the imported
  history); they re-open from Session History instead, deduped on
  `(preset_id, session_id)` (`import_bridge_key`). Copilot stays resume-only —
  no transcript mapper wired.

---

## Round-8 close-out — render fidelity

Closes rendering gaps against native agent UIs. All work sits inside the
existing `ThreadEvent`/`ToolDetail` seams — no adapter-boundary or
`ChatThread` architecture change.

**Shipped:**

- **Codex `apply_patch` diff card** (new `agent_chat/apply_patch.rs`, split
  out of `diff_card.rs` to keep it reviewable). Codex reports an edit as an
  `apply_patch` tool call carrying a `changes` array of per-file patches — a
  different shape from Claude's `Edit` (old/new strings) and ACP's normalized
  diff — but the module converts it into the same `DiffLine` stream the diff
  card already renders, so all three providers converge on one visual instead
  of Codex showing raw JSON with the patch duplicated into the result body.
- **Batched repaint on stream deltas.** Delta events (`input_json_delta`,
  `outputDelta`, text deltas) coalesce into a single repaint tick instead of
  one repaint per delta; non-delta events still repaint immediately. Keeps
  long turns smooth.
- **Bash command unwrap reads the structured `command` field**, not token
  parsing — the wire flips quote style between turns, which made token
  parsing unreliable.
- **Diff-row syntax highlighting, memoized per `(path, line)`.** The memo is
  load-bearing, not an optimization nicety: the transcript is not
  virtualized, so an expanded diff card would otherwise re-tokenize via
  syntect on every unrelated repaint, at the new ~20Hz batching rate above.
- **Session-detail popover** (new `agent_chat/session_detail.rs`): a
  read-only view of what the session advertised at `system/init` — model,
  cwd, tools, MCP servers + connect status, subagent types — cached with the
  transcript (`SessionMeta`) so a restored chat answers before its resumed
  process speaks. Backends differ in what they advertise (Claude's
  `system/init` carries all of it; Codex and ACP carry none today), so the
  trigger hides entirely rather than opening onto an empty panel.
- **Auto-declined server requests** log a muted divider, deduped so a
  repeated decline doesn't stack identical rows.
- **Subagent log fidelity:** failed child tool results and thinking lines are
  now captured into the parent's `subagent_log`; a chatty subagent's full log
  is reachable via the tool sheet. A subagent `tool_result` block carries
  `{tool_use_id, type, content, is_error}` and never the tool's `name` (the
  name only appears on the earlier `tool_use` block), so a failed result is
  logged by its error text rather than a "\<tool\> failed" title; successful
  results stay unlogged since the `tool_use` line already recorded the
  action.

**Withdrawn — live-probed, premise disproved; do not re-attempt without new
upstream evidence:**

- **Codex `fileChange` output deltas.** `item/fileChange/outputDelta` does
  not exist on Codex 0.144.3 — live-probed twice against 300- and 400-line
  patches, `fileChange` goes `started` → `completed` with zero deltas. The
  sibling `item/commandExecution/outputDelta` does stream (`{itemId, delta}`,
  see the adapter matrix above), which is what made the field name look
  plausible.
- **Claude live status line.** `system/status` never fires on Claude Code
  2.x — probed twice, only `hook_started`, `hook_response`, `init`, and
  `post_turn_summary` appear. The closest real signal is `post_turn_summary`
  (`status_category` + `status_detail`), already decoded into
  `ChatThread.last_summary`, but it lands after the turn, not during it — a
  different feature (a status chip fed by `last_summary`), not a substitute
  for a live line.
- **Context-meter catalog seed.** The model catalog carries no context
  windows — `ModelChoice` is `{wire, label, description}`
  (`crates/agents/src/thread/connection.rs:56`) and the disk-persisted
  `session_restore/catalog_cache.rs` stores no window field. A hardcoded
  model→window table would go stale as models ship and would be
  authoritatively corrected by the server one turn later anyway. The meter's
  real seed already exists and is unaffected: `ChatThread.last_known_context_window`
  is stashed from both settled `TurnUsage` (`state.rs:449`) and `LiveUsage`
  (`state.rs:551`).

**Round-9 backlog surfaced by this round:** persist
`last_known_context_window` into `PersistedChatTranscript` (currently
in-memory only, so a restored chat loses its context-bar denominator until
its next turn settles — one `#[serde(default)]` field); render the
`post_turn_summary` status chip `last_summary` already stores; subagent
detach-to-tab.

**Verification:** the apply_patch diff card, batched-repaint streaming, the
auto-declined divider, and subagent log capture were live-verified driving a
real running app. The session-detail popover is unit-verified only — it
renders for Claude alone, and Claude was unauthenticated under the sandboxed
`HOME` used to run a dev build alongside the authoring session.

---

## External tool provisioning

Two independent zero-setup paths so a fresh install needs no manual CLI installs.

**Bundled ripgrep (build-time).** `scripts/fetch-ripgrep.sh` downloads a
pinned ripgrep release (15.2.0), sha256-verifies it against the release's own
`.sha256` asset, arch-matches the app build (lipo for a universal build), and
caches via a stamp file (offline rebuilds stay a no-op). `bundle-macos.sh`
copies it to `OxiMux.app/Contents/MacOS/rg` and signs it in the nested-first
codesign block, before the bundle seal. Runtime resolution is
`tool_paths::rg_program()` (`apps/desktop/src/shell/tool_paths.rs`): the
bundled sibling of the running binary first, bare `rg` (PATH lookup) as the
dev-build fallback. All 3 rg call sites — `search_panel::rg_runner`,
`search_panel::header_render::detect_rg_available`, and Quick Open's
`command_palette::file_index::scan_files` — resolve through it. Missing-rg
hints were reworded to "(dev build?) — `brew install ripgrep`" since a
bundled app build always has it.

**Driver install pipeline (runtime, user-triggered).**
`crates/computer-use/src/install/` — one-click install/update for the
`cua-driver` computer-use daemon, driven from the settings Computer-use pane
and a conditional onboarding step (`driver_step.rs`, skipped when already
`Ready`). One install per process (`INSTALLING` atomic); a second surface
observes the running install's pull-style `status()` instead of racing it.

```
release_feed.rs  GET the trycua releases feed, filter tag prefix
                  `cua-driver-rs-v`, skip drafts; prerelease flag ignored
                  (upstream flags every driver release prerelease)
pipeline.rs       ureq download w/ connect+read timeouts + a byte ceiling,
                  sha256 vs checksums.txt (transport integrity only —
                  not the trust gate), /usr/bin/tar extract, downgrade
                  guard vs the currently-installed version
verify.rs         gate: codesign --verify --strict (binary intact) →
                  Identifier + TeamIdentifier match (publisher pin) →
                  bounded `--version` read → verify_notarized_bundle
                  (spctl --assess --type execute on the bundle; ENFORCED
                  for installs — a browser download gets Gatekeeper's
                  notarization check for free via its quarantine xattr,
                  a programmatic download carries none)
place.rs          renamex_np(RENAME_SWAP) crash-safe swap (no empty-
                  instant at the target path); two-rename .bak fallback
                  for bundles where the kernel refuses RENAME_SWAP; the
                  swapped-in bundle is re-verified before the old one is
                  discarded — a failed re-verify rolls back
```

Gate order matters: the publisher-identity pins run before the untrusted
binary executes at all beyond a bounded `--version`, and the bundle-level
`spctl` notarization assessment runs last among the pre-install gates
because it needs the extracted `.app`, not just the inner binary. The
`cua-driver` daemon itself is never stopped or restarted by an install — it's
a machine-wide singleton other MCP clients share — so a running daemon keeps
serving the old version until it next respawns; the UI surfaces that after
an upgrade.

---

## Auto-update

The app checks GitHub releases in the background and, when eligible, ends up
with a verified update *staged next to the running bundle* — but the swap
only ever happens as part of quitting, never while the app is live.

**Why the swap can't happen live.** Several subprocesses are resolved from
the app bundle's path at spawn time: the relay daemon (whose Unix-socket name
is compiled from `relay-proto`'s `PROTOCOL_VERSION`), the `agent-status` hook
CLI embedded into already-running agent sessions, and the screen-control
gate. Swapping the bundle under a live process would pair an old-version app
with new-version helpers mid-session. So the pipeline stops at a *staged*
copy plus a manifest, and `apps/desktop/src/updater.rs::apply_pending_at_quit`
(called from `on_app_quit` in `main.rs`) does the actual `renamex_np` swap
only after the app has decided to quit. The app never auto-restarts —
restart is always user-initiated (`RestartToUpdate` action, or "Restart now"
in Settings → About) — and an ignored update simply applies itself at the
next ordinary quit.

Windows works the same way and needs to more literally: it refuses to overwrite
a mapped image at all, so `oximux.exe` and every DLL beside it *cannot* be
replaced while the process holds them. The swap there is per-file
(move-aside/move-in, all files or none), and the backups it cannot delete —
because this process is running out of the files it just replaced — are cleared
by the next launch's `boot_housekeeping`.

```
crates/auto-update/src/
├── release/     the signed-release trust chain, shared with apps/cli
│   manifest.rs  `targets` (CLI archives) + `apps` (desktop payloads), by triple
│   verify.rs    minisign signature → sha256 digest → strictly-newer
│   download.rs  Fetcher + host allow-list; URLs from the *signed* tag
│   swap.rs      swap_all (all or none), restore_or_sweep_backups
├── feed.rs      macOS: GitHub /releases/latest; pins the exact asset name
│                OxiMux-{version}-macos-arm64.dmg (see docs/deployment-guide.md)
├── version.rs   plain x.y.z parse/compare
├── bundle.rs    macOS eligibility() — is this exe update-capable at all
│                (UnsupportedReason: NotABundle / Translocated /
│                RootNotWritable / NoPinnableSignature); boot-time signature pin
├── pipeline.rs  macOS: download → mount DMG → stage a copy → verify
├── staging.rs   macOS: PendingUpdate manifest, random-suffix staging dirs,
│                boot_sweep, apply_pending, recover_interrupted_swap
└── windows/     install.rs (eligibility + write probe), archive.rs (unzip the
                 `OxiMux\` payload), pipeline.rs, staging.rs (per-file receipt,
                 apply_pending, boot_housekeeping's restore-or-sweep)
```

`lib.rs` puts the three verbs that differ behind one signature each —
`boot_housekeeping`, `apply_pending_update`, `relaunch_target` — over a shared
`spawn_check`, so `apps/desktop/src/updater.rs` carries no platform branch.

Public state machine (`UpdateStatus`): `Idle → Checking → Downloading →
Installing → Ready | UpToDate | Unsupported | Failed`, tagged with a
`CheckTrigger` (`Background` vs `Manual`) so a background failure stays quiet
while a user-clicked "Check now" surfaces its error. `apps/desktop/src/updater.rs`
owns the `UpdaterState` global, a 6h ticker (first check 60s after boot), the
boot sweep, and the quit-time swap; `apps/desktop/src/platform/relaunch.rs`
is the detached `/bin/sh` helper for "Restart now" — it waits (bounded ~30s)
for the old pid to exit (releasing the single-instance `flock`), then
`open -n`s the bundle, since a new instance launched before the old one
exits would just bounce off the still-held lock.

**Trust anchor, macOS.** The running bundle's own `Identifier` + `TeamIdentifier`,
captured once at boot via `oximux-macos-trust::read_signature` and never
re-derived — an ad-hoc or "not set" team id is rejected as a pin
(`Signature::pinnable()`). The staged copy has to clear, in order: codesign
integrity (`verify_signed`), the identifier+team pin, `spctl --assess`
(the only revocation-aware check — a programmatic download carries no
quarantine xattr for Gatekeeper to check on its own), and a reconciliation of
the staged bundle's own `Info.plist` `CFBundleShortVersionString` against the
version the feed advertised, so a compromised feed can't republish an old
signed build under a high-numbered tag. The download step enforces a
redirect host allow-list (`github.com` / `*.githubusercontent.com`) and a
2×-declared-size ceiling. Staging dirs use random suffixes with
refuse-if-exists + a symlink recheck before the copy lands. Concurrent
checks are guarded by an `AtomicBool` compare-and-swap. A
`.update-pending-verify` sentinel wraps the quit-time swap; if boot finds it
still present, the installed bundle is re-verified before anything else
runs. `OXIMUX_UPDATE_FEED_URL` / `OXIMUX_UPDATE_SKIP_SPCTL` are
`#[cfg(debug_assertions)]`-gated debug knobs, absent from release builds.

`crates/macos-trust` (`oximux-macos-trust`) is shared with the `cua-driver`
installer (see "External tool provisioning" above) — same threat model
(a programmatic download that never gets Gatekeeper's quarantine-xattr
check for free), same crash-safe same-volume-copy + `renamex_np(RENAME_SWAP)`
placement primitive.

**Trust anchor, Windows.** There is no codesign pin to take: the Windows
artifacts are not Authenticode-signed (`scripts/bundle-windows.ps1` says so),
so there is no publisher identity for an update to have to match. The anchor is
instead the one the CLI's `oximux update` already uses — a **minisign signature
over `manifest.json`**, verified against `packaging/release-pubkey.txt` compiled
in by `crates/auto-update/build.rs`. That key lives only in the binary; the
GitHub publish token that could rewrite a release cannot reach it, which is what
a sha256 published beside the artifact it describes can never establish on its
own. Gate order is signature → parse → strictly-newer → sha256 → extract, each
before the step it guards, and `crates/auto-update/tests/windows_update_e2e.rs`
asserts the ordering rather than just the outcomes (a refused update must also
have made no request for the payload and touched no installed file).

The staged payload is *not* re-verified against that signature at quit, and the
reason is worth stating rather than implying a guarantee that is not there: an
attacker who could rewrite the staging directory between staging and quit could
equally well overwrite `oximux.exe` directly, since a per-user install under
`%LOCALAPPDATA%\Programs` is writable by exactly one account. There is no
privilege boundary for a re-verification to defend. (macOS pins a codesign
identity because `/Applications` is *admin-group*-writable — a different threat
model, not a stricter version of this one.) What the quit path does re-check is
integrity: every staged file against a per-file sha256 receipt written when it
was extracted, which catches the failure that actually happens — a disk that
filled mid-extraction, an antivirus quarantine that took one DLL — and catches
it before anything in the install directory has been renamed.

---

## Deferred / not in v1

| Feature | Deferred to |
|---|---|
| ACP agent protocol | v1.1 (ADR-004) |
| Side-by-side diff | Phase 6 |
| Blame, file history, commit graph | Phase 6 |
| Editor + LSP full integration | Phase 5 step 6+ (steps 1-5 shipped; step 6+ = keybindings, multi-file, LSP completions) |
| Markdown WYSIWYG / scroll-sync / TOC / keybinding | follow-on (basic preview shipped) |
| Remote http(s) image rendering in markdown preview | follow-on (FileHttpClient is file:// only) |
| `.mdx` preview | follow-on (kept as plain code editor) |
| SQLite persistence / session restore | Phase 4 |
| Multi-agent dashboard | Phase 7 |
| embeddable terminal library terminal backend | v2 (ADR in brief.md) |
| Per-pane-tab relay reattach on restore | follow-on (multi-tab leaves restore dormant for now) |
| Agent CLI PTYs with OXIMUX_* context env | follow-on |
| Cross-group multi-tab-drag repaint | follow-on |
