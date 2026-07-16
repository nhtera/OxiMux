# OxiMux — Project Changelog

Entries are newest-first. Each entry links to the commit SHA and notes what shipped.

---

### 2026-07-16 — Voice dictation (Vietnamese-first, offline) for Agent Chat (`feat/voice-dictation-vietnamese`)

Local speech-to-text in the Agent Chat composer. Vietnamese is the priority, so
the engine is **sherpa-onnx** running **Whisper** offline (multilingual incl.
`vi`), with **Parakeet TDT v3** as the best-English option. No cloud, no audio
leaves the process.

**Shipped:**
- **New `crates/dictation`** (mirrors the `crates/pty` convention, no GPUI dep):
  cpal capture at device rate → linear resample to 16 kHz → sherpa-rs offline
  decode on a dedicated worker thread. Silence gate, 30 s chunked decode
  (SIGTRAP mitigation), 2-minute hard cap, warm engine cache w/ 10-min idle
  teardown. `Drop` only signals — never blocks the GPUI main thread.
- **Model catalog + download manager**: 3 models (whisper-small default,
  whisper-tiny, parakeet-v3-int8) from k2-fsa releases; resumable HTTP Range
  download, optional SHA-256 verify, `tar` extract, disk-truth status scan.
- **Composer UI**: mic button + recording pill (mm:ss + RMS meter), `Cmd+E`
  toggle, Escape cancels, Enter stops→inserts→sends, transcript inserted at the
  cursor with smart spacing. macOS mic permission via `AVCaptureDevice`
  (`NSMicrophoneUsageDescription` added to Info.plist).
- **Settings › Voice pane**: enable toggle, per-model download/select/delete
  with live progress, language (Auto / Tiếng Việt / English), mic-permission
  status + actions. Persists to `dictation.toml` with a live-reload watcher.

Build-spike confirmed sherpa-rs links on macOS arm64 (dynamic onnxruntime dylib
— the bundle must copy it in). Unit tests green (crate + settings + smart-space);
**live mic round-trip pending developer verification** (needs a physical mic +
the 610 MB model downloaded + a bundled build for the TCC prompt).

### 2026-07-15 — Agent Chat: Codex diff cards, batched streaming, subagent log fidelity (`6c8a7a8`)

Render-fidelity pass closing gaps against native agent UIs. All inside the
existing `ThreadEvent`/`ToolDetail` seams — no architecture change.

**Shipped:**
- **Codex `apply_patch` renders as a diff card.** Previously raw JSON, and the
  result body repeated the whole patch. New `agent_chat/apply_patch.rs` turns
  Codex's `changes` array (per-file, `add`/`delete`/`update`) into the same
  `DiffLine` stream the diff card already renders for Claude/ACP edits, with a
  file-path header; the result body no longer echoes the patch.
- **Stream deltas coalesce into batched repaints.** Long turns stay smooth;
  non-delta events still repaint immediately.
- **Bash command wrapper unwrapped via the structured `command` field**, not
  token parsing — the wire flips quote style between turns, so parsing tokens
  was fragile.
- **Diff rows syntax-highlighted**, memoized per `(path, line)`. Load-bearing,
  not cosmetic: the transcript is not virtualized, so an expanded diff card
  would otherwise re-tokenize via syntect on every unrelated repaint, at the
  new ~20Hz batching rate — caught as a regression risk during review of this
  same change.
- **Session-detail popover** (new `agent_chat/session_detail.rs`): read-only
  view of what a session advertised at init — model, cwd, tools, MCP servers +
  status, subagents — cached with the transcript so a restored chat can answer
  before its resumed process speaks. Hidden entirely for backends that
  advertise nothing (Codex, ACP today).
- **Auto-declined server requests** log a muted divider, deduped so a repeated
  decline doesn't stack identical rows.
- **Subagent logs** capture failed child results + thinking lines; a chatty
  subagent's full log is reachable via the tool sheet.

**Withdrawn — evidence below; do not re-attempt without new upstream data:**

1. **Codex `fileChange` output deltas.** `item/fileChange/outputDelta` does
   not exist on Codex 0.144.3. Live-probed twice (300- and 400-line patches):
   `fileChange` goes `started` → `completed` with zero deltas. The sibling
   `item/commandExecution/outputDelta` *does* stream (`{itemId, delta}`) —
   that's what made the field name look plausible. Needs a Codex version that
   actually emits it.
2. **Claude live status line.** `system/status` never fires on Claude Code
   2.x. Probed twice; only `hook_started`, `hook_response`, `init`, and
   `post_turn_summary` appear. The closest real source is
   `post_turn_summary` (carries `status_category` + `status_detail`),
   already decoded into `ChatThread.last_summary` — but that's post-turn, not
   live, so it's a different feature, not a substitute. Round-9 candidate:
   render a status chip from `last_summary` instead of chasing a live event
   that doesn't exist.
3. **Context-meter catalog seed.** The model catalog carries no context
   windows: `ModelChoice` is `{wire, label, description}`
   (`crates/agents/src/thread/connection.rs:56`), and
   `session_restore/catalog_cache.rs` stores no window field. Building this
   would mean hardcoding a model→window table that goes stale as models ship
   and that the server corrects one turn later anyway. The meter's real seed
   already exists: `ChatThread.last_known_context_window` is stashed from
   both settled `TurnUsage` (`state.rs:449`) and `LiveUsage` (`state.rs:551`)
   and already feeds the meter.

**Wire fact worth keeping:** a subagent `tool_result` block carries
`{tool_use_id, type, content, is_error}` and never the tool's `name` (the name
appears only on the earlier `tool_use` block). A per-line decoder can't title
a result "done: \<tool\>". Failed child results are logged by their error text;
successful ones aren't logged at all — the `tool_use` line already recorded
the action.

**Round-9 backlog:**
- Persist `last_known_context_window` — it's in-memory only, absent from
  `PersistedChatTranscript`, so a restored chat loses its context-bar
  denominator until its next turn settles. One `#[serde(default)]` field.
- Render the `post_turn_summary` status chip `ChatThread.last_summary`
  already stores (see withdrawal #2 above).
- Subagent detach-to-tab (not attempted this round).

**Verification:** phases live-verified by driving a real running app cover
the apply_patch diff card, batched-repaint streaming, the auto-declined
divider, and subagent log capture. The session-detail popover is
unit-verified only — **not** live-verified, since it only renders for Claude
and Claude was unauthenticated under the sandboxed `HOME` used to run a dev
build alongside the session that authored this change.

---

### 2026-07-15 — Worktree isolation becomes a composer pill; two composer state-sync bugs fixed

**The "Run in a fresh worktree" checkbox is now a pill above the input.** It used
to render as a bare full-width column while every sibling control lives in the
composer's centered 720px reading column — so it sat ~97px left of everything
else, and **the gap grew with the window** (~430px adrift at 1600px logical),
which is why it read as broken rather than merely tight. It also floated above
the composer's top border with no chrome, reflowed the composer when enabled, and
gave a once-per-session setting the loudest control on screen.

It now sits in `render_context_row`, beside Import session, above the input. The
split follows Claude Desktop: session **context** (where this runs) above the
input, session **behavior** (permissions, model, effort) below. Context is
answered once before the first send and fixed for the session's life; behavior is
retuned constantly — so the worktree pick does not belong next to the model
picker. Being inside the composer's column, the alignment is now structural
rather than a matched constant.

The pill drives a `Popover` directly rather than reusing `render_dropdown_shell`
(whose `PopupMenu` rows cannot host a text field), with the two isolation choices
and the branch slug inside it — so opening it overlays the transcript instead of
pushing the composer around. Behavior is otherwise unchanged: still opt-in, still
hidden once bound or on a non-git project, same slug validation, same
create/failure banner (which now stays out of the way at rest instead of
reserving an empty strip).

**Two real bugs, same class, opposite directions.** `ComposerView` keeps its
**own** `unbound` flag, separate from `AgentChatView`'s and pushed down by it:

1. **Checking the worktree box stripped the draft's pickers.** `sync_composer`
   unconditionally pushed the bound-chat shape (`set_agent_picker(false, …)` plus
   a caps-derived vocab that is *empty* for a connection-less draft). The worktree
   toggle syncs the composer, so arming a worktree silently removed the agent
   picker, the model picker and the Import row from a live New Agent draft, with
   no way to get them back — the user could no longer change agent or model. Fixed
   by guarding the bound pushes and re-asserting the draft's shape via
   `sync_unbound_composer`. This was the cause of a long-standing "why do these two
   screenshots disagree?" puzzle; it was never a stale build.
2. **The pill lingered on a bound chat.** `set_worktree_draft` was pushed only from
   `sync_unbound_composer`, which stops running once bound, so the pill stayed
   (dimmed) beside a live session's controls. Fixed by clearing it in
   `sync_composer`'s bound branch. Found by driving the real app — no unit test
   bound a view, which is exactly why it survived; `make_bound_for_test` now
   closes that gap.

**Verified live**, including the paths that matter: alignment tracks the column at
two window widths, the popover does not move the composer, slug edits update the
hint and pill label live, invalid slugs error in place, and a forced create failure
(colliding branch) shows the real git error with Retry / "Continue without
worktree" — the latter sends the staged message into the project with no orphaned
workspace. Escape does not dismiss the popover, but the sibling model picker
behaves identically, so that is a pre-existing app-wide pattern, not a regression.

Tests: `oximux-app --lib` **1259/0**; workspace **2972/0**.

---

### 2026-07-15 — Live MCP elicitation verified; import-bridge bubbles keep their provider

**MCP elicitation, live: PASS.** `scripts/mcp-elicitation-probe.py` ran as a real
Codex MCP server (codex-cli 0.144.3) and both halves of the contract round-trip:
Allow → `elicitation result: action=accept content={}`, Reject → `action=decline
content=None`. That confirms `to_codex_elicitation`: an elicitation answers with
`{action}` (accept/decline), not an approval's `{decision}`, and decline omits
`content` per the nullable schema — `content=None` is the proof. A wrong shape
would hang the server rather than error, which is why this needed a live run and
not just a fixture.

Worth knowing for the next person: under the `On request` policy a single tool
call raises **two** cards in sequence — Codex's own tool-approval gate ("Allow the
… MCP server to run tool X?") first, the elicitation ("MCP · {server}: {message}")
second. Only the second is the elicitation.

**Fix: import-bridge bubbles are captioned with their own provider.** A bridge has
no live backend, so it assembles on an inert stream-json placeholder — and
`provider_label` read that placeholder's name, captioning a Pi or OpenCode
transcript as "Claude". It now prefers the bridge's own `provider_display`, which
was already populated for the footer note. Regression test
`import_bridge_labels_bubbles_with_its_own_provider`; verified to fail when the
preference is removed. GUI-verified on a Pi bridge.

Tests: `oximux-app --lib` 1255 passed / 0 failed.

---

### 2026-07-14 — Fix catalog probe leaking an OS thread out of the test scheduler

**Symptom:** `cargo test -p oximux-app --lib` aborted with SIGABRT, reporting
`Detected activity on thread None ThreadId(N), but test scheduler is running on
Some("unbound_draft_switches_agent_and_model_without_binding")`. The suite passed
with that one test skipped, and the test passed alone — a race, which is why it
had been green until timing shifted.

**Root cause:** `maybe_probe_catalog` spawns the real agent binary on a raw
`std::thread::spawn` (deliberately: the probe blocks, and the connection runs its
own workers, so no GPUI executor or tokio reactor is touched). The early-return
that skips the spawn keys off the `CatalogCache` global, which no test installs —
so `change_agent("codex")` reached straight past the injected `StubConnection` to
the real binary. The thread is owned by no executor, so it outlived the
`#[gpui::test]` scheduler and its completion landed during a later test.

This broke a contract the test seams already state: `with_connection_for_test`
injects a stub "instead of spawning a real subprocess", and `make_unbound_for_test`
exists so a test can "drive `change_agent`/`change_model` on a draft without
spawning a subprocess".

- **New `AgentChatView::probe_catalogs_live` seam** — true in `assemble` (every
  real view), false in `with_connection_for_test`. `maybe_probe_catalog`
  early-returns when it is off. A draft on a stub has no real catalog to show and
  discarded the probe's result anyway, so nothing is lost.
- **Regression test `draft_agent_pick_does_not_start_a_live_catalog_probe`**
  asserts `probed_catalogs` stays empty after picking Codex and OpenCode (the two
  dynamic-model agents a probe targets). Verified to fail when the seam is flipped
  back on, so it genuinely bites.

The production path is unchanged: real views still probe live off-thread.

Verified: six consecutive unskipped `cargo test -p oximux-app --lib` runs, no
SIGABRT, 1254 passed / 0 failed each (1253 baseline + the new test). Whole
workspace 2967 passed / 0 failed.

---

### 2026-07-14 — Agent Chat: fix transcript messages painting on top of each other

**Symptom:** a reply's last paragraphs drew underneath the next user bubble, the
hover action row, and the settled-turn summary/cost footer.

**Root cause (the previous diagnosis was wrong).** The transcript's children were
`div().flex().flex_col().w_full().max_w(px(CONTENT_MAX_W))`. As a flex item that
makes taffy size the column's height against the *container's available* width
and apply the max-width only afterwards, so a reply measured across the full pane
re-wraps into more lines once it is capped to the reading measure — and paints
those extra lines outside the height it reported. Measured with a temporary probe
over `ScrollHandle::bounds_for_item`: the 300-word reply reported **400px** and
painted ~475px. The child rects themselves were always correct and never
overlapped; this was a paint overflow, not a layout overlap.

This also retires the old "gpui-component's markdown counts only its FIRST block"
theory recorded in the `tail_gap` comment — markdown measures multi-block content
correctly; only the width it measured at was wrong.

- **Transcript children are now built at a definite width** (`transcript_column(width)`
  → `w(px(width))`), resolved by the new `AgentChatView::content_width()` as
  `min(scroll box width − padding, CONTENT_MAX_W)`, so measure width == paint width.
- **`min_w_0()` added to `wrap_scroll`'s container and the scroll box** — the
  horizontal twin of the existing `min_h(px(0.0))`. Without it a flex item's
  default `min-width: auto` refuses to shrink below the now-definite children, so
  the children pin the box open at the reading measure, `content_width()` reads
  that back, and a pane narrower than the measure clips text instead of wrapping.
  Verified live at a ~260px pane: text wraps, nothing clips.
- **`markdown_reveal_gap` deleted** (fn + test + its use). It reserved up to
  1100px of extra scroll room so `scroll_to_bottom` could out-scroll the
  under-count; with honest heights it is unnecessary, and it was leaving a ~565px
  blank gap under the last message. `tail_gap` is now just the breathing margin.

Tests: whole workspace green — 2967 passed / 0 failed (`oximux-app --lib` 1254,
`oximux-agents` 614).

---

### 2026-07-14 — Agent Chat round-7: worktree-as-workspace + OpenCode/Pi open-as-chat bridge

**What shipped:** the two round-6 follow-ups, closing the "deferred" / "known
limitation" notes in the round-6 entry below
(`plans/260714-1020-agent-chat-round7-worktree-workspace-import-wiring/`). Phases
1–2 code-complete + tested; Phase 3 (signed-bundle notifications, Codex OAuth,
live MCP elicitation, live GUI walk-through) stays user-gated.

- **A New-Agent worktree is now a first-class `Workspace`.** The "Run in a fresh
  worktree" send no longer creates a git-only worktree with no sidebar presence.
  The leaf carries no `WorkspaceRepo` (Orca thin-leaf), so it routes up: the
  roster emits `AgentChatEvent::WorktreeWorkspaceRequested{slug}` → the pane
  group dispatches the new `CreateWorktreeWorkspaceForActiveChat{slug}` action →
  `WorkspaceRoot` (owns `app_state`) resolves the active chat, runs the existing
  `create_workspace_with_rollback` (git worktree **+** DB `Workspace` row, full
  rollback on failure), refreshes the rail, and hands the outcome back through
  the chat's `on_worktree_create_outcome` (which rebinds the cwd + resumes the
  staged send). The row is enumerable by the sidebar's `list_for_project` gather,
  so the worktree agent gets a sidebar card + `⌘J` jump entry, and removal via
  the Worktree panel deletes both the worktree and the row. The active chat-view
  handle is captured **synchronously** before the async create, so a tab switch
  mid-create can't misroute the callback. The git-only `create_agent_chat_worktree`
  helper + its tests were retired (coverage preserved by
  `workspace_create_rollback.rs`, which gained an enumerability assertion).
  **Why.** Round-6 shipped the worktree but not its workspace identity; this
  routes the one flow through the DB-inserting op the sidebar already renders,
  rather than reimplementing anything.
- **OpenCode and Pi sessions open as a chat (transcript bridge).** `⌘↵` on an
  OpenCode/Pi row in the `⌘⇧H` picker now builds a chat tab that renders the
  seeded transcript (via the round-6 `load_import_provider_transcript`
  dispatcher) — but since neither provider has an in-app chat backend, the
  composer is swapped for a **Resume in terminal** action. The button emits
  `AgentChatEvent::ResumeInTerminalRequested`; the pane group spawns the
  provider's own TUI directly into a terminal tab (`open_script_terminal_tab`
  fed the `import_resume_command` argv, e.g. `pi --session '<rollout>'`) — the
  same seam `OpenLoginTerminalRequested` uses. **Why not a window action:** a
  `ResumeAgentSession` dispatched from the pane group never reaches
  `WorkspaceRoot`'s handler (it works from the Session History modal), so the
  button was inert; spawning inline removes the dispatch dependency entirely.
  New `AgentChatView::new_import_bridge` (seeded transcript, `connect_now:false`,
  `unbound:false`, `send_text` guarded to a no-op) + `render_import_bridge_footer`;
  `entry_opens_as_chat`/`OpenChatSession` gained a `preset_id`. Default Enter /
  click still resumes in a terminal — the bridge is the explicit "open as chat"
  path, never a fake live send. Bridge tabs are excluded from tab persistence
  (they carry no live session id, so a resumed-chat restore would silently spawn
  a live `claude` and drop the imported history) — they re-open from Session
  History like Diff/Tasks tabs; reopen dedups on `(preset_id, session_id)`.
  Copilot stays resume-only (no transcript mapper wired). **Why.** Modeled on
  Super Conductor's terminal-resume bridge (Validation Session 1): read the past
  turns in Chat View, continue the session in a terminal.
- **Verification.** Full workspace test suite green (`oximux-app --lib` 1254/0);
  the round-6 `send_on_armed_draft_stages_the_message_instead_of_binding` test was
  updated to the new stage-and-emit architecture (state parks at `Creating`, not
  `Failed`, since the runtime/git step moved from the roster to the host).
  **Both phases live-verified** on a fresh `dist/OxiMux.app` build: a New-Agent
  worktree (`r7-verify`) produced a sidebar card + `⌘J` entry + git worktree +
  `oximux/r7-verify` branch + DB row, the agent bound and replied, and Delete
  cleaned up worktree/branch/row/dir; an imported Pi session opened as a bridge
  with its multi-turn transcript seeded as chat bubbles (no raw JSON), and
  Resume-in-terminal spawned a live `pi (resumed)` TUI on the same conversation.
  Bridge tabs correctly do NOT survive a restart (persistence exclusion), and the
  default terminal-PTY resume path is unregressed. The three round-6 user-gated
  carry-overs (signed-bundle notifications, Codex ChatGPT-OAuth, live MCP
  elicitation) remain open. **Known cosmetic nit:** bridge assistant bubbles are
  labeled "Claude" rather than the provider, since the bridge assembles on an
  inert Claude placeholder backend.

---

### 2026-07-14 — Agent Chat round-6: signed-bundle notifications, import transcript previews, worktree-per-agent

**What shipped:** three independent closeouts from the round-6 plan
(`plans/260714-0248-agent-chat-round6-closeout-worktree/`), across
`scripts/bundle-macos.sh`, `oximux-agents`, and `oximux-app`.

- **Opt-in codesigning for real notification grants.** `bundle-macos.sh` gained
  a `--sign <identity>` flag + `OXIMUX_CODESIGN_IDENTITY` env var (the legacy
  `OXIMUX_SIGN_ID` still works), read ahead of the script's existing
  positional-arg parsing so `--debug-fast`/profile handling is untouched. The
  ad-hoc (`-`) default is byte-identical to before — only a real identity
  triggers `verify_signature`, which fails the build loudly if the sealed
  bundle's `codesign -dv` Authority chain doesn't show the requested identity.
  **Why.** `UNUserNotificationCenter` silently drops the one-time
  authorization grant for ad-hoc-signed bundles on some macOS versions, so
  round-5's attention-notification banners never appeared even after
  accepting the permission prompt; a real "Apple Development: …" / "Developer
  ID Application: …" identity (or a local self-signed codesigning cert, see
  the script header) gives a stable identity the grant sticks to.
- **OpenCode and Pi sessions reopen with a real transcript, not a blank chat.**
  `session_log/import_transcript_opencode.rs` (SQLite `message`/`part` rows,
  read-only) and `session_log/import_transcript_pi.rs` (JSONL `message` lines)
  map each provider's native records into `Vec<ThreadEntry>` previews — the
  same shape Claude/Codex import already produced — dispatched by
  `load_import_provider_transcript` in `import_provider_index.rs`. Consecutive
  text parts fold into one bubble, reasoning/thinking folds into the
  assistant entry, and anything unrecognized (tool parts, unknown blocks)
  degrades to a plain notice row rather than ever rendering raw JSON. The
  `⌘⇧H` picker's preview blurb for these two rows already shipped in
  `99b4650`; **app-side open-as-chat wiring for OpenCode/Pi is deferred**
  _(closed in round-7 — see the entry above)_ —
  today both still resume as a terminal PTY, this round only prepared the
  transcript-shaped data for that future wiring. Investigated Copilot's
  `session-store.db` `turns` table along the way: it does hold readable
  `user_message`/`assistant_response` text, but Copilot has no chat surface in
  OxiMux to seed, so it stays resume-only (documented for a future round).
- **"New Agent in a fresh worktree."** The New Agent composer's worktree
  toggle now creates a real git worktree before the first send:
  `roster.rs`'s toggle + slug input calls
  `workspace_ops::create_agent_chat_worktree(repo_root, slug)` (branch
  `oximux/<slug>`, sibling path via the same `suggest_worktree_path` helper
  the manual Worktree panel form uses) → on success rebinds the chat's `cwd`
  to the new worktree and labels the tab `oximux/<slug>`; on failure shows an
  inline banner with Retry or "continue without a worktree." Hidden entirely
  for non-git projects. **Known limitation** _(closed in round-7 — see the entry
  above)_**:** the created worktree gets no
  sidebar workspace card / `⌘J` entry — `AgentChatView` has no
  `WorkspaceRepo` handle (it's built from a bare `cwd`, not the app's storage
  layer), so there's no DB `Workspace` row to insert; the worktree still works
  identically for terminals/SCM/diff since those key off the filesystem path.
  Round-7 routes this up (leaf emits an event → `WorkspaceRoot` runs the
  DB-inserting `create_workspace_with_rollback`) so the worktree becomes a
  first-class workspace.
- **Verification.** Full round-5 observation backlog re-checked live against a
  fresh `dist/OxiMux.app` build of HEAD: subagent log, Codex session reopen,
  mid-turn Codex rewind (rewinds an active turn without crashing the session),
  and cold restore all confirmed working; live MCP elicitation and Codex
  OAuth remain user-gated follow-ups (fixture/unit coverage stands for
  elicitation; OAuth's RPC login-start was already verified live in
  round-5). code-reviewer flagged and fixed a submit-during-Creating race and
  a worktree-toggle message-loss bug, both closed with regression tests.
  Tests: workspace suite green; `oximux-app` lib 1258 tests (verified this
  session).

---

### 2026-07-14 — Import session history for OpenCode, Copilot, and Pi

**What shipped:** the `⌘⇧H` history / import modal now indexes and resumes
sessions from three more agent CLIs, matching the reference import modal's
provider set. The filter chips are now **All · Claude · Codex · Copilot ·
OpenCode · Pi**, and each new provider's rows list, scope-by-project, and
resume like the native ones.

- **Stores scanned directly** (each degrades to "absent" — a missing store,
  table, column, or malformed line yields fewer rows, never a crash; SQLite is
  opened read-only):
  - **OpenCode** — `~/.local/share/opencode/opencode.db`, the `session` table
    (`id`, `title`, `directory` = cwd, `time_created`/`time_updated`). Resumes
    via the existing `opencode --session <id>` preset.
  - **Copilot** — `~/.copilot/session-store.db`, the `sessions` table
    (`id`, `cwd`, `branch`, `summary`, `created_at`/`updated_at`) + `turns`;
    title is the stored `summary`, else the first turn's user message; scopes by
    the recorded `cwd` column and carries the git `branch`. Resumes via
    `copilot --resume=<id>`. Timestamps parse both RFC 3339 and SQLite's
    `datetime('now')` text form. (Schema is a CLI-internal detail, read
    defensively; the store is read even when its data lives in an uncheckpointed
    WAL.)
  - **Pi** — `~/.pi/agent/sessions/**/*.jsonl`, header line
    `{"type":"session","id","cwd","timestamp"}` + a `session_info.name` title.
    Resumes via `pi --session <file>` (the rollout path is the handle).
- **Design.** These are import-only, terminal-resume providers, so they ride
  `AgentAdapter::Custom` plus a new `SessionEntry.preset_id` slug rather than new
  core adapter variants (which would leak into the workspace-create dialog, the
  CLI runtime, and persistence). The picker keys filtering, the row tag, the
  icon, and resume routing on `preset_id`; a single `import_resume_command`
  resolver in `oximux-settings` owns each provider's resume argv so the index,
  picker, and spawn layer can't drift.
- **Why disk-scan.** The reference app reaches OpenCode/Copilot over a live
  server/RPC, but every provider keeps a local store carrying the same fields,
  so a synchronous read matches the existing Claude/Codex indexing without
  spawning a subprocess. Like the reference, import rows show no transcript
  preview for the SQLite providers (their list APIs return none either).
- **Scope.** New collectors in `oximux-agents`
  (`session_log/import_provider_index.rs`, unit-tested for all three shapes +
  cwd scoping); picker/routing in `oximux-app`; resolver in `oximux-settings`;
  the three providers' brand marks (`currentColor`) registered in `assets.rs`.
  GUI-verified against real local stores: OpenCode, Copilot, and Pi each list
  their cwd-scoped sessions and resume in a terminal. Tests: agents 598/0,
  settings 82/0, app picker 18/0.

---

### 2026-07-14 — Codex session history: scan real rollouts, not the legacy index

**What shipped:** the session index now discovers Codex sessions from the CLI's
rollout store instead of a stale companion file, so the `⌘⇧H` modal's **Codex**
filter lists real, resumable sessions (previously "No Codex sessions").
`oximux-agents` only.

- **Root cause.** `collect_codex` read `~/.codex/session_index.jsonl` — a
  cwd-less legacy/desktop index. Because it carried no cwd, Codex sessions were
  gated to the all-projects view only, so the default cwd-scoped modal showed
  none; and its ids weren't the CLI rollout ids `codex resume` uses.
- **Fix.** `collect_codex` now walks
  `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (the store `codex resume` lists
  and `codex_session_import` already reopens), reading each rollout's
  `session_meta` head for id + cwd + git branch + start time and its first
  non-injected user turn for the title. With a real cwd, Codex sessions scope by
  project exactly like Claude's. Both rollout shapes are handled — newer
  (`response_item` payloads, `session_id`) and older (top-level `message`,
  `id`). The injected `AGENTS.md` / `<…>`-context turns Codex prepends are
  skipped so the title matches `codex resume`.
- **Preview.** The history preview pane learned the rollout format too, so a
  selected Codex session shows its opening exchange instead of a blank pane.
- Streaming head read (early-stop at the first genuine prompt, bounded cap)
  keeps the scan cheap despite Codex inlining a large `AGENTS.md` block as the
  first synthetic turn. Tests: `oximux-agents` 590/0 (session_log 63/0).

---

### 2026-07-13 — Session-history import: agent-type filter + mode-aware routing

**What shipped:** the centered session-history / import modal (`⌘⇧H`) gained
agent-type segmentation and surface-aware import, in `oximux-app` +
`oximux-settings`.

- **Agent-type filter chips.** A header chip row (`All | Claude | Codex |
  OpenCode`) narrows the list to one agent family; click a chip or press `Tab`
  to cycle. Pure logic (`AgentTypeFilter`, `filter_sessions_typed`) lives in
  `session_history/picker.rs`, composing the type gate with the existing fuzzy
  query. OpenCode has no indexed sessions yet (its store is a SQLite DB — a
  follow-up); its segment shows a "coming soon" hint so the category is
  discoverable now.
- **Mode-aware default import.** `↵` / row-click now opens the highlighted
  session on the surface its adapter is configured for — a chat tab when the
  resolved open mode is `Chat` and the adapter is chat-capable, else a terminal
  resume. The gate is the single `AgentLaunchSettings::opens_as_chat(id)` helper
  shared with the new-agent launcher (no drift). `⇧↵` still forks; `⌘↵` still
  force-opens as chat.

---

### 2026-07-13 — Agent Chat round-5 P1 wave (6 product-parity features)

**What shipped:** the deferred P1 backlog from the round-4 research report plus two
round-4 verification carry-overs, across `oximux-agents` and `oximux-app`.

- **Subagent activity → parent tool card.** Claude sidechain events (matched by
  `parent_tool_use_id`) and Codex collab child-thread activity (buffered by
  `receiverThreadIds`, replayed on completion) now stream into the parent Agent/Task
  card's `subagent_log` (most-recent-few + "+N earlier"), instead of being dropped —
  the parent card reveals what the subagent is doing without child events leaking into
  the root transcript. Both buffers clear on the root `turn/completed` (bounded).
- **Generalized session import.** "Reopen as chat" now covers Codex, not just Claude:
  `codex_session_import.rs` reads a finished `~/.codex` rollout (Responses-API format)
  into a `ThreadEntry` transcript. Surfaced in both the session-history page and the
  New Agent flow. ACP has no list API → documented unsupported.
- **Attention notifications.** A turn finishing / erroring / needing a permission while
  the app is unfocused posts a macOS `UNUserNotificationCenter` banner + dock badge
  (`notifier/`), coalesced per-tab and cleared on focus; the three event types are
  individually toggleable in Settings → Notifications. An ACP Stop (interrupted) no
  longer fires a false "Done".
- **Codex structured auth card.** A logged-out Codex (or a turn-time 401) now surfaces a
  sign-in card instead of a generic error: ChatGPT-OAuth via `account/login/start` →
  the browser opens (fire-and-forget worker emits `AuthUrl`, never blocking the main
  thread) → completion auto-retries the turn. Secrets are never persisted.
- **Claude compacting indicator + live mode switch.** `system/status` `compacting` now
  promotes to a "Compacting context…" spinner that resolves into the existing
  compaction-boundary divider (cleared on turn-end if none arrives), so a long
  compaction reads as progress not a hang. Changing the composer permission mode now
  writes a `set_permission_mode` control_request on the live process's stdin (the Agent
  SDK's wire) — the mode switches **in place**, no `--resume` respawn (respawn remains
  the fallback for model/effort). *(Spike verified against `@anthropic-ai/claude-agent-sdk@0.3.207`.)*
- **Tool-payload fullscreen sheet.** A `⤢` on any tool card with a substantial payload
  (a diff, or a result over 600 chars) opens a full-height overlay (`tool_sheet.rs`)
  showing the whole payload: a large diff virtualized via `uniform_list` (fixed `h_row`
  rows), or a long shell/read/fetch body via the shared inline renderer with its
  row/char caps lifted (a `full: bool` size-mode threaded through `tool_bodies.rs`).
  Copy button + Esc / backdrop / ✕ dismiss; reads its tool call live by id so a
  still-running tool grows in place.

**Verification:** `cargo test --workspace --no-fail-fast` green (known-flaky
`spawn_args_reach_child_process` tolerated); each phase code-reviewed (findings fixed).
Live-GUI: Phase 5 (compacting spinner → boundary divider; in-place mode switch, PID-stable,
in-workspace Edit auto-approved) verified in the running app; remaining phases + the two
carry-overs (mid-turn Codex rewind, live MCP elicitation via `scripts/mcp-elicitation-probe.py`)
tracked in `plans/260713-1243-agent-chat-round5-p1-wave/reports/gui-verification.md`.

### 2026-07-13 — Agent Chat round-4 P0 parity (5 protocol-correctness gaps)

**What shipped:** five P0 agent-chat parity features closing the round-4 research
gaps vs Paseo/Super Conductor, across the `oximux-agents` and `oximux-app` crates.

**Claude:**
- **Live tool-input streaming** — the decoder now handles `content_block_start`
  (`tool_use`) and `input_json_delta`, so a tool card opens as soon as the model
  starts composing the call and its arguments render live (a Write/Edit shows its
  content growing) before the finalized `tool_use` block arrives. A best-effort
  partial-JSON parser (`parse_partial_json`) closes the truncated fragment; the
  fold accumulates fragments on the open tool call and upgrades it in place at
  finalize (no duplicate cards), with a 64 KiB preview cap.
- **Plan-mode approval card** — `ExitPlanMode` (which rides the `can_use_tool`
  channel) now renders as a dedicated plan card: the plan markdown plus the CLI's
  three choices (approve + auto-accept edits / approve + ask each time / keep
  planning). Approve echoes a `setMode` suggestion so the CLI exits plan mode and
  continues the turn; the composer mode chip updates optimistically (Claude sends
  no wire mode-echo). Introduces a `PermissionKind` taxonomy
  (`Tool`/`Plan`/`Mode`/`Mcp`/`Other`) on the permission event, serde-defaulting
  to `Tool` so old transcripts load unchanged.

**Codex:**
- **Conversation rewind via `thread/fork`** — `supports_rewind` is now true for
  Codex. Rewind/regenerate/edit-and-resend fork the thread server-side on the
  live connection (`thread/fork` with `lastTurnId`, addressed by an in-session
  turn-id ledger) before the process is stopped, then respawn resuming the fork.
  The original thread is left intact. `thread/rollback` is deprecated upstream and
  deliberately not used. Fork-to-new-tab stays Claude-only (it reads the on-disk
  session log). Conversation-only — no files/checkpoint axis for Codex.
- **MCP elicitation cards** — `mcpServer/elicitation/request` (an MCP server asking
  for consent mid-tool) was silently auto-declined; it now surfaces as an
  `MCP · <server>` permission card and the reply rides the elicitation
  `{action: accept|decline}` shape (distinct from an approval's `{decision}`).
- **Approval-policy + sandbox controls** — the composer footer exposes two selects
  (Approvals: on-request / never; Sandbox: read-only / workspace-write /
  danger-full-access, the dangerous options marked). Changes apply per-turn via a
  `turn/start` posture override (no respawn) and persist per session
  (`codex_posture` on the transcript, restored into the connection + the footer).

**Verification:** `cargo test --workspace --no-fail-fast` green (known-flaky
`spawn_args_reach_child_process` tolerated); protocol/decoder/fold covered by unit
+ fixture tests; live-GUI verification of the five features tracked in
`plans/260713-0008-agent-chat-round4-p0-parity/reports/gui-verification.md`.

### 2026-07-12 — Agent Chat: disk-persisted model-catalog cache for the New Agent picker

**What shipped:** a process-wide, disk-persisted cache of each dynamic-model
agent's model catalog, so the *New Agent* draft's model picker paints instantly
instead of waiting on a cold backend spawn.

**Why:** Codex and ACP agents (OpenCode, Cursor, Amp) don't declare a static
model list — the New Agent draft learns it by cold-starting a throwaway "catalog
probe" backend just to read the models. That probe is ~0.1s for Codex (a Rust
binary) but ~5s for a Node ACP agent like OpenCode (interpreter start + plugin
load + a 50-model `session/new`). Before this change the cost was paid *every*
time a draft picked that agent, because each draft view started with an empty
per-view probe map.

**How (stale-while-revalidate, mirrors `GitStateCache`):**
- New `CatalogCache` GPUI `Global` (`crates/app/src/session_restore/catalog_cache.rs`)
  keyed by adapter id: `entries` (last-known catalog, persisted as JSON under
  `agent_catalog_cache_v1`) + `fresh` (adapters re-probed live this session,
  in-memory only).
- `ModelChoice` / `ProbedCatalog` gained `serde` derives so the catalog can be
  persisted and rehydrated.
- Boot seeds the cache from disk (`main.rs`, next to `GitStateCache`); quit
  persists it. A missing/corrupt/legacy blob degrades to an empty cache (one
  cold probe, then self-heals) — persistence never blocks boot.
- `maybe_probe_catalog` now seeds the picker from the cache: a fresh entry is
  trusted with no re-probe; a disk seed paints instantly and revalidates exactly
  once; a miss falls back to the normal `Loading` state. The revalidation
  fold-back (`fold_probe_result`) never lets an empty/failed re-probe clobber a
  good seed.

**Scope / follow-up:** covers the *New Agent → pick agent* path (the throwaway
probe shown in the model-picker screenshot). The direct-launcher path
(clicking Codex/OpenCode/Cursor/Amp straight from the launcher) opens a bound
view that eager-connects a real backend and learns models via the live session
handshake — that path is unchanged and is a documented follow-up.

**Verification:** `cargo test --workspace --no-fail-fast` green; new `catalog_cache`
unit tests (round-trip, fresh-marking, empty/corrupt degradation) + a
`fold_probe_result` regression test guarding the good-seed-vs-empty-revalidation
case; GUI-confirmed the OpenCode probe returns a non-empty 50-model catalog.

### 2026-07-11 — Agent Chat: ACP interactive-resume terminal for OpenCode (Phase 7 mechanism)

**Touches**: `crates/settings/src/agent_launch.rs`, `crates/app/src/shell/agent_chat/mod.rs`, `crates/app/src/shell/pane_group/tabs.rs`

Completes the deferred **mechanism** half of the round-3 ACP terminal work (the disabled-reason bugfix shipped with the round-3 batch). A bound **OpenCode** chat can now toggle to a companion Terminal view that resumes its exact session interactively (`opencode --session <id>`), matching the Claude/Codex `--resume` companion. Both deferral gates were cleared empirically via an ACP JSON-RPC probe against the installed `opencode acp` — no live GUI required:

- **Session-id mapping (gate 1).** opencode's ACP `session/new` returns a native `ses_…` id; `opencode export <that-exact-id>` round-trips it (directory + model + version intact), proving the ACP-protocol session id IS the id `opencode --session` resumes. The chat resumes the same conversation, not a fork.
- **Concurrent-writer contract (gate 2).** opencode's on-disk store is **append-only, one file per message / per part** (`message/<sid>/<msgid>.json`, discrete `part/` files with unique ids). A live headless ACP connection and the interactive resume writing the same session never share an append target — only the per-session metadata blob (title/token counters) is a benign last-writer-wins. Contract: verified-tolerant; the ACP arm ships under it.
- **Amp/Cursor stay unwired** (honest verified-negative): the installed `amp-acp` advertises `loadSession:false` + empty session caps and demands an API key (no id to verify), amp's resume uses a *different* binary (`amp threads continue`) than its ACP wrapper, and real `amp` returned 503. Offering an unconfirmed preset would be a broken toggle.

Design: `AcpPreset` gains `interactive_resume: Option<fn(&str) -> Vec<String>>` (opencode only); `terminal_launch_spec` resolves the preset by `acp_command` and returns a spec over the generic `AgentAdapter::Custom` (spawns argv verbatim, ignores `resumption`); `spawn_companion_terminal` populates `custom_command`. The agent-supplied session id is charset-validated (alnum + `-`/`_`, non-empty, no leading `-`) before it reaches argv — defense-in-depth atop the shell-free PTY spawn. Claude/Codex paths byte-identical.

Gate `cargo test --workspace --no-fail-fast` green at **2868 / 0** (+2: opencode-wiring + session-id-charset tests). A `code-reviewer` pass (independent `cargo check`/`clippy` + a rustc repro confirming the dropped-`PartialEq` rationale) returned clean — no Critical/High/Medium; two informational edge notes (the `Custom` adapter ignores `model`/`args` overrides — now doc-noted; preset resolution matches by command and fails closed for a user-overridden wrapper — correct-by-design).

**GUI-verified end-to-end** (computer-use, live app): opened an OpenCode chat, sent a message → bound (session id minted, live context meter visible); ⌃⇧V spawned a real `opencode --session <id>` terminal (exact argv confirmed via `ps`) that replayed the exact transcript — the same session, not a fork (gate 1 live). A concurrent chat-side send with the terminal attached left both turns intact and `opencode export` valid JSON with all messages (gate 2 live). Closing the chat tab reaped the companion.

---

### 2026-07-11 — Agent Chat round-3 completion: adapter correctness + chat-UI gaps

**Touches**: `crates/agents/{src/{lib,tab_title (new)}.rs, src/thread/{stream_json,claude_stream_json,state,event,entry,tool_detail}.rs, src/thread/codex/{map,mod,protocol,image_items (new)}.rs, src/thread/acp/worker.rs, src/commit_message/{spawn,mod}.rs, src/thread/testdata/stream_json_error_{stale_resume,api}.jsonl (new), Cargo.toml}`, `crates/app/src/shell/agent_chat/{mod,composer,context_meter (new)}.rs`, `crates/app/src/{session_restore/persisted_terminals,shell/project_panes/ops,project_panes_factory,shell/pane_group/tabs,shell/chrome/tab_context_menu,workspace_root/render,shell/settings_modal/pane_agents_launch}.rs`, `crates/settings/src/agent_launch.rs`

Closes the round-3 adapter-correctness gaps + chat-UI completion items from `plans/260711-1010-agent-chat-round3-research/`. Every wire assumption was empirically probed against the **installed** CLIs first (claude 2.1.207, codex-cli 0.144.1, opencode 1.17.15, amp) — no plan-time guesses reached code.

- **Claude error correctness.** `decode_result` now reads the `errors[]` array (the real API-error carrier) before the plain `result` string, so a turn error surfaces its actual text instead of the generic "turn ended with error". A stale `--resume` (session gone) is detected (`SessionResumeStale`) and recovered: the dead session id is cleared **only when it still equals the attempted id** (a fresh `SessionInit`-minted id survives), and a divider notice is dropped so the next send starts clean. The previously-piped-but-never-read child stderr is now drained by a bounded ring thread (removing a latent full-pipe deadlock); on an error turn it rides as a `Diagnostic` event that is **de-duplicated** against the turn's own error text and **secret-redacted** before any UI/persist surface. Two real captured fixtures back the decoder tests.
- **Codex threadId guard (bug was LIVE on 0.144.1).** A single fail-open guard at the top of `map_notification` drops any turn-state- or transcript-mutating notification whose `threadId` names a different thread than the known root — so a sub-agent's `turn/completed`, deltas, usage, **or `error`** can never end/splice/corrupt the root turn. Fires ONLY when the root id is known (`(Some, Some)` mismatch); an unknown root passes through, keeping every existing single-thread test byte-identical. Sub-agent items (`collabAgentToolCall`/`subAgentActivity`) now classify as `SubAgent` via their type name (the 0.144.1 `tool` field is populated, so the old fallback missed them).
- **Codex item completion.** `webSearch` results render the query + action instead of a blank body; `imageView`/`imageGeneration` render inline thumbnails via the shared `ToolResultImages` pipeline — their file paths are canonicalized and **rejected unless contained within the session cwd** (guards against an agent pointing at `~/.ssh/config`); `item/commandExecution/terminalInteraction` (a top-level notification method, not an item type) surfaces the typed stdin on the command card.
- **Live usage + context meter.** A new `LiveUsage` event is emitted mid-turn by all three adapters (Claude `message_start`/`message_delta`, Codex `thread/tokenUsage/updated`, ACP `UsageUpdate`) **without** disturbing the settled-turn usage footer. The composer gains a compact bar-in-pill context meter (`context_meter.rs`) with >90%/≥70% thresholds, a cross-turn window-size cache (covers Claude's live gap → a designed token-count-only state on turn 1), and a "cost since open" accumulator (turn-settled only, omitted when no cost data exists). The circular ring was an explicit stretch goal, left for later.
- **Composer drafts + queue persistence.** An unsent composer draft AND queued-but-unsent messages now survive tab-close / app-quit (`PersistedTabKind::AgentChat` gains `draft`/`queued`/`unbound`, all `#[serde(default)]`), restoring into the correct bound-vs-unbound shape; queued messages restore as chips and are **never auto-sent** on relaunch. A queued chip gains "send now" (move-to-front mid-turn / dequeue+submit when idle). ⌘↵ and client-only `/clear` were found **already working** and documented rather than re-built.
- **LLM auto-titling.** After the first message on a Claude/Codex chat, a one-shot `claude -p --model haiku` call generates a short title through the existing `TitleChanged` sink (ACP chats skipped — they push native titles). Reuses the commit-message spawn plumbing (`run_plan` gained a `timeout` param; one prod + 7 test call sites updated). Hardened per the red team: **strict JSON-only parse** (no raw-stdout fallback, so injection output can't reach the label), `--tools ""` tool-lockdown atop `--permission-mode plan`, an owned/cancellable task, an 80-char cap, and a default-on settings toggle (Settings → Agents).
- **ACP terminal disabled-reason.** The view-options hint now distinguishes "send a message first" (genuinely no session) from "no interactive terminal for this agent" (a bound ACP chat) via a `TerminalAvailability` enum — fixing the GUI-found misleading hint. The interactive-resume **mechanism** was deferred pending two gate checks and landed in a follow-up commit (see the OpenCode Phase-7-mechanism entry above): opencode is wired; amp/cursor are a verified-negative.

Gate `cargo test --workspace --no-fail-fast` green at **2866 / 0** (baseline 2833; +33 net new tests). A `code-reviewer` pass (verified against a freshly regenerated 0.144.1 schema + the live `claude --help`) confirmed the containment/termination/divide-by-zero/tokio-handoff checks safe and found two real narrow gaps, both fixed: the threadId guard missed the `"error"` method (a foreign sub-agent error could still kill the root turn — `ErrorNotification.params.threadId` is required), and `/clear` didn't reset the auto-title guard (a fresh conversation kept its old title). Two optional pre-existing/defense-in-depth follow-ups noted (Codex `error_message` reads the wrong path; `redact_secrets` suffix list could widen).

---

### 2026-07-11 — Agent Chat: functional ACP EnvVar auth (respawn-with-env)

**Touches**: `crates/agents/{src/thread/{connect,acp/{mod,worker}}.rs,examples/{mock_acp_env_auth_agent,acp_env_auth_smoke}.rs (new),Cargo.toml}`, `crates/app/src/shell/agent_chat/{mod,auth_card}.rs`

Turns EnvVar-kind ACP auth from an instructions-only card into a working sign-in. The card now shows a **masked secret field per advertised variable**; on submit OxiMux **respawns the agent subprocess with those values in its environment**, then authenticates — the only way an env-credentialed agent can sign in, since a running process can't pick up env after it spawned and `AuthenticateRequest` carries no credential values.

- **Agent side.** `ConnectSpec` gained `env` + `auth_method`; `AcpConnection::spawn` is now a thin wrapper over `spawn_with_env` (existing callers unchanged). The worker builds the child via `AcpAgent::from_args` with leading `NAME=value` tokens so the secrets land in the process **environment, never argv** (no `ps` leak); the no-env path still uses `from_str` (byte-identical). After a respawn the worker `authenticate`s the seeded method **once** on the first `AuthRequired`, then retries the open (falling through to the interactive card on failure).
- **App side.** Masked `InputState` fields are reconciled from the card in `render` (the event fold has no `Window`); `submit_env_auth` reads them straight into the respawn's in-flight `ConnectSpec.env` and **never** into the persisted transcript. Plain `respawn` (Stop→resume, live switch) is unchanged — it's `respawn_with_env(vec![], None)`.

Gates: `cargo test --workspace --no-fail-fast` green (**2833/0**). New `acp_env_auth_smoke` + `mock_acp_env_auth_agent` prove the env reaches the child and the auto-authenticate opens the session; the existing `acp_auth_smoke` (Agent-kind, no respawn) stays green. A `code-reviewer` pass found no secret leak (traced the argv/env split into the vendored `agent-client-protocol` source) and three non-blocking issues — a multi-EnvVar-method field-sharing edge, a spawn-failure card-precedence edge, and doc drift — all addressed. Closes the deferred follow-up from `plans/260710-2327-acp-round2-correctness-ux/` phase 4.

---

### 2026-07-11 — Agent Chat: ACP round-2 correctness + UX (P1–P5)

**Touches**: `crates/agents/{Cargo.toml,src/thread/{acp/{mod,worker,approvals,map,auth (new)},connect,connection,event,mod,state}.rs,examples/*}`, `crates/app/src/shell/agent_chat/{mod,composer,acp_terminal_host,auth_card (new)}.rs`, `docs/system-architecture.md`

Closes the five round-2 ACP client-side correctness gaps + the UX tail found after P1–P5 shipped. Claude/Codex paths byte-identical throughout; wire shapes against `agent-client-protocol-schema 1.4.0`.

- **P1 — turn correctness.** `session/prompt`'s `stop_reason` is no longer discarded: `Refusal`/`MaxTokens`/`MaxTurnRequests` end the turn with the error banner + a human reason (they used to render as clean turns); `EndTurn`/`Cancelled` (+ any future `#[non_exhaustive]` reason) stay clean. `cancel()` now drains the parked `session/request_permission` responders (drop-to-resolve), so a Stop while a permission card is pending answers the agent `Cancelled` instead of leaving its next turn wedged (the ACP child isn't respawned on Stop).
- **P2 — prompt richness.** Attached images now ride an ACP prompt as base64 `ContentBlock::Image`, gated on `prompt_capabilities.image` (text-only fallback when absent — no rejected turns). Agent-spawned embedded terminals default `PAGER=""`/`GIT_PAGER=cat` so `git log` &c. can't hang on a pager. `mcpServers: []` serialization locked by a tripwire test.
- **P3 — true session restore.** A restored ACP tab resumes the agent's real context via `session/load` when it advertises `loadSession`; a `replaying` gate drops the agent's history replay (OxiMux repaints its own persisted blob — one code path for all agents, instant paint) while letting control updates through. Non-auth load failure falls back to a fresh session with a visible notice (worst case = today's amnesiac behavior, now explicit).
- **P4 — auth flow.** A logged-out agent (`AuthRequired`/-32000) is no longer a dead end: its `auth_methods` render an auth card — Agent pill (`authenticate`), Terminal inline login (runs the agent's login command in an embedded terminal), EnvVar instructions — and `authenticate` retries the session on the **same connection** (no respawn). Enables the `unstable_auth_methods` cargo feature (the env-var/terminal `AuthMethod` variants are feature-gated at the pinned schema; the plan's validation log had wrongly assumed they were stable). EnvVar landed instructions-only here; its functional respawn-with-env sign-in shipped in the follow-up above.
- **P5 — UX tail.** Permission requests surface the agent's **extra allow-kind options** as pills that answer with their exact `option_id` (reject-kind stays behind the base Reject button; Claude's card byte-identical — empty suggestions for a plain allow/reject request). A `ThoughtLevel`-category config option drives the reasoning-effort picker in-session (mirroring the ACP model picker; the Model-only extractor generalized to `select_for(category)`). Slash-command argument hints (`AvailableCommand.input`) feed the composer's existing usage-hint strip.

Gates: `cargo test --workspace --no-fail-fast` green (**2833/0**, +29). Six new headless smokes + mock agents (`acp_{cancel,load,auth}_smoke`) exercise the real regressions each phase fixes; the existing `acp_terminal_smoke` stays green. A `code-reviewer` pass confirmed P1/P2/P3/P5 solid and caught two real P4 bugs — a composer lockup (a prompt sent while the auth card showed wedged `turn_active` forever + left a phantom transcript entry) and a dropped `AuthMethodTerminal.env` — both fixed (Send now gates on a pending auth prompt + the worker seals a raced prompt as an error turn; terminal-login env threaded through). Plan: `plans/260710-2327-acp-round2-correctness-ux/`.

---

### 2026-07-10 — Agent Chat: normalized tool-detail classifier (P5)

**Touches**: `crates/agents/src/thread/{tool_detail (new),tool_call,event,state,mod,acp/map}.rs`, `crates/app/src/shell/agent_chat/tool_bodies.rs`, `docs/system-architecture.md`

Turns ACP's generic key:value tool fallback into the same rich cards Claude and Codex already get, via one provider-agnostic classifier. Claude rendering is byte-identical (each Claude name classifies to the archetype that maps back to its original body — locked by the dispatch test).

- **`ToolDetail` classifier (`tool_detail.rs`).** A pure `classify(name, acp_kind, input) -> ToolDetail` collapses all three providers into one archetype (`Shell`/`Read`/`Edit`/`Write`/`Search`/`Fetch`/`WebSearch`/`SubAgent`/`Mcp`/`Plan`/`Plain`/`Unknown`). Claude classifies by name; Codex by its already-renamed name (so its `web_search` finally reaches the rich WebSearch card, not a generic one); ACP by its typed `ToolKind` — authoritative because an ACP tool's `name` is a freeform human title that can't be pattern-matched. Conservative by design: an ambiguous or unrecognized signal (`switch_mode`, `other`, any future `#[non_exhaustive]` kind) resolves to `Unknown` → the clean generic card, never a wrong-body guess.
- **Threading the ACP kind.** `ToolCall` gained `#[serde(default)] kind: Option<String>`; ACP surfaces it via a follow-up `ToolKind` event (the same idiom as `ToolTerminal`/`ToolResultImages`, so the ~26 `ToolCallStarted` construction sites stay untouched), emitted from both the tool-call and tool-call-update paths and folded onto the card. Claude/Codex never emit it (classify by name).
- **Renderer.** `render_tool_body` dispatches on `ToolDetail::classify` instead of raw name matching; `render_bash` gained a generic-input fallback so an ACP `Execute` whose input uses a non-`command` key still shows its raw input (no information loss) above the output.

Gates: `cargo test --workspace --no-fail-fast` green; agents `+9` tests (classify table across Claude/Codex/ACP + conservative-unknown cases; ACP `ToolKind` emit + fold) + app cross-provider dispatch test proving Codex `web_search` / ACP `execute`/`read` route to rich cards while unclassified ACP tools stay generic. A `code-reviewer` pass returned **no findings** (Claude rendering byte-identical for real payloads; classifier conservative + future-proof against the `#[non_exhaustive]` `ToolKind`; persistence provably intact). Plan: `plans/260710-1022-agent-chat-acp-parity-research/` (P5). The one remaining ACP polish item is permission option-label buttons (a P3 follow-up).

---

### 2026-07-10 — Agent Chat: ACP embedded terminal (P4)

**Touches**: `crates/agents/src/thread/{acp/{client_terminal,map,mod,worker},event,state,tool_call}.rs`, `crates/app/src/{main,shell/agent_chat/{acp_terminal_host,mod},shell/terminal/terminal_view/mod}.rs`, `docs/system-architecture.md`

Gives ACP agents a true chat+terminal experience: an agent can create a terminal, run a command, and embed it **live and inline** in a tool card. Wire shapes locked against `agent-client-protocol-schema 1.4.0` (`schema::v1`). Claude/Codex and existing ACP paths byte-identical.

- **Protocol layer (`oximux-agents`, commit f73146e).** Advertise the ACP `terminal` capability and serve all five `terminal/*` methods (create/output/wait_for_exit/kill/release) by delegating to an app-installed `AcpTerminalHost` trait — dependency inversion that keeps the domain crate free of the UI/relay stack, mirroring the `AgentConnection`/`TerminalBackend` seams. `ToolCallContent::Terminal` maps to a new `ToolTerminal` event that binds the client-minted terminal id to its tool card. Inert until a host is installed (capability advertises false, handlers reject).
- **App backing + inline UI (`oximux-app`, commit e9ed321).** `EmbeddedTerminalHost` spawns a real PTY through the app's own terminal stack (`spawn_embedded_command`: relay daemon when up, in-process fallback), with a per-terminal watcher thread draining the PTY's independent status-event stream into an output ring + exit latch (so `terminal/output`/`wait_for_exit` answer off the renderer's queue). The chat view mounts a live inline `TerminalView` per tool-call id (mirroring the question-card reconcile), bounded-height inside the card, and reaps on tab close + when the card leaves the transcript. Background mount so a mid-turn spawn never steals composer focus; reaping releases the host entry by its own terminal id (a distinct id-space from the tool-call id).

Gates: `cargo test --workspace --no-fail-fast` green (2804/0); `+12` tests (agents: `terminal/*` handler translation, `ToolTerminal` map + fold; app: host registry edge cases + output-ring truncation). A `code-reviewer` pass caught two real defects — a reap id-space mismatch (host entry leaked because release was called with the tool id, not the terminal id) and a focus-theft on mount — both fixed before commit. Static verification confirmed the relay/in-process PTY plumbing (both back `subscribe_status_events` + `capture_status_events`), and that `create()` on the ACP worker's non-tokio thread is safe (relay owns its runtime handle). Plan: `plans/260710-1022-agent-chat-acp-parity-research/` (P4).

**Verified** — a deterministic mock ACP agent (`crates/agents/examples/mock_acp_terminal_agent.rs`) + headless driver (`acp_terminal_smoke.rs`, commit 019de86) assert the full `terminal/create` → `ToolTerminal` path over a real `AcpConnection`; **GUI-confirmed** by pointing the ACP launcher at the mock and watching the inline `TerminalView` stream live `bash` output and settle with the process-exited banner, then reap cleanly on tab close.

---

### 2026-07-10 — Agent Chat: ACP content richness (P3, 3 of 4 items)

**Touches**: `crates/agents/src/thread/{acp/map,event,state}.rs`, `crates/app/src/shell/agent_chat/{composer,mod}.rs`, `docs/system-architecture.md`

Lifts the ACP adapter closer to the Claude/Codex bar (verified against `agent-client-protocol-schema 1.4.0`, `schema::v1`). Decoder-side, low-risk; Claude/Codex paths byte-identical.

- **ACP tool-result images.** `content_images()` extracts `ContentBlock::Image` (base64 + mime) from a tool call's content and emits the P1 `ToolResultImages` event alongside the result, so an ACP tool that returns an image (a screenshot tool) renders a thumbnail (reusing P1's tool-card renderer + lightbox) instead of dropping the pixels.
- **ACP message content richness.** The agent message/thought chunk path swapped `text_of` for `message_chunk_text`: `ResourceLink` → a clickable `[name](uri)` Markdown link, `Image`/`Audio` → a muted placeholder (no longer silently dropped), embedded `Resource` → its text (a defensive JSON shape-probe that degrades to `[resource: uri]`).
- **Slash-command descriptions.** `SlashCommandsUpdated` gained a parallel `descriptions` list (ACP `available_commands_update` fills it; Claude/Codex send empty). `ChatThread` holds them in an **ephemeral, non-persisted** `slash_command_descriptions` map (blank entries skipped); the composer palette prefers the agent's own description over the on-disk catalog. The persisted `slash_commands: Vec<String>` schema is unchanged.
- **Deferred:** ACP permission option-label buttons (P3's 4th item) — it touches the shared permission card (Claude/Codex regression risk) for cosmetic value that Allow/Deny already covers; tracked as a P3 follow-up.

Gates: `cargo test --workspace --no-fail-fast` green (2792/0); agents `+3` decoder tests (ACP image, resource-link, image-placeholder) + a slash-description fold assertion. A `code-reviewer` pass returned **no findings** (all ACP field accesses verified against the schema; no wire-data panics; Claude/Codex + persistence provably untouched). Plan: `plans/260710-1022-agent-chat-acp-parity-research/` (P3). GUI verification pending (needs a live ACP agent emitting image/resource content).

---

### 2026-07-10 — Agent Chat: Claude taxonomy + Codex plan/output/question parity (P1+P2)

**Touches**: `crates/agents/src/thread/{event,state,stream_json,tool_call,question}.rs`, `crates/agents/src/thread/codex/{map,approvals,mod}.rs`, `crates/agents/src/thread/testdata/stream_json_richtools.jsonl` (new), `crates/app/src/shell/agent_chat/{mod,question_card}.rs`, `crates/app/tests/right_sidebar_tab.rs`

Brings two of the three chat adapters closer to completeness. Architecture unchanged — decoders still normalize into `ThreadEvent`; this is taxonomy coverage + one renderer add. Wire shapes were locked empirically first (the plan's mandated gate): Codex via `codex app-server generate-json-schema` (0.141.0), Claude via a live `claude` 2.1.205 stream-json capture.

- **P1 — Claude stream-json taxonomy.** `decode_assistant` gained `server_tool_use`/`mcp_tool_use` (mcp qualifies as `<server>.<tool>`) and a muted `redacted_thinking` marker (never renders the ciphertext). `decode_user` accepts the 6 `*_tool_result` variants (mcp/web_search/web_fetch/code_execution/bash_code_execution/text_editor) and **extracts inline base64 images** → a new `ToolResultImages` event, rendered as clickable thumbnails in the tool card (reusing the user-image lightbox) instead of the `[image]` placeholder. `compact_boundary` → a new `CompactBoundary` divider (reuses the `ContextCompaction` render). A debug-only unhandled-block log tripwires SDK drift. *Empirical note:* the live CLI wraps WebSearch/MCP as plain `tool_use`/`tool_result`, so the `server_tool_use`/`*_tool_result` arms are forward-compatible coverage; the confirmed daily win is inline images.
- **P2 — Codex app-server.** `turn/plan/updated` → the shared `PlanUpdated` plan panel (maps Codex `inProgress`→`in_progress`; no per-step priority → `medium`). `item/commandExecution/outputDelta` → a new `ToolOutputDelta` that streams live command output into the open card (completion still replaces with the authoritative `aggregatedOutput`, but a **blank** final no longer erases already-streamed text). `item/tool/requestUserInput` (was auto-answered empty) now renders the interactive question card and routes selections back via a Codex-shaped `answer_question` (`{answers:{<qid>:{answers:[..]}}}`, keyed by the backend's native question id — distinct from Claude's text-keyed shape). Codex's `isSecret` flag is threaded through `AskQuestion` and shown as a "🔒 Sensitive" card hint (full input-masking + no-persist is a tracked follow-up). `thread/compacted` → the same compaction divider.

Gates: `cargo test -p oximux-agents` green (479, +15 incl. a fixture-replay over the full new Claude taxonomy and Codex plan/output/question round-trips); `cargo check -p oximux-app` clean; `cargo test --workspace --no-fail-fast` green after fixing the long-stale `right_sidebar_tab` test (a `History` tab added in dd24107 was never reflected there — unrelated to this change). A `code-reviewer` pass returned 1 high (verified a non-issue against the 18-variant Codex `ThreadItem` schema + render path), 1 medium (`isSecret`, now handled), 3 low (2 fixed, 1 pre-existing spun off as a task). Plan: `plans/260710-1022-agent-chat-acp-parity-research/`. GUI verification pending (stale-binary rule).

---

### 2026-07-07 — Agent Chat: Claude-Desktop completion tail (6 view-layer gaps)

**Touches**: `crates/app/src/shell/agent_chat/{mod,bubble,tool_bodies,rewind_menu,composer,error_card (new),find_bar (new)}.rs`, `crates/agents/src/thread/state.rs`, `crates/app/src/assets.rs`

Closes the short, high-value tail between the shipped Claude chat and a Claude-Desktop-grade experience. All changes are view-layer + one `ChatThread` method; none touch the stream-json transport or the `AgentConnection` seam, so this ships independently of the multi-provider work.

- **Reliability bug — silent mid-conversation error (P1).** A turn that errored *after* the first message was swallowed: `last_error` was only rendered by the empty-state hint, which paints solely on a blank transcript. Now an inline error card sits at the transcript tail (idle + connected + non-empty + `last_error`), with a **Retry** that re-sends the last prompt (alive → direct; crashed/interrupted → respawn-then-send), gated on `!turn_active` so it never double-sends. `last_error` clears on turn start; the disconnected banner gained the same Retry(respawn) affordance.
- **Regenerate.** Settled assistant replies in the *last* turn expose a Regenerate action beside Copy, reusing the rewind machinery verbatim (session-file fork + respawn-then-send via `rewind_then_send`) so the resumed CLI session matches the truncated transcript — no second truncation path. Restricted to the tail turn (UI gate + method guard) so it can never silently drop earlier turns.
- **Markdown polish.** Code fences in replies get a language tag + one-click copy (ported from the markdown preview pane); thinking blocks render as markdown instead of raw source.
- **Generic tool cards.** WebFetch / WebSearch / `mcp__*` now render legible bodies (URL/query header + capped result, compact MCP arg line) instead of raw key:value JSON; Task/Agent was already covered. Unknown tools still fall back to the generic card.
- **New / Clear in place.** `ChatThread::clear()` + a "New chat" composer button reset the tab to a fresh non-resumed session (old child reaped); typing `/clear` is intercepted and resets locally instead of being sent as literal text. Guards on `rewinding` to avoid racing an in-flight rewind's respawn.
- **In-chat find (Cmd+F).** A find bar over the transcript searches user/assistant/tool text and steps between matches (`n/total`, ↑/↓), reusing a new entry→scroll-child map and the shared row-flash. `Search` is handled on the focused chat (stops propagation before the workspace-root→terminal fallback); Enter/Shift+Enter route to next/prev only while the find input is focused. Counter only counts jumpable (visible-transcript) matches. Close: the ✕ button, Escape (via `capture_action(InputEscape)`), OR a second Cmd+F (toggle) — the toggle is a deliberate keyboard-close fallback because some macOS input methods swallow Escape while a text field is focused.

Gates: `cargo check -p oximux-app` clean; `cargo test -p oximux-app -p oximux-agents --lib` green (1197 + 400, incl. new tests for the error/retry path, regenerate selection + non-tail refusal, tool-card dispatch, `clear()`, `recompute_matches`, and the entry-child map); `cargo test --workspace --no-fail-fast` shows only 2 pre-existing unrelated `right_sidebar_tab` stale-test failures (flagged separately). Clippy clean on all touched files. A `code-reviewer` pass found 6 issues (1 critical, 1 high, 2 medium, 2 low); all addressed in-session (see `plans/reports/reviewer-260707-agent-chat-completion.md`). **GUI-verified via computer-use** on the fresh build: code-block copy button, Regenerate present on the last reply + hidden on earlier replies (the critical fix), New-chat reset + empty-restore, and the find bar (open/`n/total`/Enter-next/toggle-close). Found + fixed one GUI bug during verification: the find bar didn't close on Escape (this machine's IME eats Escape) → added the Cmd+F toggle-close. Plan: `plans/260707-0253-oximux-agent-chat-claude-desktop-completion/`.

---

### 2026-06-30 — Fix: Send-to-Agent routes to the agent you last used

**Touches**: `crates/app/src/shell/pane_group/state.rs`, `crates/app/src/shell/project_panes/state.rs`

With more than one agent open, "Send Selection / Last Output to Agent" could land in an unintended agent. When the action fires from a focused *terminal*, the active tab isn't an agent, so `target_agent_session` fell straight through to `first_agent_session()` — an arbitrary tab-order pick that ignores which agent the user was actually working with.

- New `PaneGroup::mru_agent_session()` resolves the most-recently-active agent from the group's existing MRU queue (front = most recent), so it's naturally validated and close/reorder-safe.
- `target_agent_session` now prefers, most-specific first: active-tab agent → active-group MRU agent → any-group active agent → any-group MRU agent → first agent. The common "run a command in a terminal, send its output to the agent I was just in" flow now lands deterministically.

GUI-verified: with two agents, activating one then sending from a terminal lands in that agent; activating the other switches the target cleanly. This also explained an earlier red herring — a "restored agent isn't receiving sends" symptom was really this mis-routing (the send was going to a different agent); once correctly targeted, restored agents receive normally. `cargo test --workspace --no-fail-fast` green (2433 pass / 0 fail).

---

### 2026-06-30 — Terminal: OSC 133 shell-integration bootstrap on spawn

**Touches**: `crates/app/src/shell/terminal/shell_integration.rs` (new), `crates/app/src/shell/terminal/mod.rs`, `crates/app/src/shell/terminal/terminal_view/{mod,lifecycle,input}.rs`, `crates/app/src/app_settings/terminal_settings.rs`, `crates/settings/src/terminal.rs`

Makes **Send Last Output to Agent** (and the exit-code gutter badges) work out of the box. The terminal already *parsed* OSC 133/633 command marks but a stock shell emits none, so the prompt/output bands stayed empty and the action was a silent no-op for the default shell. Now OxiMux injects a small command-mark bootstrap when it spawns a plain shell.

- **Bootstrap generator** (`shell_integration::augment_spawn_config`). Detects zsh / bash / fish from the spawn config's shell and merges the right `env` + `args` so both the in-process backend and the relay daemon pick it up over the existing spawn wire fields (no protocol change). zsh has no extra-rcfile flag, so we point `ZDOTDIR` at an overlay dir whose four startup files `source` the user's real ones (via `OXIMUX_ORIG_ZDOTDIR`, default `$HOME`) then arm `precmd`/`preexec` and restore `ZDOTDIR`; bash takes `--rcfile`; fish takes `--init-command`. The hooks emit `133;A` (prompt), `133;C` (output start), `133;D;$?` (command end with exit). Overlay scripts are written app-side under the data dir and only rewritten when their content changes.
- **Injected only for genuine shells.** Wired at the four plain-shell spawn sites (new spawn — relay + portable fallback — and both dormant-promote/restore paths); launched-agent PTYs (where the agent binary is argv[0]) are untouched.
- **Guarded against double-emitting.** Two integrations both emitting `133;A` per prompt would collapse the output band to nothing, so the hook generically detects an *existing* OSC-133 emitter — any already-registered prompt/pre-exec hook whose body contains a `133;` mark (covers a user's own prompt framework, another terminal's hooks, etc.), plus the VS Code / iTerm env sentinels — and **skips its own injection**, deferring to the existing one. Also gated by a re-entry sentinel (idempotent re-source) and a new `shell_integration` setting (on by default; mirrors the `OXIMUX_SHELL_INTEGRATION=0` env opt-out).

Gates: `cargo test --workspace --no-fail-fast` green (2433 pass / 0 fail, incl. generator + guard + settings tests); zsh/bash overlays pass `zsh -n` / `bash -n`; real-pty functional runs confirmed both the full `A / C / D;exit` stream when OxiMux is the sole integrator AND a clean skip (single `A` marks) when another emitter is present. **GUI-verified** on a machine whose `~/.zshrc` already ships an integration: OxiMux correctly deferred to it and "Send Last Output to Agent" landed the full multi-line output band in the agent composer. On `feat/terminal-rich-stable-fixes`; fish unverified locally (not installed). Deferred: when deferring to an integration that emits `A` but no `D` (no exit code), the exit-code gutter badges stay dark for that session — could add a D-only hook in future.

---

### 2026-06-30 — Terminal: right-click menu, standard behaviors, stability hardening

**Touches**: `crates/app/src/shell/terminal/terminal_context_menu.rs` (new), `crates/app/src/shell/terminal/terminal_view/{input,render,mod,lifecycle,state}.rs`, `crates/app/src/shell/terminal/terminal_links.rs`, `crates/app/src/shell/pane_group/render.rs`, `crates/app/src/actions.rs`, `crates/app/src/workspace_root/{mod,ops,render}.rs`, `crates/app/src/shell/{project_panes/ops,workspace/workspace_ops}.rs`, `crates/app/src/shell/settings_modal/pane_terminal.rs`, `crates/pty/src/{state,backend,portable_pty_backend}.rs`, `crates/relay-client/src/backend.rs`, `crates/settings/src/terminal.rs`

Closes the terminal surface/polish gaps + the stability red flags from the 2026-06-29 terminal research (`plans/260630-0004-oximux-terminal-rich-stable-fixes/`). The engine was already strong; this is surface + a handful of real bugs.

- **Right-click context menu (the headline gap).** The terminal grid had NO context menu — right-click was dropped unless a mouse-reporting app consumed it. New `TerminalContextMenu` (mirrors `tab_context_menu.rs`: one entity owned by `WorkspaceRoot`, opened via `OpenTerminalContextMenuAt`) holds a `WeakEntity<TerminalView>` and calls view methods directly for grid ops (Copy / Paste / Paste Text / Select All / Clear / Open-Copy Link / Search / Send Selection·Last-Output to Agent), dispatching root-level actions for Split / Set Title / Close Tab. Right-click auto-selects the word under the cursor when nothing is selected, so Copy is never a no-op. The card is edge-aware: it opens down-right from the cursor but flips UP near the bottom and LEFT near the right edge so it's never clipped off-screen. Still forwards to vim/tmux when they're reporting.
- **Standard terminal behaviors.** `Clear` wipes grid + scrollback (new `TerminalBackend::clear()` → `ESC[H ESC[2J ESC[3J`). **`Cmd+A` now selects the FULL scrollback** (behavior change — was viewport-only; Copy yields everything retained). Middle-click pastes the clipboard (gated off inside mouse-reporting apps). New `copy_on_select` setting (off by default) + Settings → Terminal toggle. A program's OSC 2 window title now drives the terminal tab label (a manual "Set Title…" still wins and pins it). Send-last-output-to-agent reads the full command output from the history grid (OSC 133/633 marks) instead of clamping to the viewport. Both send-to-agent paths now **bracketed-paste-wrap** the payload (`agent_paste_bytes` / `CliRuntime::send_agent_paste`) when the target agent has DECSET 2004 on, so a multi-line selection/output lands as one reviewable block instead of the agent's readline executing each line; `\n`-terminated auto-submit commands stay raw. Double-click word selection was already Unicode-aware (`char::is_alphanumeric()` spans CJK/accented) — added a regression test rather than a redundant predicate change.
- **Stability hardening.** Mutex-poison `expect` → `into_inner()` recovery at all 4 shared-backend spawn sites (a panic in one holder can't cascade-abort). `adopt_live_session` clears stale selection/hover/drag so placeholder state never leaks onto the live session. The search-grid clone runs OFF the main thread (a generation guard drops out-of-order results) so a large-scrollback search no longer blocks the relay reader. Concurrent link-existence `stat()` tasks are capped. A failed `promote_to_live` restores the dormant cwd (pane stays retryable, not wedged). A shift-drag selection latches local — releasing Shift mid-drag no longer leaks mouse reports into the app. Three capture/autosave paths dropped a needless `ListPtys` daemon round-trip on the main thread (cached session id). Guarded a provably-safe `.unwrap()` in the link parser; tracked the OSC 52 remote/local clipboard split as a deferred `FIXME`.

Gates: `cargo check --workspace` clean; `cargo test --workspace --no-fail-fast` green (0 failures, includes new tests for pty `clear`, full-grid extraction, Unicode word boundary, poison recovery, selection-clear-on-adopt, `classify_link`); release build clean. On feature branch `feat/terminal-rich-stable-fixes`; GUI smoke checklist pending before merge.

---

### 2026-06-27 — Refactor: Tier-1 reorg + extract `oximux-ui` (zero behavior change)

**Touches**: `xtask/src/main.rs`, `xtask/file-size-allow.txt` (new), `crates/app/src/lib.rs`, `crates/app/src/shell/mod.rs`, `crates/app/src/{app_settings,agent_glue,session_restore,platform,loaders}/` (new folders), `crates/app/src/shell/terminal/` (new cluster), `crates/app/src/shell/diff_view/{mod,paint}.rs`, `crates/ui/` (new crate `oximux-ui`), `Cargo.toml`, `docs/*`

De-bulk the ~93k-LOC `app` crate for navigability — a pure refactor, full suite green and matching baseline (2382 pass / 0 fail / 2 ignored), clippy/debug/release clean.

- **Tier-1 in-crate reorg.** 31 loose top-level modules grouped into five concern folders (`app_settings/`, `agent_glue/`, `session_restore/`, `platform/`, `loaders/`); ~17 terminal modules clustered under `shell/terminal/`. All via `git mv` + crate-root re-exports, so every `crate::<name>::…` call site resolves unchanged.
- **`oximux-ui` crate.** The most-shared widget surface (`app/src/ui/` — FloatingSurface, buttons, danger_ghost — used by 21 modules) + the generic `ConfirmDialog` move into a new `crates/ui`. Depends only downward (gpui, gpui-component, oximux-settings) with zero path back to `oximux-app` (cargo-tree verified); host re-exports it as `crate::ui`. `toast`/`divider` stayed in `app` (they reach host state — moving them would create a forbidden back-edge).
- **diff_view slim.** `impl Render for DiffView` moved out of `mod.rs` (2546→1737 LOC) into its `paint.rs` rendering sibling.
- **File-size lint relaxed to GPUI reality.** Warn 1500 / fail 3000 (was 500/800), plus a ratchet allowlist (`xtask/file-size-allow.txt`) that grandfathers the 3 remaining over-cap files and can only shrink.
- **Deferred (parked w/ design record):** `oximux-contract` (33-module action floor) + all `*-ui` feature crates — see `docs/adr/006-tier1-reorg-and-oximux-ui.md`.

Gates: `cargo build --workspace` (debug + `--release`), `cargo test --workspace --all-targets` (2382 pass), `cargo clippy --workspace --all-targets` (0 warnings), `cargo run -p xtask -- file-size-lint` (ok). On a refactor branch; per-phase commits; pending GUI smoke + merge to main.

---

### 2026-06-25 — Agents: hook-driven ambient tracking + rail repaint perf

**Touches**: `crates/app/src/shell/ambient_agent_scan.rs` (new), `crates/app/src/shell/terminal_view.rs`, `crates/app/src/shell/session_merge.rs`, `crates/app/src/shell/left_rail/*`, `crates/app/src/shell/pane_group/mod.rs`, `crates/app/src/shell/project_panes/mod.rs`, `crates/app/src/shell/agent_session_persistence.rs`, `crates/app/src/shell/workspace_ops.rs`, `crates/app/src/workspace_root.rs`, `crates/agents/src/{osc_sideband,status_machine}.rs`

Hand-launched agent CLIs in normal terminal tabs now participate in the same per-workspace agent list as spawned agent sessions, with the SAME stable, hook-driven status — not a flaky terminal-title heuristic.

- **Hook-driven ambient detection.** A plain `TerminalView` now consumes the OSC-9999 status sideband the global hooks already emit onto its stream (`AmbientAgentScan`), giving a hand-typed `claude`/`codex` the same rich status (prompt title, live tool step, needs-approval, working/idle) as a spawned agent. The terminal-title heuristic is kept only as an immediate-presence fallback for an agent that has not yet fired a hook. Marker-gated so a plain shell pays one substring scan and no allocation.
- **Status stability.** Sideband `Working` no longer decays to `Idle` from a stale/later plain-output timestamp (`StatusMachine::sideband_running`).
- **Ambient rows show their prompt.** The hook prompt becomes each ambient row's title (the reference cockpit's primary per-agent distinguisher); it rides the compared ambient map, so a new prompt repaints the rail through the dirty-check.
- **Rail repaint perf.** `set_sidebar_data` dirty-checks its render inputs and repaints only on a real change; the per-workspace agent merge (150+ rows) is cached and rebuilt only on a session/live-agent change — killing the per-output-byte whole-rail rebuild that caused hover/scroll jank.
- **Active styling.** A multi-agent active workspace wraps the card and its agent rows in one container sharing the active surface fill; child hover fills are suppressed while active. Workspace summary label precedence matches status precedence (active tracked sessions stay authoritative).

Gates: `cargo build -p oximux-app`; `cargo clippy -p oximux-app -p oximux-agents` (0 warnings); `cargo test -p oximux-app --lib` (1014 pass); `cargo test -p oximux-agents --lib` (279 pass). Live verify: run `OXIMUX_STATUS_HOOKS=1 RUST_LOG=oximux_app=debug,oximux_agents=debug cargo run -p oximux-app`, type `claude` in a plain terminal, prompt it, and watch for `ambient agent OSC-9999 sideband decoded` + the grouped rail row.

---

### 2026-06-23 — Agents: fix NeedsApproval status detection (Notification hook)

**Touches**: `crates/app/src/agent_status_hooks.rs`, `crates/app/src/main.rs`

The agent status hooks reported `needs_approval` via a `PermissionRequest` hook event that does not fire in current Claude (verified live), so an agent blocked on a tool-permission prompt never lit the amber status dot. Replaced it with the `Notification` hook event — the event Claude actually fires for permission prompts — gated by a new `--filter-notification` CLI flag so only genuine permission asks report `needs_approval`.

- The filter keys on the payload's typed `notification_type == "permission_prompt"` field (captured from a live prompt), with a narrow message-keyword fallback. The benign "waiting for your input" idle nudge is correctly ignored.
- `notification_is_permission()` is a pure, unit-tested helper; `run_agent_status_cli` reads stdin once and gates the emit.
- Caveat: the `Notification` hook fires on Claude's idle delay (not instantly), so it is a reliable backstop; the immediate signal remains the adapter's prompt-body regex.
- Pixel-verified live: a permission notification drives the tab status dot to the amber `status_warn` colour; idle/non-permission notifications do not. App-crate lib tests 968 pass.

**Commit**: `8d0749f` (chore: remove quick-reply approval card)
**Touches**: `crates/app/src/shell/approval_card.rs` (deleted), `crates/app/src/actions.rs` (`QuickReplyToAgent` removed), `crates/app/src/workspace_root.rs` (handler removed), `crates/app/src/shell/pane_group/` (overlay wiring removed), `crates/app/tests/fixtures/claude-approval-bytes.txt` (deleted)

The keystroke-send approval card (an overlay that mounted over a blocked agent's pane with Approve/Deny/free-text buttons) is removed. The mechanism carried more complexity than it earned: a per-adapter byte map (`1\r` approve / `Esc` deny) that is fragile to Claude's command-dependent menu shape, a double-send debounce state machine, and bottom-dock overlay mutual-exclusion with the composer. Agents are answered directly in their terminal, or via the `⌘I` composer.

- Agent **status detection** is unchanged — `AgentStatus::NeedsApproval` still drives the tab-strip dot, the status badge, the dashboard card, and the macOS approval notification. Only the interactive overlay is gone.
- The `⌘I` prompt composer (Phase 4) is unaffected and now the sole bottom-docked agent overlay.
- No relay protocol change. App-crate lib tests: 965 pass (the 3 approval-card unit tests removed with the module).

---

### 2026-06-22 — Agents: prompt composer bar with @file autocomplete (cockpit rebuild Phase 4)

**Commits**: `19f4c20` (feat: prompt composer bar with @file autocomplete), `40f17fc` (fix: composer submit + correct agent routing)
**Touches**: `crates/app/src/shell/compose_bar/` (new — `mention_parser`, `mention_resolver`, `send_formatter`, GPUI view, @mention dropdown), `crates/app/src/shell/`, `crates/agents`

A Cmd+I composer bar docks over the active agent pane. Multi-line draft input with inline `@file` autocomplete (fuzzy over the project's `rg` index, reusing the command-palette fuzzy ranker). Cmd+Return submits: formats the draft via `send_formatter`, routes via `SendTextToActiveAgent`, and delivers bytes using `TerminalBackend::bracketed_paste()` (DECSET-2004) when the agent shell supports it. The first submit to an untitled agent tab auto-titles it from the prompt. Esc closes the dropdown then the bar.

- `mention_parser` / `mention_resolver` / `send_formatter` are pure modules, unit-tested.
- Submit fix (`40f17fc`): the Input widget binds secondary-Enter to its own action, so the keystroke never reached `on_key_down`; moved submit into the Enter capture handler keyed on the secondary flag — bare Enter still inserts a newline.
- Routing fix: composer renders and submits only while its origin agent is the active tab, preventing misroute on tab switch.
- No relay protocol change.

---

### 2026-06-21 — Agents: emit status via relay RPC + Settings toggle for status hooks (cockpit rebuild Phase 2c)

**Commits**: `c8a3de3` (feat: emit agent status via relay RPC instead of /dev/tty), `d6ea6bf` (feat: Settings toggle for agent status hooks)
**Touches**: `crates/relay` (new `Request::AgentStatus`; protocol v5 → v6), `crates/app` (`agent_status_hooks.rs`, `pane_agents_launch.rs` Settings toggle), `agent_launch.toml` (`status_hooks_enabled`)

Two connected changes that make the OSC-9999 producer path reliable and user-controllable.

- **Relay RPC emission** (`c8a3de3`): Claude Code runs hook commands in a detached session (no controlling terminal), so the previous `/dev/tty` emitter never reached the agent PTY. Status now routes through the relay: a new `oximux agent-status` CLI (invoked by the hook) reads `OXIMUX_PTY_ID` and sends `Request::AgentStatus` to the daemon, which frames the payload as OSC-9999 on the PTY's output stream where the existing scanner decodes it. Registry fans the packet to live subscribers only. Shell emitter removed. Protocol bumped v5 → v6 (`relay-v6.sock`).
- **Settings toggle** (`d6ea6bf`): the `OXIMUX_STATUS_HOOKS` env-var gate replaced by a persistent toggle in Settings → Agents ("Status hooks"). Stored as `status_hooks_enabled` in `agent_launch.toml`; read at spawn and OR-combined with the env var (debug escape hatch retained).

---

### 2026-06-21 — Agents dashboard: live tool subline on status cards (cockpit rebuild Phase 2b)

**Commits**: `5c07a6b` (feat: show live tool on dashboard agent cards)
**Touches**: `crates/app/src/shell/agents_dashboard/` (card render), `crates/app/src/shell/workspace_root.rs` (live `LatestStatusMap` upgrade)

Line 2 of each dashboard agent card now renders the live tool step from the OSC-9999 sideband detail (`AgentSnapshot.detail`). Lands the `LatestStatusMap` → `AgentSnapshot` upgrade deferred from Phase 1.

- Per-session status watcher records structured sideband detail into a live workspace-keyed map on `WorkspaceRoot`; map is threaded to the dashboard alongside the existing activity tail.
- Detail is held only while the agent is Running and cleared otherwise — no stale tool name lingers between tool calls.
- Card prefers the sideband detail over the log-tailed activity line when available (e.g. "Bash · cargo build" instead of raw output tail).

---

### 2026-06-21 — Agents: opt-in OSC-9999 status hooks for Claude Code (makes the sideband live)

**Commits**: _(local, pending; branch `feat/agent-sideband-phase1`)_
**Touches**: `crates/app` (new `agent_status_hooks.rs`, `assets/hooks/oximux-status-emit.sh`), `crates/agents` (debug log)

The producer side of the OSC-9999 status sideband. With `OXIMUX_STATUS_HOOKS=1`, launching a Claude Code agent injects a `--settings` hooks block so Claude emits structured status into its PTY, which the existing scanner reads — closing the gap where the regex status machine can't see the agent's internal tool steps.

- A small POSIX-sh hook script writes `ESC]9999;{"v":1,"state":"working","tool":"Edit"}BEL` to the controlling terminal (`/dev/tty`) on `PreToolUse` (working), `PermissionRequest` (needs_approval), and `Stop` (idle). Hook stdout is captured by Claude, so the packet must go to the tty; tool name is extracted with `sed` (no `jq`/`python` dependency).
- The hooks are passed as a `claude --settings <json-string>` at spawn — app-owned, never written into `~/.claude`. Because `--settings` replaces the `hooks` key, the user's existing global hooks are read and merged in so they keep firing. Hooks run `async` so they never slow the agent.
- **Opt-in, default off** (`OXIMUX_STATUS_HOOKS=1`). Claude Code only for now.

Tests: `cargo test -p oximux-app --lib` = 927 passed. A round-trip test runs the real hook script in a subprocess and feeds its bytes through the Phase-1 scanner, proving the emit→decode format end-to-end. The live link (Claude fires the hook → `/dev/tty` → PTY → scanner) needs a running Claude session to confirm; `RUST_LOG=oximux_agents=debug` logs each decoded packet.

---

### 2026-06-21 — Agents dashboard: two-line status cards (cockpit rebuild Phase 2a)

**Commits**: _(local, pending; branch `feat/agent-sideband-phase1`)_
**Touches**: `crates/app/src/shell/agents_dashboard/*`, `left_rail`, `workspace_ops`, `workspace_root`

Phase 2a of the agent-CLI cockpit rebuild — a status-driven dashboard upgrade. Independent of Phase 1; uses existing `AgentRow` data plus two new per-workspace maps.

- **Two-line agent cards** (`row_render.rs`): a 28px icon ring whose border carries the status color and whose brand glyph (claude-code / codex / aider / sparkles) is tinted to the verb color; line 1 = `project · branch` + verb chip + diff counts, line 2 = the live activity tail (Running) or a `Review →` CTA (action-required). Row height 38 → 52.
- **Attention treatment**: tier-0 rows (NeedsApproval / WaitingForInput) get floating-card chrome plus a 2px left accent bar so they stand out.
- **Recency secondary sort**: within an attention tier, the most-recently-active session floats to the top. The key is the latest session's `ended_at` (else `started_at`) as the raw RFC-3339 string — storage stamps one consistent format, so lexicographic ordering matches chronological (no `chrono` needed in the app crate). Sourced from SQLite in `gather_rail_db_data` and threaded like the existing adapter map.
- **`row_builder.rs`** (new): `build_agent_rows` / `widest_row_index` extracted from `model.rs`, which drops back under the size warning. The agent icon is resolved once per row in the builder, never in the `uniform_list` render closure.

Tests: `cargo test -p oximux-app --lib` = 922 passed (+5 new: recency-descending, tier-0-beats-newer-running, icon-path population). Code-reviewed (APPROVE_WITH_NITS). GUI-visual verification is environment-blocked; correctness is headless-tested.

---

### 2026-06-21 — Agents: structured OSC-9999 status sideband (cockpit rebuild Phase 1)

**Commits**: _(local, pending; branch `feat/agent-sideband-phase1`)_
**Touches**: `crates/core`, `crates/agents`, `crates/app` (5 consumer sites)

Phase 1 of the agent-CLI cockpit rebuild (`plans/260620-agent-cli-cockpit-rebuild/`). A pure library change that lets an agent (or a hook) report structured status out-of-band via an OSC-9999 escape sequence — closing the Codex/Aider `EMPTY_PATTERNS` blindness where the regex `StatusMachine` can't classify the raw TUI. No relay protocol change, no UI behavior change.

- **New `osc_sideband.rs`** — `AgentOscScanner`, a byte-level state machine that parses `ESC]9999;{"v":1,"state":...}BEL|ST` out of the PTY `Output` stream. It runs inside the existing 50 ms poll loop (no new `TerminalEvent` variant). It **strips only the 9999 sequences** from a `cleaned` copy of the chunk and feeds *that* to the regex machine — necessary because the regex fallback ("any output → Running") would otherwise fire on a pure-sideband chunk and clobber the reported status. CSI/SGR and all other OSCs pass through untouched. 4096-byte payload cap with truncate-and-continue; per-field caps (tool 64 / input 256 / msg 512 / session_id 64).
- **`AgentSnapshot { status, detail: Option<SidebandDetail> }`** now flows on the per-session watch channel (was a bare `AgentStatus`); the regex path publishes `detail: None`, the sideband path attaches `tool` / `tool_input` / `msg`. `current_status()` still returns a bare `AgentStatus`.
- **`StatusMachine::feed_sideband`** maps the sideband state and drives it through `force()`, inheriting the terminal-state guard and blocking-entry ring wipe.
- **`poll_helpers.rs`** extracts the per-poll event processing; `runtime_impl.rs` dropped 891 → 527 LOC (cleared the 800 hard cap; its test module moved to `runtime_impl_tests.rs` via `#[path]`).

Tests: 239 `oximux-agents` (15 scanner + 8 poll-helper + 3 feed_sideband new) + 917 `oximux-app` lib, all green; `clippy -D warnings` clean on core + agents. Code-reviewed (APPROVE_WITH_NITS, nits applied). OSC number = 9999.

Deferred (by design): no agent emits OSC-9999 yet — that needs the hook installer (later phase). The `LatestStatusMap` → `AgentSnapshot` upgrade is deferred to Phase 2b (it is SQLite-sourced, independent of the watch channel).

---

### 2026-06-20 — Restored agent terminals: eliminate status/render event theft

**Commits**: _(local, pending)_
**Touches**: `crates/pty`, `crates/relay-client`, `crates/agents`

Fixed the remaining intermittent 300–343 ms clear/redraw lag in restored tracked-agent tabs. The agent status poller and terminal renderer destructively drained the same per-session queue; live tracing showed the status poller stealing 4/30 output events, matching the delayed redraw rate.

The renderer is now the sole consumer of its queue. Tracked agents opt into an independent bounded `Output`/`Exit` stream; relay restore registration atomically backfills pending lifecycle events without removing renderer events. Synthetic relay crash exits are independently delivered to both consumers. Deterministic portable and real-relay tests drain status first, then prove the renderer still receives the same output marker and exit code.

Live restored Claude Code verification: 100 type→Ctrl+U trials, zero samples over 50 ms; end-to-end p50 15.8 ms, p95 29.3 ms, max 31.0 ms. Before this fix, 13% stalled for 300–343 ms.

---

### 2026-06-19 — Terminal: fix typing latency (App Nap run-loop throttle) — now sub-frame

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/main.rs`, `crates/pty/src/backend.rs`, `crates/relay-client/src/backend.rs`, `crates/app/src/shell/terminal_view.rs`

Root-caused and fixed the post-restore typing lag with a microsecond input→echo→frame trace driven into the live app. Findings: input handling ~0.4ms and the relay transport ~0.04ms were never the problem — the GPUI **run loop / foreground executor was serviced ~75ms–1s late**, so the PTY-output drain ran long after the bytes arrived. Cause: **macOS App Nap** throttling. OxiMux only suppressed App Nap during *scoped* daemon round-trips, and it runs as a bare binary (not an `.app`), which makes App Nap apply even while frontmost.

**Fixes:**
- **Global App-Nap suppression for the whole process lifetime** (`main.rs`) — the key fix. Drain latency dropped from p50 75ms to p50 12ms.
- **Event-driven output drain** — the relay pump wakes the view (via a new `TerminalBackend::set_output_waker`) the instant output is enqueued, instead of waiting on a (throttled) poll timer. Poll remains a fallback for hidden tabs.
- **Post-input frame persistence** — a keystroke (and IME composition) keeps the render loop self-scheduling for ~10 frames so a straggler echo paints within one frame.

**Verified on the live app** (keystroke trace): typing into a **plain shell** is now **p50 2.3ms, max 20ms, zero keystrokes >50ms** — sub-frame, native-class (was 75ms+ with 100–600ms outliers). Residual lag seen when typing into a busy **Claude Code agent** is the agent's own redraw latency (measured up to 2.2s while "crunching") — inherent to the program, not the terminal. Debug trace gated behind `OXIMUX_INPUT_TRACE`.

---

### 2026-06-19 — Terminal restore: regression tests for the responsive-after-reopen invariants

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/terminal_view.rs`, `crates/app/src/shell/pane_group/e2e_tests.rs`

Investigated a report of laggy typing in a terminal reattached on app reopen. Profiling the live process showed the main thread idle at rest (no busy loop), and the restored and freshly-spawned terminals share the same backend — the responsiveness gap is the on-screen tab's PTY-drain cadence, not the restore path corrupting state. Locked that invariant with headless coverage so it can't regress into the lag symptom:

- **Placeholder → live-session handoff** (`restore_lifecycle_tests`): a pending restore placeholder is fast-poll eligible the moment it mounts; `adopt_live_session` clears the pending flag, swaps to the live session, keeps the view fast-poll eligible, and is idempotent (a duplicate delivery is a no-op, so it can't double-arm the drain task or corrupt state).
- **Visibility drives drain cadence**: an on-screen view drains at the foreground rate (low echo latency); a backgrounded one throttles.
- **Integration guard** (`active_terminal_tab_drains_fast_background_throttles`): the active tab's view is visible (fast drain) and every other tab's view is hidden (throttled), and the two flip on a tab switch.

These join the existing restore coverage (persistence round-trip of order + cosmetics + agent metadata, rank-based placement, split/multi-group tree rebuild). No hot-path behavior change in this entry.

---

### 2026-06-19 — Tab restore: preserve preview/pin/color/title + saved order

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/persisted_terminals.rs`, `crates/app/src/shell/pane_group/mod.rs`, `crates/app/src/shell/project_panes/mod.rs`, `crates/app/src/project_panes_factory.rs`

Fixes two close-and-reopen regressions reported against the tab strip.

**Preview + cosmetic state survives a relaunch**: `is_preview`, `pinned`, color tag, and custom title were runtime-only and dropped at serialize, so every restored tab came back permanent, unpinned, untinted, and renamed-to-default. They are now persisted on `PersistedTab` (serde-default → older snapshots still parse) and re-applied on restore, matching the IDE convention where a reopened window looks exactly as it was left.

**Saved tab order no longer changes on restart**: agent tabs mount asynchronously and were appended at the tail *after* the persisted order had already been applied, so any agent not already at the end of the strip jumped position on relaunch (terminal/editor/browser tabs, restored synchronously, were unaffected). Each restored tab now carries its saved visual rank and the strip is re-sorted by rank as tabs settle, so async-mounted tabs land in their saved slot regardless of mount order.

Covered by headless tests: cosmetics serde round-trip + legacy-blob back-compat, and rank-based placement (a tab placed last still lands in its saved slot, with cosmetics restored). Live GUI close-reopen walkthrough left to a manual pass.

---

### 2026-06-18 — Editor enhancement: stability, LSP wiring, tab-lifecycle UX, diff zoom

**Commits**: `aa8800f` (merge: stability + LSP + tab lifecycle + zoom + diff open-in-tab), `4213285` (file-load retry), `8bccdfd` (diff font-zoom)
**Touches**: `crates/editor/src/{editor_view.rs,autosave.rs,lsp/*,lsp_bridge.rs,file_tree/*}`, `crates/app/src/shell/{pane_group/*,diff_view/*}`, `crates/app/src/keymap_registry/inventory.rs`

Promotes the `gpui-component`-`Input`-based editor from a viewer to a working surface. The foundation decision stands: keep the `Input` code-editor widget; do NOT port a custom editor.

**Stability / data safety**: debounced autosave (1000ms, clamped) with pause/resume coordination; dirty-tab close guard (Save / Discard / Cancel); file-tree refresh reconciles expanded ids by path (no collapse on save); LSP empty-`didOpen` read-race fixed; restored active-tab index clamps to a valid range.

**LSP in the production open path** for Rust / TS-JS / Python (pyright) / Go: extension→server resolution table (missing-binary skips cleanly), `attach_lsp` wired at the real file-open site, plus completion, hover, and go-to-definition providers.

**Tab-lifecycle UX**: single-click opens a reusable **preview tab** (italic), promoted to permanent on edit / double-click; an open file deleted/renamed on disk gets a strikethrough **external-mutation badge** and is never auto-closed (buffer preserved); transient file-read failures **retry** at 250ms/1s/2.5s with a loading/failed-retry state (terminal errors — not-found / permission — fail fast). Scroll/cursor survive tab switches (GPUI keeps background tab entities alive).

**Font zoom**: Cmd+= / Cmd+- / Cmd+0 zoom editor text (editor-global, session-scoped, clamped 8–32px). The **diff body shares the same zoom**: row height + body typography scale together so larger code doesn't clip, and the sticky header, staging card, and scroll anchor track the zoomed row height; the diff section headers (copy-path, colored +N/−N, collapse, open-in-editor, sticky overlay) were already in place.

GUI-screenshot verification of the diff-zoom + failed-load states is environment-blocked (ScreenCaptureKit capture unavailable); the build runs cleanly and the behaviors are covered by headless tests where testable.

---

### 2026-06-16 — Editor: markdown rendered preview (Source / Preview / Split)

**Commits**: _(local, pending)_
**Touches**: `crates/editor/src/markdown_preview.rs` (new), `crates/editor/src/editor_view.rs`, `crates/editor/src/lib.rs`, `crates/app/src/file_http_client.rs` (new), `crates/app/src/main.rs`, `crates/app/src/lib.rs`

`.md` and `.markdown` files now open in a rendered preview instead of a raw code editor.

**View-mode toggle** in the breadcrumb header (segmented buttons, right-aligned, markdown-only): **Source** (plain Input), **Preview** (full GFM render), **Split** (resizable source-left / preview-right via `h_resizable`). Default on open = Preview. Mode is view-lifetime — resets to Preview on reopen, not persisted. `.mdx` files and all non-markdown paths are unchanged.

**Renderer** uses `gpui-component`'s existing `text::markdown` (GFM): headings, bold/italic/inline-code, tables, task lists, blockquotes, clickable links, fenced code blocks with syntax highlighting, and images.

**Local image rendering**: `gpui` defaults to `NullHttpClient` (no image loads). A new `FileHttpClient` (`file://`-only `gpui::HttpClient` impl) is installed in `main.rs` via `cx.set_http_client(...)`. `markdown_preview.rs` rewrites repo-relative `![](path)` URLs to `file://` URIs (`absolutize_image_paths`) before passing the rendered text to the widget.

**New modules**:
- `crates/editor/src/markdown_preview.rs` — `MarkdownViewMode` enum, `mode_toggle()` render helper, `render_preview()`, `absolutize_image_paths()` (pure)
- `crates/app/src/file_http_client.rs` — `FileHttpClient` struct; overrides `get()` because `http::Uri` rejects `file:///path` (empty authority) in the default path

---

### 2026-06-16 — Left-rail parity batch 2: pinning, drag UX, visual finish, row interactions

**Commits**: _(local, pending)_
**Touches**: `crates/storage/migrations/V016__workspaces_pinned.sql` (new — `pinned INTEGER DEFAULT 0`, ladder → 16), `crates/storage/src/{migrations.rs,model.rs}`, `crates/storage/src/repositories/workspace.rs` (`set_pinned`, SELECT cols), `crates/core/src/workspace.rs` (`pinned: bool`), `crates/app/src/shell/left_rail/{mod.rs,workspace_list_render.rs,row_menu.rs,workspace_card.rs,workspace_row.rs,project_group.rs,project_drag.rs}`, `crates/app/src/shell/workspace_ops.rs` (`toggle_workspace_pin`, `rename_workspace_now` pub), `crates/app/src/assets.rs` (register `pin.svg`), `crates/app/assets/icons/pin.svg` (new)

Implements all 12 research recommendations plus **workspace pinning**. Sort mode stays global; free drag-reorder stays gated to Manual.

**Pinning:** migration V016 adds `workspaces.pinned`; a pinned workspace floats to the top of its project group in *every* sort mode (`sort_workspaces` now orders `[primary, pinned…by mode, unpinned…by mode]`). Pin/Unpin sits at the top of the row menu (and right-click), shows a pin glyph, persists across restart, and is excluded from free-drag (it's already anchored). Pinned rows are tracked in the Smart-sort settle entry so a pin re-ranks immediately while attention changes still debounce.

**Drag UX:** edge auto-scroll during a drag (re-arming tick on `list_scroll` driven by `on_drag_move` band detection) lets a row drop onto a previously off-screen position; a ~3s Smart sort-settle stops rows reshuffling under the cursor; the click-vs-drag threshold is engine-provided (`DRAG_THRESHOLD = 2px`).

**Visual finish:** knockout-ring drop caret (`paint_insertion_line` gains a `bg_rail` box-shadow casing); deterministic per-project identity hue dot (`project_identity_hue`); the warm-wash investigation found the rail already renders flat opaque (no fix needed). The rail open/close **width tween (#9) was deferred** — the rail is mounted/unmounted via `left_rail_open`, not resized, so a tween would need a risky restructure for the lowest-priority P2.

**Row interactions:** `+` / `…` hidden at rest and revealed on hover; inline double-click rename (mounts a focused `gpui_component` Input, `capture_action(InputEnter/InputEscape)` + blur-commit, reuses `rename_workspace_now`); right-click context menus on workspace rows and project headers; collapse no-jump scroll anchor (`on_next_frame` offset restore).

Status: full workspace build + clippy clean, all tests pass; code-reviewed (1 medium settle/pin race + 4 minor findings applied). Live GUI-verified on a bundled debug `.app` — pinning, auto-scroll, drag-reorder, inline rename, right-click menus, and identity dots all confirmed; a missing `pin.svg` asset registration was caught live and fixed. Escape-to-cancel rename is not keyboard-testable here (macOS IME eats Escape; wiring matches the working dialog).

---

### 2026-06-15 — Left-rail drag-to-reorder: projects and workspaces, persisted via sparse-float sort_order

**Commits**: _(local, pending)_
**Touches**: `crates/storage/migrations/V014__project_sort_order.sql` (adds `sort_order REAL`), `V015__workspace_sort_order.sql` (same), `crates/storage/repositories/project.rs` (`reorder_to`, `list_ordered`, `normalize_ranks`), `crates/storage/repositories/workspace.rs` (`reorder_to_target`, `normalize_ranks`), `crates/core/src/project.rs` + `workspace.rs` (`sort_order: f64`; dropped `Eq`), `crates/app/src/shell/left_rail/project_drag.rs` (new module), `crates/app/src/shell/left_rail/project_group.rs` + `workspace_row.rs` (drag wiring), `crates/app/src/actions.rs`

Projects and workspaces in the left rail can now be reordered by dragging. A sparse-float `sort_order` column (REAL) is added to both `projects` and `workspaces` via migrations V014 and V015 (migration ladder is now at 15). A one-shot Rust backfill at `db::open` seeds existing rows from current display order so no manual migration step is needed.

**New module** `shell/left_rail/project_drag.rs`: drag payloads, `insertion_side` helper, `paint_insertion_line` (solid 2 px accent full-width drop indicator), `SidebarDragPreview` ghost chip, `WorkspaceDragConfig`. Uses Zed's stateless GPUI idiom (`on_drag` / `drag_over` / `on_drop`); `reorder_slot_value` pure helper computes the new float rank between neighbors.

**Behavioral changes:**
- Project list is now **manual-sticky** — ordered by `sort_order` via `ProjectRepo::list_ordered`, not recency. Opening a project no longer floats it to the top.
- Workspace drag is only active in `Manual` sort mode; the primary workspace row is not draggable.
- Escape cancels an in-flight drag via `cx.stop_active_drag` (first branch of `DismissOverlay` handler).

`Project` and `Workspace` core structs gained `sort_order: f64` and dropped `#[derive(Eq)]` (f64 is `PartialEq` only).

Status: build + clippy clean, unit/integration tests green. Hands-on live GUI verification outstanding. Drop indicator is full-width — GPUI `drag_over` is style-only, inset not achievable without a custom overlay.

---

### 2026-06-15 — Usage popover: floating themed card above the inline browser

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/usage_popover.rs` (new — `UsagePopover` panel-window view + `open`), `crates/app/src/shell/mod.rs` (module), `crates/app/src/shell/usage_meter.rs` (card redesign: progress bars, "% left", Session/Weekly, freshness), `crates/app/src/workspace_root.rs` (`toggle_usage_popover` + panel-window state; `push_stash_dialog` added to `panes_covered`)

Clicking the status-bar usage chip while an inline-browser tab was open showed the popover *behind* the page — the webview is a single native view layered above the GPU canvas, so a GPUI-drawn card lands underneath it, and hiding the whole webview to surface it blanks the page.

The popover is now a separate **`WindowKind::PopUp` panel window** (the app's first secondary window). macOS composites it at the popup window level, above every normal window's native child views, so the themed card floats over the page with the page still visible. The card matches the reference cockpit's status panel: a gauge-icon **"Agent usage"** header with an "Updated just now" freshness line, then a **green→amber→red headroom bar** per window (colored by remaining), a **"NN% left … Resets in X"** row, and a muted "Account usage API · Max 5x" footer. The unavailable state shows the header + "Unavailable" + the reason.

- **Dismiss** (GPUI has no cross-window outside-click event): the panel opens focused and closes when it resigns key — i.e. the moment you click anything else — plus Escape. A short re-open debounce keeps the same chip click that dismisses it (by resigning key) from instantly reopening it; the panel calls back to clear the owner's handle on self-dismiss.
- The shared card renderer (`usage_meter::render_usage_popover`) is reused; off macOS it still renders in-window (no native layering to fight).
- Also folded the **push-stash dialog** into `panes_covered` — a full-window opaque dialog (like the confirm/rename dialogs already listed), so hiding the webview under it is correct.
- Path not taken: a native `NSMenu` (works above the webview, low risk) was prototyped first but can't draw progress bars — menu items are text/icon only — so the floating card was chosen to match the reference layout.

---

### 2026-06-15 — Usage meter: drop the local-log estimate; show "Usage unavailable" when signed out

**Commits**: _(local, pending)_
**Touches**: `crates/agents/src/session_log/usage.rs` (model gutted to `UsageWindow`/`UsageSnapshot`/new `UsageState`; estimate math removed), `crates/agents/src/session_log/usage_probe.rs` (tally/budget/file-scan machinery removed; `sample()` now returns `UsageState`), `crates/agents/src/session_log/usage_oauth.rs` (`FetchError` splits `Unauthorized(msg)`/`Unreachable`/`NoToken`; 401/403 body → API error message), `crates/app/src/shell/usage_meter.rs` (Unavailable popover + plain-percent rendering), `crates/app/src/shell/status_bar.rs` (Unavailable chip), `crates/app/src/workspace_root.rs` (`usage_state` field)

When the CLI's OAuth token is invalid/expired (e.g. the user signed out), the meter used to fall back to a **local-session-log estimate** — which guesses a per-tier budget from this machine's logs and routinely floored at `~100%`, presenting a fabricated number as if it were real. That estimate is now gone entirely. The exact account usage API is the only data source; a recent exact reading still stands in for up to 30 min during a brief token refresh (unchanged), but past that the meter reports **"Usage unavailable"** with the cause, matching the reference cockpit.

- **New state model.** `UsageState` is `Available(snapshot)` or `Unavailable { reason }`. The status-bar chip shows exact `NN% 5h · NN% wk` when available (no more `~` prefix — the estimate that justified it is gone) or a warn-colored **"Usage unavailable"** chip otherwise; the popover shows window detail or the failure reason.
- **Reason names the cause.** `401/403` → the API's own message (e.g. *"Invalid authentication credentials"*); no stored token → *"Not signed in"*; offline/timeout → *"Usage data is temporarily unavailable"*. A long backoff on the no-token case still avoids re-prompting the Keychain every tick.
- **Removed:** `UsageSource`, `budget_for_tier`/`TierBudget`, `weighted_tokens`, the 5-hour/weekly token-bucket windows, the `.claude/projects/**/*.jsonl` scan + per-file tally cache, and all their tests — the meter no longer reads session logs at all.

Verified: clean build (zero warnings), clippy clean (touched files), 31 agent + 21 app usage/status-bar tests green (new: unavailable-without-token, fresh-cache-serves, stale-cache→unavailable, per-cause reason map, 401-body parse). GUI screenshot verify of the live signed-out state outstanding.

---

### 2026-06-15 — Inline browser: send a modern Safari User-Agent (sites stop serving the legacy page)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/native.rs` (`BROWSER_USER_AGENT` + `WebViewBuilder::with_user_agent`)

The inline browser sent no explicit User-Agent, so `wry`/WKWebView used its bare default — which omits the trailing `Version/<n> Safari/<build>` token. Major sites (Google especially) read that absence as an unknown/legacy browser and fall back to a stripped-down page that also ignores `prefers-color-scheme` (why Google rendered the old light-only results layout instead of the modern SPA). The webview now sends a current desktop **Safari** UA (`…AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.3 Safari/605.1.15`).

- **Safari, not Chrome.** The reference cockpit spoofs a Chrome UA because it *is* Chromium; OxiMux's engine is WebKit, so a Safari UA is honest to the engine. A Chrome UA on WebKit would invite Blink-only code paths and `sec-ch-ua` client-hint mismatches the engine can't satisfy. The `Intel Mac OS X 10_15_7` platform token is what real Safari reports on every Mac (incl. Apple Silicon) by design.
- Applied at build time for every profile/webview; bump the `Version/` number as Safari advances.

---

### 2026-06-15 — Element picker: default click copies the element's context as text (not a screenshot)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/agent_context.rs` (picker JS payload + `format_pick`/`PickPayload`), `browser_view/render.rs` (picker tooltip)

The inline browser's element picker (crosshair) used to copy a **screenshot of the element** on a plain left-click, with the text context only reachable via the `C` key. That's now flipped to match the reference cockpit: a left-click copies the element's **context as pasteable text** by default, and a screenshot is the secondary action (`S` key or the ⋯ menu's "Copy screenshot"). The text copy is the generally-useful grab — it drops straight into a search box, an editor, or an agent prompt.

- **Enriched, plain-text format** (was a terse markdown stub): a `Attached browser context from <url>` header (query/fragment stripped so search terms / tokens don't ride along), then `Selected element:` with tag, **accessible name** (ARIA `aria-label`/labelledby → button/link/label text → title/alt/placeholder), **role** (explicit or implicit), tag-qualified **selector** (`textarea#APjFqb`), **dimensions**, text content, **nearby context** (associated labels, `aria-describedby`, placeholder, sibling text), computed styles, an HTML excerpt, and the element's **ancestor path** (`tag[role=…]` chain) + **full DOM path** (selector chain from `body`).
- The ⋯ chip still offers the narrower facets (Copy element / HTML / styles / text), now plus **Copy screenshot**; bare `C`/`A`/`S` accelerators and the standalone camera button are unchanged. The page-controlled payload stays clamped and is treated as inert text.

Verified: clean build (zero warnings), clippy clean (touched files), picker/context lib tests green (incl. the rewritten format test asserting the header strips the query string).

---

### 2026-06-14 — Browser cookie import: cascade menu, Chromium + Firefox → active profile

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/cookie_import/` (new module — `mod.rs` types+orchestration, `catalog.rs` browser table, `detect.rs` detection+profile enumeration, `read.rs` SQLite readers, `decrypt.rs` Chromium AES, `from_file.rs` JSON-export parser, `inject.rs` pure helpers), `browser_view/native.rs` (`import_cookies` objc2 `WKHTTPCookieStore` write, `popup_menu_tree`, `menu_anchor`), `browser_view/native_menu.rs` (nested-submenu `MenuEntry`/`popup_tree`), `browser_view/mod.rs` (`open_profile_menu` cascade + dispatch + `apply_import_result`), `crates/app/Cargo.toml` (`rusqlite`, `tempfile`, `aes`/`cbc`/`pbkdf2`/`sha1`/`hmac`; objc2 cookie features)

The inline browser can now import session cookies from installed browsers into the **active** webview profile, so the user lands logged-in without re-authenticating. The entry point is a cascade in the existing native profile menu (modeled on the reference cockpit, not a modal wizard): **Import Cookies → From `<Browser>` → `<source profile>`**, plus **From File…**. A browser with a single profile collapses to one row; multi-profile browsers expand to a submenu of their named profiles. The submenu only appears when at least one importable browser is detected.

- **Sources (v1):** the Chromium family — Chrome, Brave, Edge, Arc, Comet, Vivaldi, Opera, Chromium — and Firefox. Safari is excluded (its cookies use the `Cookies.binarycookies` binary format, not a SQLite DB). Detection requires a real cookie DB on disk (a never-launched browser is skipped); profiles come from Chromium's `Local State` → `profile.info_cache` name map and Firefox's `Profiles/` directories.
- **Reading is lock-safe.** Each source cookie DB (+ its `-wal`/`-shm` sidecars) is copied to a temp dir and opened read-only, so a *running* browser's WAL lock never blocks the import and the source is never touched.
- **Chromium decryption.** Encrypted (`v10`/`v11`) cookie values are decrypted with AES-128-CBC under a key derived from the browser's macOS Keychain item (`<Browser> Safe Storage`) via `PBKDF2-HMAC-SHA1(secret, "saltysalt", 1003)`; the Chromium-127+ 32-byte per-host HMAC prefix is stripped. Reading the Keychain triggers a one-time macOS access prompt — always user-initiated (the menu pick). Firefox values are plaintext. Cookie values are never logged.
- **Injection** goes through the live webview's own per-profile data store (`configuration().websiteDataStore().httpCookieStore()`), so cookies land in exactly the active isolated profile `wry` bound at build time and persist across reloads. `secure`, `sameSite` (lax/strict), and absolute expiry are carried over; Google "integrity" cookies (`SIDCC`, `__Secure-1PSIDCC`, `__Secure-3PSIDCC`, `__Secure-STRP`, `AEC`) are dropped — they're bound to the source browser's TLS fingerprint and would reject the transplanted session.
- **From File…** imports a browser-extension cookie export (Cookie-Editor / EditThisCookie JSON array): `domain`/`name`/`value` required, `path`/`secure`/`httpOnly`/`sameSite`/`expirationDate` optional, lenient about field types.
- A confirmation toast floats over the page after import ("Imported N cookies from `<Browser>` — reload to apply"); failures (Keychain denied, no DB) surface as a toast too. The read+decrypt runs on a background thread; only the WebKit cookie write runs on the main thread.
- **Infrastructure:** `native_menu` gained nested-submenu support (a `MenuEntry` tree whose leaves carry caller ids; `popup_tree` returns the chosen id) shared with the flat menu via a new `present` helper. macOS-only (Keychain + `WKWebsiteDataStore`); the cascade is omitted off-platform.

Verified: clean build (zero warnings), clippy clean (new code), 884 app lib tests incl. 13 new cookie-import tests (decrypt v10 round-trip + 127-HMAC strip, Local-State profile parse, Chromium/Firefox readers, expired-drop, Google integrity filter, JSON-export parse). **Live GUI run confirmed end-to-end**: the profile menu's 3-level cascade opens without crash (Profiles → Import Cookies → From Google Chrome → every Chrome profile enumerated by name; Brave + Comet also detected), and importing a Chrome profile's cookies into the active OxiMux profile then navigating to Gmail logged straight in as that account — no re-auth, Keychain decryption path working, even Google's session surviving the integrity-cookie filter. The earlier latent double-borrow risk (NSMenu modal loop pumping a queued GPUI task) did not fire — the `cx.spawn` deferral opens the menu outside the event-handler borrow, and the longer-open cascade did not reproduce it.

---

### 2026-06-14 — Usage meter: exact-path resilience (no more false ~100%), account-safe

**Commits**: _(local, pending)_
**Touches**: `crates/agents/src/session_log/usage_oauth.rs` (read-only fetch returns `Result<_, FetchError>` distinguishing no-token vs transient; curl surfaces HTTP status; `CLAUDE_CONFIG_DIR`-scoped Keychain service name), `crates/agents/src/session_log/usage_probe.rs` (last-known-good exact cache with 30-min staleness cap, failure-kind-keyed backoff), `crates/agents/src/session_log/usage.rs` (`UsageSnapshot.captured_at_ms` freshness field), `crates/app/src/shell/usage_meter.rs` (`format_time_ago` + cached-reading popover disclosure), `crates/agents/Cargo.toml` (`sha2`)

The status-bar usage meter was pinned at "~100% estimated" while the real account panel read ~17–26%. Root cause: the exact account-API path (`GET /api/oauth/usage`) silently falls back to a local session-log *estimate* whenever the CLI's OAuth access token is expired (it lives ~8 h and the call 401s) — and that estimate can never match the server number (it sees only this machine's logs and guesses per-tier budgets + a model weighting), so it floored at 100%.

The fix keeps the meter on the exact number and is strictly **account-safe** — it never mints, refreshes, rotates, or writes credentials, and never calls the OAuth token endpoint (doing so impersonates the official client and risks an account ban). It only (a) reads the token the official CLI already minted and (b) makes the read-only usage GET.

- **Last-known-good exact cache.** A successful account-API reading is cached; when a later tick can't reach the API (token expired mid-refresh, brief offline), the meter keeps showing that exact reading instead of dropping to the estimate. The cached percentages are slightly stale but the reset timestamps are absolute (still count down correctly), and normal CLI use refreshes the token within a tick or two. The local estimate now renders only when there is genuinely no prior exact reading and no way to get one.
- **Failure-kind-keyed backoff.** A missing/declined token backs off long (15 min — avoids re-prompting the Keychain every tick); an expired/unreachable token backs off one tick (60 s) so the meter recovers promptly once the official CLI refreshes the token during normal use. The fetch now distinguishes the two via the HTTP status (`curl` reports `%{http_code}` instead of swallowing it under `--fail`).
- **`CLAUDE_CONFIG_DIR`-scoped Keychain.** CLI 2.1+ scopes its OAuth Keychain item by config dir (`Claude Code-credentials-<8 hex of sha256(dir)>`); with `CLAUDE_CONFIG_DIR` set we now try the scoped item before the legacy unsuffixed one (and read the on-disk fallback under the same dir). Previously such setups never reached the exact path at all.
- **30-minute staleness cap** on the last-known-good reading, so a very old exact value isn't presented as current — past the window the meter drops to the marked estimate.
- **Freshness disclosed in the popover** (following the Electron cockpit's status pattern). A cached exact reading now carries a capture time (`UsageSnapshot.captured_at_ms`); the popover keeps the real numbers but its footer reads "Showing cached usage · updated *N* ago" (`just now` / `Nm ago` / `Nh ago`) instead of passing the reading off as live. Fresh readings and the estimate keep their existing captions. The capture time is deliberately kept out of the per-tick change-detection path (it only flips when freshness actually changes) so the meter doesn't repaint every tick.

Refresh of an expired token is **delegated to the official `claude` CLI** (which the user runs constantly in this cockpit) — OxiMux reads the CLI credential read-only and never writes it, mints, or rotates it. This matches what two mature reference tools actually ship: a menubar tool (owner-aware read-only + delegated refresh) and an Electron cockpit (read-only API + last-known-good "stale" bars ≤30 min, passive refresh, no background CLI spawn). Neither direct-refreshes the CLI's token; this design follows the latter. Validated endpoint/flow facts and the account-ban rationale are recorded in project memory.

Verified: clean build, clippy clean, 230 agents lib tests + 871 app lib tests, incl. 6 new (`last_known_good_exact_beats_local_estimate`, `local_estimate_used_when_no_prior_exact_reading`, `stale_exact_reading_falls_through_to_estimate`, `scoped_keychain_service_matches_cli_derivation`, `format_time_ago_buckets`, `popover_caption_discloses_cached_reading`). Live: confirmed the read-only usage GET returns the real windows (5 h 26 %, 7 d 27 %) once the token is valid; confirmed this machine carries both a plain and a scoped Keychain item.

---

### 2026-06-14 — DnD + split-panel stability: cross-group strip drop, mouse-capture dividers, drag polish

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/divider.rs` (new — shared armed-divider state + bounds cache + fraction math + bounds canvas), `project_panes/mod.rs` (`transfer_tab_at`, divider arm/resize/disarm/reset, single-terminal split spawn), `project_panes/render.rs` (mouse-capture workspace divider + capture overlay, zone-overlay cross-fade), `pane_group/render.rs` (cross-group strip drop wiring, foreign hover preview, mouse-capture sub-pane divider + overlay, ghost icon/color), `pane_group/mod.rs` (sub-divider state + methods), `pane_group/tab_drag.rs` (`source_pinned` payload field, ghost icon/color), `shell/mod.rs` (register `divider`)

Two stability defects + drag/drop polish parity. The DnD geometry already matched the reference; the gaps were structural.

- **P0 — cross-group strip drop now works.** Dropping a tab from pane A onto pane B's *tab strip* was a silent no-op (the chip `on_drop` early-returned on a same-group guard); only drops on the pane *body* moved the tab. A foreign-source drop on a destination chip now moves the tab into B at the exact insertion-bar slot under the cursor (drop on the trailing gap appends), via a new `ProjectPanes::transfer_tab_at` (append then slide to slot). The insertion bar previews the destination slot during a cross-group hover. The drop is handled on the **chip itself** (not just the strip body): GPUI's `on_drop` consumes the active drag and stops propagation on the first hovered drop-listener, so a chip that early-returns still eats the drag — the chip therefore performs the cross-group move directly (`ProjectPanes` threaded into the chip).
- **P1 — divider resize is direct mouse-capture, not drag-and-drop.** Resizing a split divider was modeled as a drag op, inheriting the framework's drag-activation dead-zone (sticky start) and broadcast-to-every-divider filtering. It's now MouseDown-arms / topmost-overlay-MouseMove-resizes / MouseUp-disarms: no dead-zone, tracks past the hitbox, and the occluding capture overlay keeps stray events off the terminals beneath. Double-click resets the split to equal. Applies to both workspace and within-tab sub-pane dividers. Parent-row bounds are captured each paint by a zero-cost measuring canvas keyed by split path.
- **P2 — drag polish.** The drop-zone overlay cross-fades (opacity, ~80ms) on zone change instead of hard-snapping; the drag ghost shows the tab's icon + color dot (and tracks the grab point — this gpui already anchors the preview at `cursor − grab_offset`); a pinned source tab now suppresses the cross-group insertion bar *and* the body-zone split overlay (prevent, don't explain — pinned tabs refuse `take_tab`).
- **P3 — single-terminal split spawns a fresh terminal.** Drag-to-edge splitting a pane whose only tab is a terminal moved that tab, emptying (and purging) the source pane. It now leaves the original terminal in place and spawns a new terminal in the new pane (focus follows). Multi-tab panes and non-terminal tabs keep move semantics. (The menu/keyboard split path already spawned a fresh terminal, so it was unchanged.)

Verified: clean build (zero warnings), clippy clean, 869 lib tests + 6 new tests green (`fraction_along` math ×4, cross-group slot landing, pinned-cluster clamp). **Live GUI run** confirmed: cross-group strip drop lands at the cursor slot + insertion-bar preview, drag-to-edge split, both workspace and sub-pane divider mouse-capture resize (no dead-zone), divider double-click reset, and the ghost icon/label. GUI testing surfaced — and fixed — two defects the build/tests missed: (1) the chip eating the foreign drop (above); (2) the divider double-click being swallowed by the transient capture overlay — fixed by giving the overlay its own `click_count≥2` handler that resolves the target via a remembered last-armed path. P3 drag-to-edge (single-terminal → spawn) is code-verified; its niche cross-group edge-drop wasn't isolated in the automation harness (zone-targeting), but the primary single-terminal-split path (menu/keyboard) was already spawn-fresh and is unchanged.

---

### 2026-06-14 — New-tab picker: "New Browser Tab" entry + command-palette polish

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/adapter_picker.rs` (new browser-tab row, leading icons, shortcut chips, section layout), `workspace_root.rs` (route the new selection through the ⌘⇧B handler), `assets.rs` + `assets/icons/square-terminal.svg` (new terminal glyph)

The `+` new-tab popover listed only "+ New terminal" and the agent adapters. It now opens an embedded browser tab too, and the whole menu picked up a command-palette finish.

- **New Browser Tab quick action** sits beside New Terminal at the top; selecting it reuses the same root handler the ⌘⇧B keybinding fires, so menu and shortcut open the tab through one path.
- **Leading glyphs** on every row — a terminal mark, a globe, and each agent's brand icon (reusing the card icon map) — in a fixed box so labels align.
- **Shortcut chips** show each quick action's *live* chord (⌘T / ⌘⇧B) pulled from the keymap registry, so a rebind stays accurate; the "default" / "not installed" agent hints keep their muted right-aligned text.
- A separator splits the quick actions from the agent list; the card widened a touch to fit glyph + label + chip.

Verified: clean build, 13 picker unit tests green, live-verified — menu renders with icons/chips and New Browser Tab opens a tab.

---

### 2026-06-14 — Browser toolbar menus: native `NSMenu` dropdowns replacing injected in-page panels

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/native_menu.rs` (new — `NSMenu` helper), `native.rs` (anchored pop-up + coordinate mapping), `mod.rs` (deferred open + apply), `render.rs` (button-bounds capture), `agent_context.rs` (JS menus demoted to off-macOS fallback), `Cargo.toml` (`NSMenu`/`NSMenuItem`/`NSEvent` features)

The page-theme and profiles dropdowns were HTML injected into the page's shadow DOM — the only layer guaranteed to sit above the native webview, since anything GPUI paints lands *under* it. They now render as real native `NSMenu` dropdowns (system font, native ✓, vibrancy), the way the rest of the OS draws button menus.

- **Why injection was needed, and why a native menu sidesteps it:** GPUI paints to one Metal canvas with the webview layered on top, so a GPUI-drawn menu would hide behind the page. `NSMenu` pops in its **own window** that the window server composites above everything — both the canvas and the webview — so no in-page injection.
- **Crash fixed (double-borrow):** `popUp` runs a nested run loop that pumps the app's main-thread tasks. Calling it inside the click handler — which holds the GPUI `App` `RefCell` borrow — let a pumped task re-enter `App::update` and abort (`panic_already_borrowed`). The menu now opens from a deferred foreground task, so the handler returns (releasing the borrow) first.
- **Anchored under the button:** a zero-cost measuring `canvas` behind each trigger button records its window-relative bounds each paint; the menu maps those into the window-root view's space (reusing the webview-pin Y-flip) and drops from the button's bottom-right with a small gap, falling back to the mouse if bounds aren't captured yet.
- **Callbacks:** a small `define_class!` `NSMenuItem` target records the picked row's tag; the appearance pick applies inline, the profile pick routes through the existing deferred-to-render path (a profile switch rebuilds the webview, which needs a `Window`).
- **Off-macOS** keeps the injected-HTML menus as a fallback (no AppKit menu to pop there). The element picker stays in-page by necessity — it reads DOM under the cursor, not chrome.

Verified: clean build (zero warnings), 22 browser_view tests green, adversarial review clean, live-verified placement.

---

### 2026-06-14 — Browser DevTools: docked inside the pane (page over inspector) replacing a standalone window

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/native.rs` (webview re-parent into a pane-sized container + attached-inspector dock)

DevTools opened in a separate floating `Web Inspector` window. WebKit's *attached* inspector docks into the inspected webview's superview and splits it; our webview was parented straight into the GPUI window root, so an attached inspector overran the whole app — which is why the code forced it detached into its own window (and that window sometimes re-docked and broke the app layout). Now it docks **inside the browser pane**, page-on-top / inspector-below, like a normal browser.

- **At build, the webview is re-parented into a pane-sized container view** (`wrap_for_docked_inspector`). The container is pinned to the pane's body bounds each paint (replicating wry's top-left→AppKit Y-flip against the window root); the webview fills it via autoresizing until the inspector claims its share. So WebKit's split is **confined to the pane** — it never touches the toolbar, sidebar, or other tabs.
- **`open_devtools` steers the inspector to attached** (`StartsAttached = true`) and force-attaches, the inverse of the old standalone-window steering. Visibility now gates the **container** (not the webview) so a docked inspector hides with its tab.
- **Graceful fallback:** if the container wrap can't be set up (`inspector_dock == None`), DevTools reverts to the old standalone-window path rather than spilling.

Verified: clean build, 22 browser_view tests green. Live-verify pending on your click.

---

### 2026-06-14 — Browser profiles: dropdown menu (list + New Profile…) replacing cycle + standalone "+"

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/agent_context.rs` (`profile_menu_js` + `switch_profile`/`new_profile` IPC), `crates/app/src/shell/browser_view/mod.rs` (`open_profile_menu` + deferred `ProfileRequest`), `crates/app/src/shell/browser_view/render.rs`

The profile control was a cycle-on-click button (rotate to the next store, current name only in a tooltip) plus a separate "+" button to mint a new profile — two buttons, neither showing the full list. Consolidated into one unified menu.

- **Profile button → opens a Profiles menu** listing every cookie-isolated profile with a green **✓** + a highlighted row on the active one, a divider, then **New Profile…**. Switching is one click to a *named* target (no blind cycling); the standalone "+" button is folded in and removed.
- The button **tints green** when a non-default profile is active, so an isolated store shows at a glance.
- Same in-page-menu technique as the theme menu (a GPUI menu would render under the native webview); dark glass panel, blue hover, pop animation, backdrop dismiss.
- Host owns the list: `open_profile_menu` injects it as a JSON array of `{id, name, active}` (the page can't fabricate a profile). Selecting posts `switch_profile {id}` (`"default"` or a UUID) or `new_profile`; because both rebuild the webview (which needs a `Window`, absent in the IPC callback), the choice is stashed as a `ProfileRequest` and applied on the next render — the same deferral the pick-to-agent path uses.

Verified: 22 browser_view tests green (1 new: item splice + action parse); clean build. Live-verify pending on your click.

---

### 2026-06-14 — Browser page-theme: dropdown menu (System/Light/Dark) replacing cycle-on-click

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/agent_context.rs` (`appearance_menu_js` + `AppearanceValue` + `set_appearance` IPC), `crates/app/src/shell/browser_view/mod.rs` (`open_appearance_menu`/`set_appearance`), `crates/app/src/shell/browser_view/render.rs`

The page color-scheme control was a cycle-on-click button (System → Light → Dark, the current state visible only in a tooltip) — undiscoverable and slow to reach a known state. It's now a proper dropdown menu, matching the reference browsers' disclosure pattern.

- **Click → opens a menu** listing **System / Light / Dark** with a green **✓** on the active option, so the current state is visible and any option is one click away (no more 3× cycling).
- The menu is **injected in-page** (its own shadow root), not drawn in the toolbar — a GPUI menu would render *under* the native webview (same constraint that puts the picker popover + copy toast in-page). Dark translucent panel with a "Page theme" header, blue row hover, pop animation; a full-page backdrop dismisses it (plus `Esc` when the page holds focus). Anchored top-right under the toolbar control cluster.
- The contrast button **tints green** while an override is active (System = no tint), so a non-default page theme is visible at a glance.
- Host side: `AppearanceValue` (`system`/`light`/`dark`) deserializes the menu's `set_appearance` IPC and maps onto `PageAppearance`; `open_appearance_menu` injects the menu seeded with the active slug, `set_appearance` applies the choice.

Verified: 21 browser_view tests green (2 new: active-slug marking + `set_appearance` parse); clean build. Live-verify pending on your click.

---

### 2026-06-14 — Browser copy-confirmation: in-page toast (no more toolbar icon shift)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/agent_context.rs` (`confirm_toast_js`), `crates/app/src/shell/browser_view/mod.rs` (`flash_confirm`), `crates/app/src/shell/browser_view/render.rs` (drop trailing pill)

The "✓ Copied" confirmation used to render as a pill appended to the end of the toolbar's flex row. Because the address bar is `flex_1`, the pill's arrival shrank it and shoved every icon between them to the left — a visible blink on each copy.

- **Confirmation now floats in the page**, not the toolbar: a glassy white "✓ <label>" toast pinned to the page's top-right, slides down + fades in, auto-dismisses (~1.5s). It lives in its own shadow root (a toolbar pill shifts icons; a host-drawn GPUI overlay would render *under* the native webview — same constraint that puts the picker chip in-page). Result: **zero toolbar layout shift**, and the confirmation sits where the eye already is.
- The firing toolbar button still flashes a green check (which control was pressed) — unchanged, and shift-free since it's a same-size icon swap.
- Picker results keep their own near-element chip and skip the page toast (no double confirmation); screenshot / DOM / console copies get the toast. The label is JSON-encoded into the injected JS so page-safe text can't escape the literal.

Verified: 19 browser_view tests green (1 new: toast label encoding); clean build. Live-verified on duckduckgo.com.

---

### 2026-06-14 — Browser element-picker: click-copies-image default + "⋯ More" facets

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/agent_context.rs` (picker JS + `PickPart` + `format_pick_part`), `crates/app/src/shell/browser_view/mod.rs`, `crates/app/src/shell/browser_view/render.rs` (tooltip)

The element picker used to act on keyboard keys only (`C`/`A`/`S`) — **clicking a grabbed element did nothing**, so there was no copied feedback and no choice of what to copy. Now a click performs the default copy instantly and surfaces the rest behind a "more" affordance (the disclosure pattern those reference browsers use).

- **Click → copies a screenshot of the element** (the most generally useful grab) and drops a white **"✓ Copied"** chip (green check badge) anchored above it.
- **"⋯ More" button on the chip** opens a small popover with the other facets: **Copy element** (full agent markdown), **Copy HTML** (raw `outerHTML`), **Copy styles** (CSS declaration block), **Copy text**, **Copy image again**, **Send to agent**. Native-style panel (rounded, translucent, monospace identity header, blue hover highlight, keyboard hints); a backdrop dismisses it. The chip + popover live in their own shadow root so they survive the picking overlay's teardown.
- **Keyboard accelerators preserved**: while hovering, bare `C` (copy element) · `A` (→ agent) · `S` (screenshot) · `Esc` (cancel) still copy directly — each then shows the same chip so `⋯` is reachable afterward.
- Host side: `PickPayload` gained a `part` facet (`all`/`html`/`styles`/`text`, `#[serde(default)] = all` for the `C` accelerator + older IPC); `format_pick_part` copies the bare value for the narrow facets and the full markdown for `all`.

Verified: 18 browser_view tests green (2 new: facet formatting + `part` default/parse); clippy clean on new code. **Live-verified:** popover + identity header render over duckduckgo.com.

---

### 2026-06-13 — Browser toolbar UI/UX polish (detached DevTools · stop button · https lock · group divider)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/{native,render,mod}.rs`, `crates/app/src/assets.rs`, `crates/app/Cargo.toml` (+`objc2-foundation` `NSUserDefaults`), `crates/app/assets/icons/lock.svg` (new)

Follow-up polish on the browser toolbar.

- **DevTools no longer breaks the app layout.** wry's `open_devtools` docks the inspector *attached*, which (because our webview is a window-level child of GPUI's surface) laid the inspector out across the whole window and overlapped the SCM panel / left rail. Now `open_devtools` sets the WebKit default `__WebInspectorPageGroupLevel1__.WebKit2InspectorStartsAttached = false` before `[_inspector show]`, so the inspector opens in its own standalone window that never touches the app layout. (In-pane docking the way a normal browser does it would mean re-parenting WebKit's private inspector view into a clipped container and hand-managing the page/inspector frame split — far more machinery than a local cockpit needs.) **Live-verified:** opens as a separate "Web Inspector" window, the app chrome stays intact behind it, and it closes cleanly.
- **Stop button:** the reload button swaps to a stop button while a page is loading (`window.stop()`); the loading flag is cleared on stop since `window.stop()` fires no load-finished callback.
- **https lock glyph** left of the address text for secure origins.
- **Group divider** separating the nav/address group from the agent-context + page-tools cluster.

Verified: 857 lib tests green; clippy clean on new code. Code-reviewed **SHIP** after one fix (the stop button clearing `loading` so it can't stick).

---

### 2026-06-13 — Browser P2 polish + P3 rich (copy-confirmation · DevTools · page theme · profiles · pick→agent)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/` (mod.rs, native.rs, render.rs, agent_context.rs), `crates/app/src/browser_profiles.rs` (new), `crates/app/src/{persisted_terminals.rs, project_panes_factory.rs, main.rs, lib.rs, assets.rs}`, `crates/app/src/shell/{project_panes/mod.rs, pane_group/mod.rs}`, `crates/app/Cargo.toml` (+`objc2-app-kit` NSAppearance/NSResponder/NSView), root `Cargo.toml` (uuid `serde`), `crates/app/assets/icons/{check,wrench,contrast,user}.svg` (new)

Closes the "did anything happen?" gap on the agent-context probes and adds the optional rich layer. Built on the same in-process `wry` webview — still no CDP.

- **Copy confirmation** (was silent): the firing probe button swaps to a green check + a "copied" pill (`Screenshot copied` / `DOM copied` / `Console copied` / `Element copied`) for ~1.4s via a per-result timer; a screenshot also flashes the page white once the capture lands (so the flash is never in the shot); the element picker shows an in-page `✓ Copied` bubble at the picked element. Tooltips reworded to disambiguate **Screenshot (image)** from **Copy DOM (text)** — the two used to read as one generic "snapshot".
- **DevTools** (wrench): `with_devtools(true)` at build; the button toggles the inspector (`open/close/is_devtools_open`) and tints green while open.
- **Page appearance** (contrast, cycle System→Light→Dark): macOS `NSAppearanceCustomization::setAppearance` on the webview's view (`.aqua` / `.darkAqua` / cleared) drives the embedded page's `prefers-color-scheme`; independent of the app chrome.
- **Profiles** (user + ＋): named, cookie/cache-isolated webview stores via `wry`'s macOS `with_data_store_identifier([u8;16])` (the profile UUID's bytes → a distinct `WKWebsiteDataStore`). The button cycles Default → each profile (switching rebuilds the webview against the new store, keeping the URL); ＋ creates one. The list persists as JSON in the app data dir (a `BrowserProfiles` global); the active profile persists per tab (`PersistedTabKind::Browser` gained `profile_id`, `#[serde(default)]` so pre-existing tabs restore into the default store).
- **Pick → agent** (the deferred direct-pipe): the picker's `A` key routes the formatted element into the active agent terminal via the existing `SendTextToActiveAgent` action (`C` still copies to the clipboard). The IPC callback has no `Window`, so the text is stashed and dispatched on the next render (the same path the terminal/diff views use).

UX note: appearance + profile are **cycle-on-click** rather than dropdown popovers — an inline menu would render *under* the native webview (the same reason modals set `WebviewSuppressed`), so cycling keeps the controls self-contained; a richer popover picker is a possible follow-up.

Verified: full lib suite green (857 tests, 0 fail) incl. new unit tests (pick→agent IPC parse, copy-kind button-sharing + pill labels, profile name/JSON round-trip); clippy clean on new code; full workspace build links the objc2 FFI. Code-reviewed **SHIP** — applied both flagged fixes (clear stale title/loading on a profile rebuild; mint the new-profile name inside the mutation to avoid duplicate "Profile N"). Live-GUI verification of the toolbar controls (devtools open, appearance switch, profile cookie-isolation, copy-confirmation) is **pending**.

---

### 2026-06-13 — Browser agent-context (v1 — DOM / console / screenshot / element-pick → clipboard)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/` (new: agent_context.rs; +native.rs, mod.rs, render.rs), `crates/app/Cargo.toml` (+`objc2-app-kit`, `objc2-web-kit`, `objc2-core-foundation`), `crates/app/src/assets.rs`, `crates/app/assets/icons/camera.svg` (new)

The *why* of the browser tab: hand the live page to an AI agent as pasteable context. Four read-only probes on the existing `wry` webview — **no CDP, no network capture** — each copying to the system clipboard. All driven through the webview's JS bridge (`window.ipc.postMessage` → one `with_ipc_handler` → entity event loop); screenshot is the one macOS-specific call.

- **Console / errors** (`INIT_SCRIPT`): a document-start script hooks `console.*` / `onerror` / `unhandledrejection` into capped (512) ring buffers — re-run per navigation, idempotent on SPA soft-nav. The **list-tree** button reads them back as a fenced block.
- **DOM snapshot** (`SNAPSHOT_JS` → **file-code** button): a depth-bounded tree-walker emits the interactive / landmark elements (selector · role · name, capped 200) plus title/url and an innerText snippet; the host numbers them `@ref1…` so an agent can name them back compactly.
- **Screenshot** (`native::screenshot` → **camera** button): `WKWebView.takeSnapshot` → `NSBitmapImageRep` PNG re-encode → `ClipboardItem::new_image`. The picker's `S` routes an element rect through the same path.
- **Element picker** (`PICKER_JS` → **crosshair** button): an injected shadow-root overlay highlights the element under the cursor; `C` copies a markdown payload (selector · CSS path · computed-style subset · clamped HTML · rect), `S` screenshots its rect, `Esc` cancels. Capture-phase listeners + `stopPropagation` keep the page from seeing the picker's keys; OS chords (Cmd/Ctrl/Alt) are left to the page. Hand-written — no remote-fetched lib.

Page payloads are treated as untrusted: clamped in the injected JS, the host only ever writes them to the clipboard as inert text (markdown-injection hardened — fenced blocks grow past any inner backtick run, page text is newline-collapsed), and the IPC parser fails closed on foreign messages. `wry`-pulled `objc2-app-kit` / `objc2-web-kit` / `objc2-core-foundation` are flipped to direct deps only to enable the snapshot feature flags — no new crate builds.

Verified: full workspace suite green (1150 tests, 0 fail) incl. 7 new unit tests (IPC parse + the three markdown formatters + fence-escape); clippy clean on new code; full debug build links the objc2 FFI. Code-reviewed — fixed markdown fence/newline injection via page content, a screenshot double-fire window, picker hijacking of Cmd+C/S, and added a cross-platform stub. **Live-GUI-verified** on the bundled app against real pages: DOM snapshot copies 200 `@ref`-numbered elements; the camera button lands a real PNG of the page on the clipboard (incl. on `file://`); the picker highlights the hovered element and `C` copies its selector / CSS path / styles / HTML, then tears the overlay down cleanly; the console capture round-trips both the empty-state block and a **non-empty** capture (`console.log/warn/error` + `unhandledrejection` + an uncaught `throw` via `window.onerror`, backticks/quotes intact in the fence).

**Webview focus handoff** (`native::focus_parent` = `makeFirstResponder` of the GPUI surface; `wry`'s `focus_parent`): a native webview that takes macOS first-responder — by a click into the page or the picker's `focus()` — keeps it when hidden (`isHidden` doesn't resign first-responder), which previously swallowed keyboard input. Now the webview hands first-responder back to the GPUI surface in three spots: on hide (`set_active(false)`), on picker end (`C`/`S`/`Esc` — the picker posts a `pick_cancel` IPC on `Esc`), and on the rising edge of the address bar gaining focus. **Verified live:** typing in the address bar after clicking into the page now lands in the bar (navigated to several URLs cleanly) and the non-empty console capture round-trips (`console.log/warn/error` + `unhandledrejection` + uncaught `throw` via `window.onerror`). **Not a browser bug (ruled out via control):** switching to a terminal *tab* and typing only reaches the terminal after a click into its body — but this is identical for a terminal→terminal tab switch with no browser involved, so it's pre-existing general behaviour (terminal panes take keyboard focus on a body click, not a tab activation; possibly only observable under synthetic input), not a webview/agent-context regression. **Also:** the JS-bridge probes (DOM/console/picker) need an http(s) origin — `file://` pages don't get `window.ipc` (the native screenshot still works there).

---

### 2026-06-13 — Inline browser tab (v1 — browsing; `wry` webview over GPUI)

**Commits**: _(local, pending)_
**Touches**: `crates/app/src/shell/browser_view/` (new: mod.rs, native.rs, render.rs), `crates/app/Cargo.toml` (+`wry`), `crates/app/src/shell/{mod.rs, pane_content.rs, pane_group/mod.rs, pane_group/render.rs, project_panes/mod.rs}`, `crates/app/src/{actions.rs, keymap_registry/inventory.rs, persisted_terminals.rs, project_panes_factory.rs, workspace_root.rs, assets.rs}`, `crates/app/src/shell/left_rail/project_menu.rs`, `crates/app/assets/icons/{globe,arrow-left,arrow-right}.svg` (new)

An embedded **browser tab** as a first-class pane-group leaf — open with **⌘⇧B** (or the command palette). Built on a native `wry` webview attached as a child of GPUI's Metal surface (the layering was de-risked in a throwaway P0 spike: the webview composites above the GPU canvas, lands on the element's logical bounds with no scale math, hides on demand, and never steals keyboard focus). Chosen over a macOS-only `objc2-web-kit` binding so a future Windows/Linux build keeps one webview API.

- **`BrowserView` entity** (`browser_view/`): owns the `wry::WebView`; a compact toolbar (back / forward / reload + an address bar that parses a URL, promotes a bare domain to `https`, or falls back to a search query) above a `canvas` anchor that re-pins the native webview frame to the laid-out bounds every paint. Navigation / title / page-load events ride wry callbacks into a `cx.spawn` event loop that updates url/title/loading.
- **Leaf integration**: a new `PaneGroupTabKind::Browser { url }` + `PaneContent::Browser(Entity<BrowserView>)` thread through every exhaustive match (open, persistence, tab chip, MRU, close) — no relay, no PTY. The tab chip shows a globe glyph and the **live page title** (URL host fallback).
- **Visibility**: the native view floats above the GPU canvas, so a per-render sweep in `PaneGroup` hides it when its tab isn't active, a `WebviewSuppressed` global hides it whenever any modal/overlay/context-menu/floating-terminal covers the panes, and the project-switch hide-all path hides it for backgrounded projects.
- **Persistence**: only the URL is stored (`PersistedTabKind::Browser { url }`); snapshot reads the live URL from the view so a restored tab reopens where the user left off (single-group + multi-group restore paths).

Verified: full workspace suite green (845 lib tests, 0 fail) + 6 new unit tests for URL normalization/host/encoding; clippy clean on new code; runtime AppKit-hierarchy introspection confirmed the webview attaches + shows (screen-capture being unavailable this session). Code-reviewed — fixed a project-switch webview leak, overlay-suppression gaps, and a 1-frame restore flash. Agent-context (DOM / console / screenshot / element-pick) is the next milestone (P2) on this same webview.

---

### 2026-06-13 — Settings pane visual polish (carded sections + rich agent rows)

**Commits**: _(local, pending)_  
**Touches**: `crates/app/src/shell/agent_presentation.rs`, `crates/app/src/shell/settings_modal/` (layout.rs, controls.rs, pane_agents.rs, pane_agents_launch.rs)

A visual pass over the Agents settings pane to bring it in line with the reference cockpit's polished, carded look — replacing the flat, monochrome row list with grouped panels, agent icons, and colour-coded controls. View-layer only; no behaviour or persistence changes.

- **Carded sections** (`layout.rs`): a reusable `card_surface` (recessed fill, hairline border, rounded corners, top edge-highlight) wraps each settings group as its own panel, and `section_title` gives each card a bold heading + muted description. The Agents pane now reads as two titled cards ("Commit messages", "Agent launch") instead of one undifferentiated divider list.
- **Rich agent rows** (`pane_agents_launch.rs`): each built-in agent renders with its CLI glyph in a rounded icon tile, the name (+ an accent "Default" badge when it's the default agent), the current flags/model summary, and the live controls pinned right. The identity dims when an agent is disabled.
- **Icon chips + real toggles**: the default-agent picker is now a row of icon chips (glyph + name, accent-ringed when selected) instead of a plain segmented track; the enabled control is a real pill toggle; and skip-permissions is an accent `toggle_chip` (`controls.rs`) that reads with colour when on rather than an "On/Off" word swap. Agent-icon resolution lives in a shared `adapter_icon_path` helper.

GUI-verified live: the carded pane renders, default-agent chips select with the accent ring + glyph recolour, the "Default" badge appears on the chosen agent's row, skip-perms toggles between accent/muted with the flags summary updating, and edits persist through the file-watcher. Full workspace suite green (839 lib tests); clippy clean on the touched files. This also closes the outstanding visual-smoke check from the agent-launch-settings entry below (screen capture worked this session).

---

### 2026-06-13 — Agent launch settings + relay argv (protocol v5)

**Commits**: _(local, pending)_  
**Touches**: `crates/settings/src/agent_launch.rs` (new), `crates/settings/src/lib.rs`, `crates/app/src/agent_launch_settings.rs` (new), `crates/app/src/lib.rs`, `crates/app/src/main.rs`, `crates/app/src/shell/settings_modal/` (mod.rs, pane_agents.rs, pane_agents_launch.rs [new]), `crates/app/src/shell/adapter_picker.rs`, `crates/app/src/workspace_root.rs`, `crates/app/src/project_panes_factory.rs`, `crates/agents/src/runtime.rs`, `crates/agents/src/runtime_impl.rs`, `crates/agents/src/cli/` (claude_code.rs, codex.rs, aider.rs), `crates/relay-proto/src/messages.rs`, `crates/relay/src/registry.rs`, `crates/relay/src/server.rs`, `crates/relay-client/src/backend.rs`, `crates/app/src/relay_supervisor.rs`

A Settings → Agents "Launch defaults" section, so the one-click launcher can apply per-agent defaults — matching the reference cockpit's agent-settings screen. Configurable per agent: extra CLI flags (a one-tap skip-permissions toggle), a default model, and enable/disable; plus a default agent surfaced first in the picker.

- **Settings model** (`agent_launch.rs`): `AgentLaunchSettings { default_agent, yolo_defaults_migrated, agents: { <id>: { args, model, disabled } } }`, persisted to `agent_launch.toml` (TOML + GPUI global + debounced file-watcher, same pattern as the other settings files). A `split_args` helper shell-splits the free-text args (quote-aware) at launch.
- **Skip-permissions ON by default** (matching the reference cockpit): on a fresh profile, a one-shot migration (`seed_yolo_defaults`) back-fills each built-in's skip-permissions flag (`--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` / `--yes-always`) and persists the file, so the first one-click launch starts the agent in full-autonomy mode. The `yolo_defaults_migrated` guard means a user who later clears a flag is never re-seeded; an agent already configured is left untouched.
- **Settings UI** (`pane_agents_launch.rs`): default-agent segmented picker + a row per built-in agent with three live chips — Enabled/Disabled, Skip-perms On/Off (toggles the agent-correct flag: `--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` / `--yes-always` in/out of the args string, preserving any other hand-edited flags), and a Model cycle. Edits write the TOML immediately; the watcher reloads + swaps the global. Both sections are searchable.
- **Launch threading**: `AgentSessionConfig` gained `extra_args`; each built-in adapter appends them after its model/effort flags and before the positional prompt. The picker `on_select` fills the model when unset and the extra args from the global; the restore path re-applies args on respawn.
- **Picker** (`adapter_picker.rs`): hides disabled agents and floats the default agent to the top with a "default" badge (stable order otherwise).
- **Relay protocol v5** (`messages.rs`, `registry.rs`, `server.rs`, `backend.rs`): `Request::Spawn` now carries `args` (the program's argv). The daemon runs `program args…` directly as the PTY leaf, so a launch WITH flags still shows only the agent's banner — no `exec` wrapper, no echoed command line. Socket bumps to `relay-v5.sock`; a fresh client spawns a fresh daemon and any stale v4 daemon idles out (the established per-version-socket drain). The runtime relay branch now always direct-spawns the resolved absolute binary with its argv, falling back to the login-shell `exec` wrapper only when abs-path resolution fails; a stdin prompt seed (aider) is written after spawn in both cases.

Verified: relay v5 daemon spawns live on launch (`relay-v5.sock`); a new end-to-end integration test spawns `/bin/echo MARKER` through the real daemon and confirms the arg reaches the child (`spawn_args_reach_child_process`); `build_command` arg ordering, settings TOML round-trip + `split_args`, the skip-perms/model toggle helpers, and picker filter/order are unit-tested. Full workspace suite green; clippy clean. GUI screenshot verification was blocked this session by a ScreenCaptureKit/Screen-Recording failure in the environment (not app code) — the visual settings-pane + live launch smoke is the one outstanding check.

---

### 2026-06-13 — One-click agent launch (default settings)

**Commits**: _(local, pending)_  
**Touches**: `crates/app/src/shell/adapter_picker.rs`, `crates/app/src/shell/adapter_picker_params.rs` (deleted), `crates/app/src/shell/mod.rs`, `crates/app/src/workspace_root.rs`, `crates/app/src/shell/workspace_ops.rs`, `crates/app/src/state.rs`, `crates/agents/src/runtime_impl.rs`

The `+` adapter picker used to open a model → effort → "Start agent" sub-step after you clicked an agent. Simplified to a single click: clicking an agent row launches it immediately with the agent's own default settings, matching the one-click launch of the reference cockpit.

- **Picker** (`adapter_picker.rs`): clicking an adapter row fires `select_adapter` → spawn + close. Removed the model/effort `Stage::Params` machinery, the per-adapter last-used preselection, and the whole params sub-step file (`adapter_picker_params.rs`). `AdapterSelection::Adapter` now carries just `{ kind, id }`.
- **Both launch entry points pass defaults**: the picker `on_select` and the create-dialog auto-spawn both call `spawn_agent_tab(.., None, None, ..)` — no model/effort flags. The session-restore path keeps its stored model/effort (`spawn_agent_tab` still accepts them), so a previously-configured restored agent is unaffected.
- **Dropped the now-unused `agent_last_params` boot snapshot + repo handle** from `AppState`. The storage repo + V009 table stay in place (dormant lib API) rather than ripping a shipped migration.
- **Clean launch display** (`runtime_impl.rs`): the relay spawn path used to run the login shell and write `exec <agent>` into it, so the terminal showed a `% exec claude` line. Now a plain launch (no argv, no stdin prompt — the one-click default) spawns the agent binary **directly** as the PTY's foreground process, so nothing echoes a command line — the terminal shows only the agent's own banner. The binary is resolved to an absolute path (via the app's PATH, which already located it at detection) so the detached daemon can `exec` it. A launch that carries argv (a restored session's model/effort) or a stdin seed still falls back to the login-shell + `exec` wrapper, which both carries the argv and resolves PATH from the user's profile. Process tree is identical either way (the agent is the PTY leaf), so cancel, exit→status, the exit banner, and auto-close are unchanged.

Verified: clicking "Claude Code" in the `+` menu spawns `claude` straight into a new tab — no intermediate picker, no `exec` line, just Claude's banner (Claude Max session live, MCP detected). 831 app-lib + 224 agents tests green (12 adapter-picker).

---

### 2026-06-13 — Agent name + count for hand-launched agents

**Commits**: _(local, pending)_  
**Touches**: `crates/agents/src/agent_title.rs`, `crates/agents/src/lib.rs`, `crates/app/src/shell/agent_presentation.rs`, `crates/app/src/shell/pane_group/` (mod.rs, render.rs), `crates/app/src/shell/project_panes/mod.rs`, `crates/app/src/shell/workspace_ops.rs`, `crates/app/src/shell/left_rail/` (mod.rs, project_group.rs, workspace_row.rs, workspace_card.rs), `crates/app/src/workspace_root.rs`

Building on the ambient-status work below: the worktree card and the tab chip now show the detected agent's NAME, and a hand-launched agent counts in the status-bar total — closer to the reference app's per-worktree agent listing, kept compact to fit OxiMux.

- **`agent_label_from_title`** resolves a display name from the OSC title (Claude Code / Codex / Gemini CLI / Aider / OpenCode / Cursor / …), whole-token matched with a `[\w./\\-]` boundary so `opencode-helper` doesn't match.
- **Card status line** renders `"Claude Code · Running"` — the name comes from a hand-launched agent's title (`AmbientAgent.label`) or, for a spawned session, the tracked adapter id mapped via `adapter_display_name`. No name resolved → the line shows the verb alone as before.
- **Tab chip** (`detect_tab_agent`) unifies a hand-launched agent terminal with a spawned one: while a recognized agent runs, the plain terminal tab adopts the agent's name, brand icon, and status dot (a user-set custom title still wins for the label). Updates live via the group's existing per-view observer.
- **Status-bar "N agents"** now adds plain terminals running a recognized agent (`ambient_agent_count`) to the spawned-tab count, so a hand-typed `claude` registers there too.

Verified live: real `cc` typed into a plain terminal flips the graphify-rs card from a stale "Claude Code · Stopped" to "Claude Code · Ready", renames the tab "Terminal 1" → "Claude Code", and bumps the status bar `0 agents → 1 agent`. Unit-tested the label heuristic (named-agent precedence, bare-glyph fallback, boundary rejection), the adapter-name map, and card-plan name carry-through.

Note: OSC title state isn't replayed when the daemon re-attaches a restored terminal (only scrollback is) — a cold-restored agent tab reads its stale tracked status and "Terminal N" until the program re-emits a title.

Scope (v1): a single name + verb per card (not an expandable multi-row list); the dashboard/nav badge unchanged.

---

### 2026-06-13 — Ambient agent status from plain-terminal titles

**Commits**: _(local, pending)_  
**Touches**: `crates/agents/src/agent_title.rs` (NEW), `crates/agents/src/lib.rs`, `crates/app/src/shell/agent_presentation.rs`, `crates/app/src/shell/pane_group/mod.rs`, `crates/app/src/shell/project_panes/mod.rs`, `crates/app/src/shell/workspace_ops.rs`, `crates/app/src/shell/left_rail/` (mod.rs, project_group.rs)

Status tracking used to be opt-in at spawn time: only a tab minted through the adapter picker got a `StatusMachine`, so a plain terminal — or a coding agent the user launched by hand (`claude`/`codex`/…) — never moved the sidebar dot. The card kept showing the last tracked session's stale state ("Stopped") while real work was happening in a terminal.

Now an agent is detected ambiently from the terminal's OSC window title, which agent CLIs rewrite as they run (already piped into `TerminalView.title`):

- **`classify_agent_title`** maps a title to a live status — a Braille spinner glyph → `Running`, an idle/awaiting marker (U+2733) → `Idle`, a stop-hand glyph (U+270B) or permission phrasing → `NeedsApproval`. Keyword fallbacks fire only when a known agent token is present (whole-token matched), so plain-shell titles (cwd, `user@host`) never register a concrete state.
- **Plain-terminal tabs are scanned** each render; the reading is keyed by worktree path. `resolve_effective_status` combines it with the tracked DB status: an *active* tracked session (working/blocking) stays authoritative, otherwise the live ambient reading overrides an absent/idle/finished one — so a hand-typed agent surfaces over a stale "Stopped"/"Done".

Verified live in a plain terminal: a working-spinner title flips a stale "Stopped" worktree to "Running" (blue); an idle marker shows "Ready" (green). Unit-tested classifier (glyph + keyword + boundary cases) and resolution precedence.

Scope (v1): left-rail cards only; the Agents dashboard and the nav unread badge still read the DB. No stale-decay timer yet — a sticky working title persists until the shell overwrites it (a follow-up could time it out). Hook-server / inline-status-protocol layers intentionally deferred.

---

### 2026-06-13 — Surface PTY exit; auto-close cleanly exited terminals

**Commits**: `3dffc99`  
**Touches**: `crates/app/src/shell/terminal_view.rs`, `crates/app/src/shell/pane_group/` (mod.rs, render.rs, sub_pane.rs, e2e_tests.rs), `crates/relay/src/registry.rs`, `crates/relay/tests/checkpoint_lifecycle.rs`

A terminal whose child process exited used to freeze on its final frame — indistinguishable from a hang (e.g. a program started with `exec`, leaving no shell to fall back to, after the user quit it). The daemon already emitted `Exit` on child death but the view dropped it. Now the exit is handled, with a hybrid close policy:

- **Clean exit (status 0) → auto-close the pane.** A lone-view tab closes the whole tab; a split leaf or a stacked leaf-tab closes just that pane and keeps the tab (cascade mirrors Cmd+W). The window-less PTY poll task emits a `CleanExit` event; the pane group queues it and closes in `render`, the one place with a window.
- **Non-zero / signalled exit → keep the pane open with a centered `⏻ process exited (code N) · ⌘W to close` banner**, so a failure stays readable (status code retained, unlike a code-blind auto-close).
- **Dead sessions never cold-restore as corpses.** An exited view is no longer persisted as a reattach target, and the daemon now replays `Exit` to a client that reconnects *after* the child already died (the daemon outlives the app) — so a re-launched app drops or respawns the slot instead of adopting a frozen, input-less pane.

Verified live (auto-close, crash banner, split-leaf close, dead-corpse drop on relaunch) and with unit tests on each rung of the close cascade plus the attach-after-exit replay.

---

### 2026-06-13 — Exact usage meter via the account usage API

**Commits**: `e0bdac5`  
**Touches**: `crates/agents/src/session_log/` (usage_oauth.rs NEW, usage.rs, usage_probe.rs, mod.rs), `crates/app/src/shell/usage_meter.rs`

The status-bar meter now shows the account's REAL rate-limit numbers instead of a local estimate. The OAuth deployment behind the primary CLI exposes `GET /api/oauth/usage` — the same window utilization its own usage panel renders, exact percentages and reset timestamps, account-wide across devices. Verified side by side: chip `16% 5h · 5% wk` and popover reset countdowns match the CLI's panel and the account settings page exactly.

- **Auth + transport**: bearer token from the CLI's Keychain credentials item (on-disk file fallback); `curl` with its config fed via stdin so the token never appears in process arguments. Ad-hoc dev bundles may re-prompt Keychain access per reseal (stable `OXIMUX_SIGN_ID` makes "Always Allow" stick); a decline backs off for 15 minutes and falls to the estimate.
- **Strategy order**: account API first, deduplicated JSONL tally as the offline/unauthenticated fallback. `UsageSnapshot` carries its source: exact numbers render without the `~` prefix, the popover drops token counts for plain percentages, the weekly line gains the real reset countdown, and reset formatting learns day spans ("6d 21h").

---

### 2026-06-12 — Live-drill fixes: usage overcount, stuck agent rows, floating-toggle focus

**Commits**: `1b50a09`  
**Touches**: `crates/agents/src/session_log/` (usage.rs, usage_probe.rs), `crates/app/src/shell/` (agent_session_persistence.rs, floating_terminal_host.rs), `crates/app/src/state.rs`, `crates/core/src/agent_session.rs`, `crates/storage/` (agent_session repo + tests)

Three defects surfaced by the round's live GUI drills, all verified fixed in a follow-up drill:

- **Usage meter counted streamed repeats.** Session logs re-emit the same assistant message (same API call, same `message.id`) on multiple lines as content blocks stream — up to ~8 repeats — inflating the token tally ~5× and pinning the weekly meter at 100%. The tally now dedupes by message id (last occurrence wins, carrying final `output_tokens`). The weekly budget multiplier was recalibrated 10 → 150 blocks against an observed in-budget heavy week (deduped ~23.6M weighted tokens reading ~70%).
- **Cancelled agents left `idle` rows forever.** `Running` decays to `Idle` at the prompt, so a tab closed (or app crashed) while an agent sat idle left a non-terminal row the running-only boot sweep never rescued — the dashboard showed "Idle" where "Stopped" belonged. The sweep now matches every non-terminal status with no `ended_at`, and the persistence watcher writes `Interrupted` itself when the status sender drops without a terminal transition (tab-close teardown can abort the poll task before its Exit event drains).
- **Floating-terminal toggle went dead after hide.** Hiding the card while keyboard focus sat inside it left focus on an unrendered element — action dispatch had no focus path, so every chord (including the re-show toggle) silently no-opped until a click. Hiding now hands focus back to the active pane.

---

### 2026-06-12 — Tabbed floating terminal: tab set, persistence, expand-to-pane

**Commits**: `cf81d6d`  
**Touches**: `crates/app/src/shell/` (floating_terminal.rs rewritten, floating_terminal_host.rs NEW, floating_terminal_persistence.rs NEW, mod.rs), `crates/app/src/workspace_root.rs`

The floating ("PiP") terminal grows from one session into a small tabbed surface:

- **Tabs.** Compact strip appears at two-plus tabs (single tab keeps the original strip-less look); click to switch, `Cmd+{`/`Cmd+}` cycle, per-tab `×`, `+` button or `Cmd+T` spawns at the active workspace cwd, double-click a chip to rename (reuses the pane rename dialog). The pane chords keep their pane meaning — the card scopes the existing actions via focus-path handlers, no new bindings.
- **Sessions survive relaunch.** The tab set persists per window (`floating_terminal.tabs.<window_id>`); on the first toggle after launch, tabs whose relay PTY still lives in the daemon reattach with scrollback intact, the rest respawn fresh at their saved cwd. This also closes a silent leak: floating PTYs already survived quit in the daemon but nothing ever reclaimed them. Entity-drop paths (last tab closed/expanded, title-bar close) write the blob immediately so a deliberately-closed set can never resurrect.
- **Expand-to-pane.** The `⤢` button moves the active tab's terminal view into the active pane group as a regular tab — same entity, the PTY keeps streaming mid-command, no respawn — then focuses it there.
- Host logic (spawn, restore round-trips, expand, rename dialog) split into `floating_terminal_host.rs`; the card itself stays PTY-free and event-driven.

---

### 2026-06-12 — Agent session persistence, Stopped status, live activity, usage meter

**Commits**: `4635afe`  
**Touches**: `crates/agents/src/session_log/` (NEW: mod, activity, usage, usage_probe), `crates/agents/src/` (runtime_impl.rs, status_machine.rs), `crates/app/src/shell/` (agent_session_persistence.rs NEW, usage_meter.rs NEW, agent_presentation.rs, agent_status_badge.rs, agents_dashboard/, left_rail/mod.rs, status_bar.rs, workspace_ops.rs), `crates/app/src/` (workspace_root.rs, project_panes_factory.rs), `crates/storage/` (V013 migration, workspace repo)

Agent observability round — the rail finally tells the truth about agents:

- **Session persistence wired end-to-end.** The `agent_sessions` table had no writer (the V001 workspace FK rejected repo-root `primary:<project_id>` launches, so the pipeline was stillborn). V013 drops the FK; a per-launch watcher now inserts the row and mirrors every status transition (+ `ended_at`) for fresh spawns AND relay re-adopts. Dashboard/rail rows show real Running / Done / Failed / Stopped, and survive restarts (boot sweep marks dangling `running` rows Interrupted — code that finally has rows to act on).
- **Cancel reads as "Stopped", not "Failed".** `cancel()` flags the session before closing the PTY; the poll loop publishes `Interrupted` instead of misreading the kill signal as `Done`/`Failed`. Presented as "Stopped" with a muted dot + "Stopped before finishing" tooltip everywhere (verb chip, rail dot, tab badge — badge was red before).
- **Live activity line.** Running primary-CLI rows show the current tool call ("Bash: cargo test…") from a bounded 64 KiB session-log tail — 2 s focus-gated background tick, 5-minute freshness gate, cleared the moment the session stops. No hooks or user setup required.
- **Status-bar usage meter.** `~NN% 5h · ~NN% wk` chip estimated from local session-log token tallies scaled by the account tier; click opens a popover with reset countdown, raw token counts, and the estimate disclosure. Hidden entirely when no data exists; budgets live in one tunable table pending live calibration. 60 s background tick with per-file parse caching.

---

### 2026-06-12 — Terminal search follow, link existence gating, bell notifications

**Commits**: `d21fadb`  
**Touches**: `crates/app/src/shell/` (terminal_view.rs, terminal_links.rs, terminal_search_state.rs, pane_group/mod.rs, project_panes/mod.rs, workspace_ops.rs, settings_modal pane_terminal + pane_notifications), `crates/app/src/notifier/` (mod.rs, mac.rs), `crates/app/src/actions.rs`, `crates/app/src/keymap_registry/inventory.rs`, `crates/app/src/workspace_root.rs`, `crates/settings/src/terminal.rs`

Terminal interaction polish — search, file links, and the bell:

- **Search match cycling follows the viewport.** Cycling (overlay Enter/⇧Enter/arrows/buttons and the new `find_next_match` ⌘G / `find_prev_match` ⌥⌘G actions) scrolls an off-screen match to mid-viewport; find-as-you-type jumps to the first hit. Works in plain and regex modes. (⌘⇧G stays with the source-control tab; both chords are rebindable.)
- **Path links underline only when real.** `path:line:col` spans confirm existence on a background task (TTL'd cache keyed by resolved path, zero filesystem IO in hover/paint); unconfirmed or missing paths never underline and never open, so version-string look-alikes can't bait a click. `~/...` paths now resolve to the home directory.
- **Bell → Notify.** Third bell mode routes BEL from a pane you can't see through the notification pipeline (master/source gates, visible-pane suppression, focus gate, per-workspace burst collapse all apply; per-pane 2 s rate limit). Banner click jumps to the ringing tab — cross-project included — via a new `bell:` banner namespace alongside the agent one. "Terminal bell banners" source toggle added to Notifications settings.

---

### 2026-06-12 — SCM section caps, split staged/unstaged counts, untracked counts, token batch

**Commits**: `2e93015`  
**Touches**: `crates/git/src/` (numstat.rs, repository.rs), `crates/core/src/git_state.rs`, `crates/app/src/shell/git_panel/` (changed_files, row_renderer, selection, mod), `crates/settings/src/` (theme.rs, motion.rs), `docs/design-guidelines.md`

Source-control panel scannability + the design-token batch:

- **Per-section line counts.** `FileStatus` now carries both unstaged (worktree-vs-index) and staged (index-vs-HEAD) counts from two batched numstat calls per poll; a partially staged file shows each section's own `+N −N` instead of one combined vs-HEAD figure on both rows.
- **Untracked file counts.** Whole-file `+N` via a bounded `git diff --no-index` pass: first 20 untracked rows, files over 1 MB skipped, (path, mtime) cache → zero git spawns at steady state, concurrency capped at 4.
- **Section row cap.** Changed/staged/untracked sections render 12 rows plus a "View all (N more)" footer that expands in place; tree mode counts folder rows against the cap. Shift+Click range selection mirrors the rendered set exactly in both view modes — it can never select an off-screen row.
- **Design tokens.** `graph_lane_colors` (5-hue colour-blind-safe palette, reserved for future commit-graph lanes), `Motion::m_exit` (200 ms) + `ease_in_exit()` cubic-bezier(0.7, 0, 0.84, 0); radius-scale ratios and the hover-only scrollbar spec documented in design-guidelines.
- Commit-message heuristic updated for the count split (staged counts drive the per-file `(+A -B)` notes).

---

### 2026-06-12 — Keyboard shortcut registry: editable keybindings, TOML overrides, live rebind

**Commits**: `e492b29`  
**Touches**: `crates/app/src/keymap_registry/` (NEW), `crates/app/src/keybindings_settings.rs` (NEW), `crates/settings/src/keybindings.rs` (NEW), `crates/app/src/shell/settings_modal/` (Keybindings pane rewritten), `crates/app/src/keymap.rs` (DELETED), 9 chord-display call sites

Named-action shortcut registry becomes the single source of truth for every keybinding:

- **Registry inventory** (`keymap_registry/inventory.rs`). 47 actions with stable ids, labels, six categories, and default chords — replaces the hand-maintained `keymap.rs` + read-only settings table pair and absorbs the menu chords (⌘Q/⌘H/⌥⌘H/⌘M). Group-split and reload-commands actions ship unbound but bindable.
- **User overrides** (`keybindings.toml` in the app data dir). Flat `action_id = "chord"` table; empty string unbinds. Unknown ids and invalid chords keep the default and surface as an error toast at first window; a syntactically broken file falls back to defaults — never a crash. Chord validation rejects unknown key names (gpui's parser accepts any token).
- **Live rebind without restart**. Settings edits append `NoAction` shadows for vacated chords then re-bind every owner of an affected chord — correct through chord swaps and conflict-then-resolve sequences (covered by unit tests + a keystroke-simulation e2e). The boot keymap is never cleared, preserving the component library's text-input bindings.
- **Editable Keybindings pane**. Record-a-chord (keystroke interceptor runs before action dispatch, so bound chords are recordable; Esc cancels, ⌫ unbinds), conflict badges on every owner of a duplicated chord, per-row Reset for overrides, Reset-all, searchable via the global settings finder.
- **Registry-driven chord display**. Command palette, activity-bar tooltips, top-bar hint, welcome screen, left rail, workspace dialog, and SCM refresh tooltip all resolve glyphs live from the registry — user overrides show up everywhere; the palette's incorrect ⌘D/⌘⇧D chips on the group-split rows are gone.

---

### 2026-06-12 — Notification system overhaul, agent-awake service, Notifications settings pane

**Commits**: `aeaaada`  
**Touches**: `crates/app/src/notifier/` (mac.rs, mod.rs, null.rs), `crates/app/src/agent_awake.rs` (NEW), `crates/app/src/shell/settings_modal/` (Notifications pane NEW), `scripts/bundle-macos.sh`

Replaces the deprecated `NSUserNotification` backend and wires a full notification dispatch stack:

- **UNUserNotificationCenter backend** (`notifier/mac.rs`). Per-tab banner identifiers with replacement semantics; click round-trip through a process-global delegate; authorization request with denial latch; graceful no-op for unbundled `cargo run` binaries.
- **Dispatch gating**. Master enable + per-source enables (agent-state now; terminal-bell reserved). Hard visible-pane suppression: a pane on screen in the frontmost window never banners. 5 s per-workspace burst collapse (process-wide). Existing per-kind toggles + focus gate preserved. Persisted settings keys are back-compatible.
- **Notification click navigation**. Cross-project: switches to the owning project, selects and scroll-locates the workspace in the left rail, focuses the exact agent tab, raises the window. Stale clicks are ignored.
- **Agent-awake service** (`agent_awake.rs`). Ref-counted `IOPMAssertion` (`PreventUserIdleSystemSleep`) held while any agent session is Running. Settings toggle (default on). Unit-tested via a backend seam.
- **Notifications settings pane**. Master / source / kind / sound / focus / agent-awake toggles plus a "Send test notification" button with availability hints. Notification rows moved out of the Agents pane.
- **Bundle codesigning** (`scripts/bundle-macos.sh`). Ad-hoc by default; `OXIMUX_SIGN_ID` env override. Sealed code identity required for UNUserNotification delivery.
- Boot-infrastructure banners (relay version mismatch / respawn) routed through the same backend.

Tests: 1 898 workspace tests green; new unit coverage for gating matrix, burst gate, identifier parsing, and awake refcount.

---

### 2026-06-06 — Settings modal, Quick Open index, lifecycle scripts, Create PR + CI, floating PiP terminal

**Commits**: `cdcbe65`, `0817caa`, `778160a`, `e5dc89f`, `d0c7ba1`, `a2a6c95`, `326464a`  
**Touches**: `crates/app/src/shell/settings_modal/` (NEW), `crates/app/src/shell/command_palette/file_index.rs` (NEW), `crates/settings/src/project_scripts.rs` (NEW), `crates/app/src/project_scripts_loader.rs` (NEW), `crates/git/src/gh.rs` (NEW), `crates/app/src/shell/source_control/pr_ops.rs` (NEW), `crates/app/src/shell/source_control/ci_status.rs` (NEW), `crates/app/src/shell/floating_terminal.rs` (NEW)

Five features shipped as a batch:

- **Settings panel modal** (`Cmd+,` or left-rail cog). Five panes: Terminal, Agents, Keybindings (read-only display), Appearance, About. Terminal pane writes `terminal.toml`; Agents pane writes `commit_message_ai.toml`; the existing FSEvents file-watcher re-applies both — the modal never sets config on the global directly. `save()` and `app_data_dir()` added to both settings loaders.

- **Quick Open file index** (`Cmd+P`). `file_index.rs` shells out `rg --files` asynchronously, caps at 20 000 results, ranks and trims to 50, then opens the selected path as an editor tab. Shows an install hint if `rg` is absent. Per-project cache is invalidated on project switch. Replaces 3 hardcoded stubs in the palette.

- **Per-repo lifecycle scripts**. `ProjectScripts` + `ScriptKind` in `crates/settings/src/project_scripts.rs`; loader reads `.oximux/scripts.toml` per project (keys: `setup`, `run`, `cleanup`, `auto_setup` bool). Left-rail workspace "…" menu surfaces "Run setup / Run / Run cleanup" for defined script kinds — each spawns an interactive PTY tab at the worktree cwd (script fed via stdin, matching the relay spawn path). `auto_setup = true` triggers setup automatically after worktree create. Cleanup runs awaited before worktree delete, bounded 30 s with `kill_on_drop` force-escape.

- **One-click Create PR + CI checks** (Source Control panel). New `crates/git/src/gh.rs` wraps `gh` CLI: `GhCmd`, `available`, `is_github_remote`, `has_open_pr`, `pr_create`, `pr_checks`, `CheckRun`. SCM primary-action gains a `CreatePR` rung gated on: branch in-sync + GitHub upstream + no open PR; Push/Sync-ahead rungs remain. `pr_ops.rs` is the tokio→GPUI bridge. `ci_status.rs` renders a compact `CI passing/running/failing ✓N ✗N ●N` row from `gh pr checks`, refreshed on a 30 s throttle inside the SCM state observer (only while a PR is open). `serde` + `serde_json` added to the `git` crate.

- **Floating PiP terminal** (`Cmd+Shift+T`). `floating_terminal.rs` toggles an in-window draggable/resizable terminal card rooted at the active worktree cwd. Not a second OS window — rendered inside the GPUI layer. PTY persists across hide/show cycles; close tears it down. Window geometry is debounce-persisted to the settings repo as JSON.

---

### 2026-06-06 — Custom commands + interactive command palette

**Commits**: `e85b0cd`  
**Touches**: `crates/settings/src/custom_commands.rs` (NEW), `crates/settings/src/lib.rs`, `crates/app/src/custom_commands_loader.rs` (NEW), `crates/app/src/lib.rs`, `crates/app/src/actions.rs`, `crates/app/src/shell/command_palette/{entry,mod,palette_modal}.rs`, `crates/app/src/workspace_root.rs`, `crates/app/src/shell/workspace_ops.rs`

Two parts:

- **Command palette is now interactive.** Previously a display-only mockup; it now has type-to-filter, ↑/↓ navigation, Enter/click dispatch (built-in actions and custom commands), and Esc to close — modelled on the project picker's focus/key handling. Click and keyboard share one `activate_item` path (dispatch + close).
- **Custom commands.** Reusable prompt snippets defined in TOML, loaded from a global config (`~/Library/Application Support/dev.nhtera.oximux/commands.toml`) and a committable per-project `.oximux/commands.toml`, merged with name-keyed precedence (`load_and_merge`, pure + unit-tested). They appear in the palette under a "Custom" group; selecting one sends its prompt to the active agent via the existing `SendTextToActiveAgent` path (newline appended to auto-submit). Malformed config is skipped with a warning, never crashes load. A "Reload Custom Commands" entry and per-project-switch reload keep them fresh. No file watcher (intentional).

---

### 2026-06-06 — Pane layout presets (stacked / horizontal / bottom-terminal)

**Commits**: `efc767d`  
**Touches**: `crates/app/src/shell/pane_group/layout_presets.rs` (NEW), `crates/app/src/shell/pane_group/mod.rs`, `crates/app/src/shell/pane_group_manager.rs`, `crates/app/src/shell/project_panes/mod.rs`, `crates/app/src/actions.rs`, `crates/app/src/keymap.rs`, `crates/app/src/shell/command_palette/entry.rs`, `crates/app/src/workspace_root.rs`

Three one-click presets reshape the active project's pane layout:

- **Stacked** (panes top-to-bottom), **Horizontal** (side-by-side), **Bottom-terminal** (content on top, a terminal docked across the bottom).
- Triggered from the command palette ("Layout: …") and `⌃⇧1 / ⌃⇧2 / ⌃⇧3`.
- `apply_preset` is a pure transform over the `PaneTree<PaneGroupId>`: it rebuilds from the existing leaves, so all panes + tabs are preserved (reparent, not recreate). Active-pane focus is restored after reshape.
- Bottom-terminal docks an existing terminal-bearing group at the bottom, or spawns a new terminal group when none exists.
- Pure transform unit-tested (shapes, idempotency, leaf preservation); no new pane primitives.

---

### 2026-06-05 — Smart git button in the status bar

**Commits**: `44b1131`  
**Touches**: `crates/app/src/shell/source_control/mod.rs`, `crates/app/src/shell/status_bar.rs`, `crates/app/src/workspace_root.rs`

Surfaces the Source Control primary-action state machine as a one-click "next git step" button in the status-bar git zone:

- Renders the SAME resolved `PrimaryAction` the SCM side panel computes (cached on the panel; single resolver, surfaces never diverge).
- Click executes the resolved op end-to-end via existing methods: Commit, Stage All (stages every unstaged file), Push, Pull, Sync, Publish — no new git logic, no merge.
- Disabled/busy states come from the resolver (in-flight commit/remote ops gate the button); tooltip surfaces the resolver's context (commit counts / disabled reason).
- Local-only — no remote-host/PR integration.

---

### 2026-06-05 — Agents dashboard (attention-sorted, all-projects)

**Commits**: `bed677a`  
**Touches**: `crates/app/src/shell/agents_dashboard/{mod,model,row_render}.rs` (NEW), `crates/app/src/shell/mod.rs`, `crates/app/src/shell/left_rail/mod.rs`

Wires the previously-inert `Agents` nav item to a real all-agents dashboard:

- One virtualized (`uniform_list`) row per live/status-bearing agent across **all** projects/worktrees; dormant workspaces excluded.
- Sorted by **attention priority**: needs-input / waiting-for-approval float to the top, then running, then idle/live, then done/failed.
- Each row shows project · branch · agent name · status verb · diff `+A −B`, reusing the rich-card `agent_verb` + `diff_counts` (no duplicated logic).
- Clicking a row activates that project + workspace and focuses its agent tab via the existing `activate_workspace` path (cross-project switch).
- Long rows scroll horizontally instead of clipping in the narrow rail (`with_width_from_item` at the widest row).

Pure data layer (`model.rs`: `attention_rank`, `sort_agent_rows`, `build_agent_rows`, `widest_row_index`) is fully unit-tested.

---

### 2026-06-05 — Left-rail rich worktree cards + live diff counts

**Commits**: `003e034`  
**Touches**: `crates/app/src/shell/agent_presentation.rs` (NEW), `crates/app/src/shell/left_rail/workspace_card.rs` (NEW), `crates/app/src/shell/left_rail/workspace_row.rs`, `crates/app/src/shell/left_rail/{mod.rs,project_group.rs}`, `crates/app/src/workspace_root.rs`, `crates/app/src/shell/workspace_ops.rs`

Replaces single-line workspace rows in the left rail with two-line rich cards:

- **Line 1**: status dot + workspace name + primary/folder badge + git branch chip
- **Line 2**: agent-state verb (colored) + working-tree diff counts (`+A −B`)

**`agent_presentation.rs`** (new shared module) — `AgentVerb` struct + `agent_verb()` function; single source of truth mapping `AgentStatus` + `is_live` flag to verb label and status-token color. Both the status dot and the card line 2 delegate here; color parity is enforced by the shared function.

**`workspace_card.rs`** (new card painter) — `render_workspace_card` consuming a `WorkspaceCardPlan`; `CARD_HEIGHT_MULT = 2.2 × h_row` (documented exception in `design-guidelines.md`).

**Live diff counts** — `WorkspaceRoot` runs `run_diff_refresh_round` every 2s (focus-gated: pauses while window is unfocused); shells out `git diff --numstat` per worktree concurrently off-thread; coalesces results into `diff_counts` cache and notifies the rail. `workspace_ops.rs` reads the cached counts when refreshing the left rail.

---

### 2026-05-30 — Terminal emulator richness — Phases 1–12 (slice 2 added)

**Status**: code complete; 1163 workspace tests pass; clippy `-D warnings` clean  
**Commits**: `3b08487` (P1–P3), `b4e084f` (P4–P6), `c58fca6` (P7), `e7f316f` (P8), `2b11d2f` (P9), `1f6ef9f` (P10), `86acc23` (P11), `8b5ddc7` (P12 slices 1+1.5), pending (P12 slice 2)  
**Plan**: `plans/260529-2042-terminal-emulator-richness/`

Closes the emulator-quality gap with the reference GPUI terminals that
share OxiMux's `alacritty_terminal` + `portable-pty` backend.

#### Sprint A — base feels real
- **P1 SGR text attributes** — bold/italic/underline/strikethrough/dim now propagate from alacritty to the canvas paint via per-cell flags + per-run font weight/style overrides.
- **P2 mouse selection + gestures** — `point_to_cell` + drag-anchor selection; Cmd+C copy; double/triple click word/line; Shift-click extends.
- **P3 live scrollback** — render-side `display_offset` driven from snapshot; wheel scroll, scrolled-up chip, snap-to-bottom on send.

#### Sprint B — TUI tools work
- **P4 input encoder** — app-cursor / app-keypad modes, xterm modifier params, Alt-prefix; tested via headless E2E that drives the real keymap.
- **P5 mouse reporting** — SGR/UTF-8/X10 mouse encoding for vim/htop/tmux; respects motion modes and modifiers.
- **P6 cursor shapes** — DECSCUSR Block/Bar/Underline; live-reload-tunable blink interval; unfocused-pane dim ghost cursor.

#### Sprint C — cockpit value
- **P7 hyperlinks + path-to-editor** — OSC 8 explicit + plain-text URL/file:line detection; Cmd-click opens via editor host's new `open(path, line, col)`.
- **P8 shell integration** — OSC 7 cwd updates; OSC 133/633 prompt+command marks with green/red gutter badges; OSC 52 clipboard write (gated by setting); OSC 9;4 progress capture; ColorRequest replies; DSR cursor-position replies.
- **P9 terminal settings** — `terminal.toml` in `~/Library/Application Support/dev.nhtera.oximux/` with FSEvents-backed live reload (debounced; filtered to the settings file to skip sqlite WAL churn); knobs for scrollback, scroll multiplier, blink, dim/unfocused alphas, OSC 52 toggle, option-as-meta, bell style.

#### Tier-3 — depth + north-star
- **P10 CJK wide-character layout** — `Cell.wide` / `wide_spacer` flags; canvas advances columns by 2 for wide cells; per-row `force_width` hedge keeps mono crispness on rows with no wide chars.
- **P11 box-drawing vector rendering** — U+2500–U+257F stroked via PathBuilder; horizontal merge collapses same-color same-weight runs into one continuous stroke (no inter-cell seams); diagonals U+2571–U+2573 fall through to the font face.
- **P12 inline agent** — slices 1+1.5+2 shipped:
  - 1 + 1.5: grid-text extractors on `TerminalSnapshot` + Cmd+Shift+I / Cmd+Shift+O actions to send selection / last completed command output to the active agent's input buffer.
  - 2: `TerminalBackend::write_output(id, bytes)` seam so an external producer (an agent CLI, a replayer) can stream bytes into a `spawn_dormant` session's grid emulator without a PTY child. Portable backend override refuses live sessions (concurrency safety against the watcher thread). Same `state.advance` path the live PTY uses, so the rendered result is byte-for-byte identical.
  - Slice 3 (`block_below_cursor` element + scroll-math accounting) deferred pending dogfood signal.

---

### 2026-05-29 — Multiplexer enhancements Phase 4 — Per-pane tabs & context env

**Status**: code complete; 375 app lib tests + storage/relay/relay-client/pty, 0 failures; runtime smoke pending  
**Commits**: `9e36a2a`, `585d200`, `3c6cc06`, `c002af9`, `c8b33be`, `7b9bb85`, `2919c42`  
**Touches**: `crates/app/src/shell/context_env.rs` (NEW), `crates/app/src/shell/pane_group/sub_pane.rs`

#### feat(app): per-pane tab strips (LeafTabs)

- Each split leaf in `TerminalSplitTree` is now a `LeafTabs` tab container; a compact chip strip (chips + '+' button) renders when a leaf has > 1 tab or is freshly split
- `NewTabInPane` action (Cmd+Shift+T) and '+' button add a new shell to the focused pane; chip clicks switch the active tab
- Cmd+W cascades: per-pane tab → leaf → group tab
- `PersistedSubPane.tabs` persists the tab list (serde-default; backward compatible, no SQL migration)

#### feat(app): shell context env (`SurfaceIds`)

- Every spawned terminal carries `OXIMUX_WORKSPACE_ID` (project root path), `OXIMUX_SURFACE_ID`, `OXIMUX_TAB_ID` (minted UUIDs), `OXIMUX_SOCKET_PATH`, plus the daemon-injected `OXIMUX_PTY_ID`
- New module `crates/app/src/shell/context_env.rs` (`SurfaceIds` struct); ids persist in the per-pane layout blob and are re-injected on dormant respawn
- Agent CLI PTYs not yet threaded with context env; per-pane-tab relay reattach + cross-group multi-tab-drag repaint deferred to a follow-on

---

### 2026-05-29 — Multiplexer enhancements Phase 3 — Multi-window & tear-off tab

**Status**: code complete; 375 app lib tests + storage/relay/relay-client/pty, 0 failures; runtime smoke pending  
**Commits**: `4cae319`, `e88d00b`, `1d02cf4`, `fdb1ee7`, `fdad549`  
**Touches**: `crates/app/src/window_registry.rs` (NEW), `crates/app/src/window_factory.rs` (NEW), `crates/storage` (V005 migration)

#### feat(app): multi-window support (`WindowRegistry` + `open_workspace_window`)

- App-level `WindowRegistry` GPUI global holds a strong `Entity<WorkspaceRoot>` per window with stable persist ids (`"main"` / `"w{n}"`)
- `NewWindow` action (Cmd+N) opens a new workspace window via the reusable `open_workspace_window` factory (`window_factory.rs`)
- App lifecycle: single quit observer; last-window close calls `cx.quit()`; non-last close dismisses only that window

#### feat(storage): per-window persistence (V005 migration)

- Migration V005 adds `window_id` column to `pane_buffers` and `pane_relay_ids` (PK rebuilt; `DEFAULT 'main'` for backward compatibility)
- Settings layout key and capture/restore thread a per-window id; `capture_session` writes an open-windows manifest; boot reopens every persisted window

#### feat(app): cross-window tear-off (`MoveTabToNewWindow`)

- `MoveTabToNewWindow` action + context-menu item moves a group tab into a new window
- `TerminalBackend::detach` releases a relay attachment without killing the PTY (detach source first, then attach destination — relay client multiplexes one subscriber per `pty_id`); destination re-mounts via `attach_pty_existing`

---

### 2026-05-23 — Phase 5 / Step 05 — Pane as editor host (workspace wiring)

**Status**: complete  
**Touches**: `crates/app/src/shell/main_pane.rs`, `crates/app/src/workspace_root.rs`, `crates/app/src/shell/right_sidebar/`, `crates/editor/src/editor_view.rs`

#### feat(app,editor): phase-05 step 5 — pane-as-editor-host

- **`PaneContent` enum** (`Terminal | Editor`) — each leaf in the `MainPane` grid now holds either a terminal or an editor; grid is no longer terminal-only
- **`MainPane::open_editor_in_focused_pane(path, window, cx)`** — replaces focused leaf content with `EditorView`; same-path short-circuit prevents redundant reloads
- **`RightTab::Files`** — new tab in `RightSidebar`; always visible (no git-repo gate); hosts `FileTreeView`; `SelectFilesTab` action bound to `Cmd+Shift+T`
- **File open flow**: click file row in Files tab → `OnOpenFile` callback → `WorkspaceRoot::open_file_in_active_pane` → `MainPane::open_editor_in_focused_pane`
- **`EditorView` focus parity**: `focused: bool` field mirrored by `cx.on_focus`/`cx.on_blur`; `set_window_title` removed from `render` (multi-leaf editors cannot share one title)
- **Session behavior**: editor leaves persist for the running session; silently dropped on app quit or project switch (no restore in v1)

---

### 2026-05-23 — Phase 5 / Step 04 — File tree UI (FileTreeView + --file-tree-spike)

**Status**: 8 pure unit tests green; cargo check/clippy clean; file-size-lint pass  
**Touches**: `crates/app/src/shell/file_tree_view.rs` (NEW), `crates/app/tests/file_tree_view_unit.rs` (NEW), `crates/app/src/main.rs`, `crates/app/src/shell/mod.rs`

#### feat(app): phase-05 step 4 — file tree UI (FileTreeView + --file-tree-spike)

- **`FileTreeView`** GPUI entity subscribing to `Entity<FileTree>` (from `oximux-editor`) via `cx.subscribe_in`; `expanded_ids: HashSet<TreeNodeId>` tracks UI expand state independently of walker-visited flag
- **Lazy expand**: dir click → `tree.expand(id)` + `RowKind::Placeholder` sentinel rendered immediately; `Loaded(id)` event triggers `rebuild_rows()` swapping sentinel for real children
- **Click flow**: dir click toggles expand; file click fires `on_open: Arc<dyn Fn(PathBuf, …)>` callback
- **Raw `uniform_list`** (not `gpui-component::Tree`) — follows `FileExplorer` precedent to avoid auto-expand-on-click conflict with lazy walker model
- **`build_display_rows`** pure fn extracted; 8 unit tests cover placeholder/empty-dir sentinels, collapse/expand, H1 chevron regression
- **`--file-tree-spike`** CLI flag: standalone window against CWD; `on_open` stubs to `tracing::info!` until step 5 wires workspace

---

### 2026-05-23 — Phase 5 / Step 03 — File tree backend (ignore + notify-debouncer-full)

**Status**: 7 integration tests green; cargo check/clippy clean; file-size-lint pass  
**Touches**: `crates/editor/src/file_tree/` (NEW), `crates/editor/tests/file_tree_tests.rs` (NEW), `crates/editor/src/lib.rs`, `crates/editor/Cargo.toml`, root `Cargo.toml`

#### feat(editor): phase-05 step 3 — file tree backend

- **`FileTree` GPUI entity** (`file_tree/mod.rs`): headless; emits `FileTreeEvent::{Loaded, Refresh, WatchError}`; `cx.spawn` drives the debounced watcher event loop; `cx.background_executor().spawn` runs the walker
- **Lazy single-level walker** (`file_tree/walker.rs`): `ignore`-crate `WalkBuilder::max_depth(1)` + `filter_entry(SKIP_NAMES)`; `sort_entries` dirs-first ASCII-lowercase alpha; gitignore-aware (requires `.git` marker in root)
- **FSEvents watcher** (`file_tree/watcher.rs`): `notify_debouncer_full::new_debouncer` 200ms debounce; closure forwards into tokio mpsc; `is_ignored` + `find_node_to_invalidate` are pure free fns
- **`SKIP_NAMES` const** shared by walker + watcher — single source of truth for filter list
- **`remove_subtree`** recursive eviction on re-expand prevents grandchild node leaks and phantom `Refresh` events
- No UI wired yet — step 4 owns the `uniform_list` render and UI diffing

---

### 2026-05-22 — Phase 5 / Step 02 — Editor save round-trip + LSP textDocument lifecycle

**Status**: all automated gates pass (cargo check, clippy -D warnings, 21 editor tests, file-size-lint); smoke green  
**Code-review score**: 8/10  
**Cook report**: `plans/reports/cook-260522-1939-phase-05-step02-editor-save-roundtrip.md`  
**Touches**: `crates/editor/`, `crates/app/src/main.rs`

#### feat(editor): user-facing

- **Cmd+S** saves the buffer to disk (UTF-8, `std::fs::write`); reports error via `tracing::error` if write fails, dirty flag stays set so user can retry
- **Dirty badge**: window title shows ` •` suffix (`"OxiMux — main.rs •"`) when buffer diverges from disk; clears on successful save
- **Undo/redo with LSP sync**: Cmd+Z / Cmd+Shift+Z (gpui-component built-in) now keeps rust-analyzer in sync — `cx.observe` pattern catches silent undo/redo edits that bypass `InputEvent::Change`
- **LSP live edits**: rust-analyzer receives `textDocument/didChange` on every text change (squiggles update before save); `textDocument/didSave` on Cmd+S; `textDocument/didClose` when editor window closes

#### refactor(editor): internal

- **New module `crates/editor/src/lsp_bridge.rs`** (145 LOC): `spawn_attach_lsp` extracted from `editor_view.rs` to stay under file-size lint; handles handshake-completion catch-up `didChange` when buffer drifted during handshake window
- **LSP client API**: `did_change` / `did_save` / `did_close` accept `&lsp_types::Uri` (parse-once; eliminates per-keystroke URI allocation — H1 fix from code review)
- **`decide_change_propagation` pure fn**: extracted from observe callback; 3 unit tests; guards cursor-move no-ops and computes version increment
- **`SaveFile` action** declared in `oximux-editor` crate (crate-cycle workaround; bound in `app/src/main.rs`)
- **New integration tests** (`tests/lsp_notification_serialization.rs`, 4 tests): `didChange` full-sync JSON shape, `didSave`, `didClose`, version monotonic; no GPUI runtime needed

#### Known limitations (non-blocking for step 2)
- `dirty_set_on_change` behavioral test deferred — requires GPUI test harness (step 7)
- Edits made during LSP handshake window silently dropped until `set_lsp_client` completes; catch-up `didChange` covers the gap at handshake completion (Fix #3)
- `fs::write` is not atomic (no temp-file + rename); hardening deferred to step 8/14

---

### 2026-05-22 — Phase 5 / Step 01 — Editor + LSP spike

**Status**: code complete; 14 editor unit tests green; cargo check/clippy clean; go/no-go pending manual smoke  
**Cook report**: `plans/reports/cook-260522-0240-phase-05-step01-editor-lsp-spike.md`  
**Touches**: `crates/editor/`, `crates/app/src/main.rs`, root `Cargo.toml`

- **EditorView** (`editor_view.rs`): GPUI entity wrapping `gpui-component` Input in `code_editor("rust")` mode; `attach_lsp` spawns rust-analyzer, installs `HoverProvider`, pumps `publishDiagnostics` to `WeakEntity<InputState>`.
- **LspClient** (`lsp/client.rs`): Content-Length framing; initialize/initialized/didOpen handshake; request timeout 5s; server-initiated requests answered `{result:null}` (prevents rust-analyzer hangs on `client/registerCapability`); captures `tokio::runtime::Handle` at spawn to bridge GCD ↔ tokio.
- **LspHoverProvider** (`lsp/providers.rs`): bridges `gpui::Task` ← tokio via `handle.spawn`; `Rc<LspHoverProvider>` scoped to local executor.
- **Transport** (`lsp/transport.rs`): read/write with `Content-Length` framing; 6 unit tests including EOF-mid-header.
- **`--editor-spike` flag** (`crates/app/src/main.rs`): short-circuits normal workspace boot; opens single editor window on `crates/app/src/main.rs`; self-aborts with clear message if tokio handle not in scope.
- **`url` crate** used for `path_to_file_uri` — percent-encodes paths with spaces (prevents silent diagnostic mismatches).
- Spike is **read-only**: no `didChange`, no save, no dirty flag (step 2 owns that).

**Manual smoke** (go/no-go gate — requires interactive macOS session):
```bash
cargo run -p oximux-app -- --editor-spike
```

---

### 2026-05-22 — Phase 5 / Step 07 — relay daemon hardening

**Status**: 10 sub-steps complete; 6 new relay integration tests + 4 supervisor unit tests, all green; clippy clean  
**Touches**: `crates/relay/`, `crates/relay-proto/`, `crates/app/`, `scripts/`

- **Graceful shutdown**: `Notify`-based SIGTERM/SIGINT handler in `server.rs`; `Request::Shutdown` wired to same `Notify`; `PidGuard` cleans up pid file on drop (mirrors `SocketGuard`).
- **Idle GC + Stats**: `spawn_idle_gc` task reaps sessions idle past `ServerConfig::idle_timeout`; `PtyRegistry` gains per-entry `AtomicU64` byte counters + `started_at`; new `Request::Stats` / `Response::StatsOk(Vec<PtyStats>)` proto messages expose live PTY metrics.
- **Structured logging + log rotation**: layered tracing subscriber — stderr text + daily-rolled JSON via `tracing-appender` + macOS oslog mirror; `OXIMUX_RELAY_TRACE=1` opens trace level; `purge_old_logs` sweeps `relay.log.YYYY-MM-DD` files older than 7 days at startup; `--pid-file` and `--log-dir` CLI flags.
- **Crash heartbeat + version guard**: `relay_supervisor.rs` adds `SupervisorError::VersionMismatch`; 1Hz `watch_pid` loop; on relay death calls `on_relay_died` (sqlite orphan cleanup + AppKit banner); `VersionMismatch` shows macOS notification and parks in degraded mode — no auto-respawn.
- **Install scripts**: `scripts/oximux-launchd-install.sh` (opt-in launchd agent; `plutil`-lints plist; refuses if token absent) + `scripts/oximux-uninstall.sh` (full hygiene).

---

### 2026-05-20 — Phase 3 Step 4 + CliRuntime — CustomCommandAdapter + first concrete AgentRuntime

**Status**: step 4 + CliRuntime code complete; 16 new tests, all green (510 workspace total)  
**Plan**: `plans/reports/cook-260520-1448-phase-03-step04-cli-runtime.md`  
**Code-review score**: DONE_WITH_CONCERNS — all Critical/High items resolved in-slice

#### What shipped

| File | Role |
|---|---|
| `crates/agents/src/cli/custom.rs` | `CustomCommandAdapter` — escape-hatch `CliAgentAdapter`; reads `custom_command: Option<(String, Vec<String>)>` from config; empty `status_patterns()` (StatusMachine defaults handle output→Running / silence→Idle / exit→Done/Failed); always-detects |
| `crates/agents/src/runtime_impl.rs` | `CliRuntime` — first concrete `AgentRuntime` impl; adapter registry (`HashMap<AgentAdapter, Arc<dyn CliAgentAdapter>>`); per-session `PortablePtyBackend` + 50ms tokio poll task + `watch::channel<AgentStatus>`; `cancel` does `drain_events+close` in `spawn_blocking` then awaits poll handle with timeout+abort |
| `crates/core/src/lib.rs` | `AgentAdapter::Custom` variant; `#[derive(Hash)]` for registry keying |
| `crates/agents/src/runtime.rs` | `AgentSessionConfig::custom_command` field; `cancel()` doc notes step-13 SIGTERM-grace deferral |

#### Design notes
- `CliRuntime` is the canonical `AgentRuntime`. Future ACP runtime (v1.1) will be a sibling impl; both expose identical `watch::Receiver<AgentStatus>` to UI.
- `StatusPattern` uses `regex::bytes::Regex` — PTY output is not guaranteed UTF-8.
- `MAX_SAFE_STDIN_SEED = 4096` — soft cap; `warn!` guard on larger seeds.
- `custom_command` field on `AgentSessionConfig` is acknowledged debt (M1) — step 10 launch dialog refactors to a typed `AdapterConfig` enum.

#### Resolved during review
- C1: `drain_events()` before `close()` inside `spawn_blocking` — eliminates cancel deadlock
- H1: `select!{handle / sleep}` + abort on timeout — poll task no longer leaks on cancel timeout
- H2: `cancel()` doc now explicitly notes step-13 SIGTERM-grace deferral (impl is SIGKILL today)
- H3: `MAX_SAFE_STDIN_SEED = 4096` warn guard — large seeds won't stall the `spawn_blocking` thread silently
- M2: test `subscribe_then_cancel_publishes_terminal_status` — UI contract: badge sees final state across cancel
- M3: test `double_cancel_second_call_errors` — session table is source of truth

#### Known limitations / deferred
- No natural-exit session-table cleanup — entry stays until `cancel()` called; step 9 (pane integration) owns reaping.
- SIGTERM-grace dance deferred to step 13; current `cancel()` is SIGKILL (reap-before-resolve honored).
- No detection registry yet — step 8; until then `register_adapter()` called manually.
- `pub mod runtime_impl` may expose internals (M4) — tighten to `pub(crate)` if a 2nd internal helper goes pub.

---

### 2026-05-20 — Phase 3 Foundation (steps 1-3) — Agent Runtime Traits

**Status**: steps 1-3 code complete; 18 new tests, all green (494 workspace total)  
**Plan**: `plans/reports/cook-260520-1242-phase-03-foundation.md`  
**Code-review score**: 7/10 — all High/Medium items resolved in-slice

#### What shipped

| File | Role |
|---|---|
| `crates/core/src/agent_session.rs` | `AgentSessionId(u64)` newtype (private field); `AgentStatus` enum (6 variants); `is_blocking()` / `is_terminal()` helpers |
| `crates/agents/src/runtime.rs` | `AgentRuntime` async trait (`async-trait`); `AgentSessionConfig`; `AgentStatusStream = watch::Receiver<AgentStatus>` (multi-subscriber fan-out) |
| `crates/agents/src/cli/adapter.rs` | `CliAgentAdapter` async trait; `CommandSpec`; `StatusPattern` (regex::bytes — raw PTY is not guaranteed UTF-8) |
| `crates/agents/src/status_machine.rs` | `StatusMachine`: 1 KiB ring buffer; `feed` / `tick` (5s idle decay) / `note_exit` / `force`; 18 unit tests |

#### Resolved during review
- H1: replaced `mpsc::Receiver` with `watch::Receiver` — multi-subscriber fan-out for badge + sidebar + dashboard
- H2: ring cleared on blocking-entry transition — stale bytes can't re-match after state clears
- M2: `AgentSessionId` field made private; forgery prevented
- L1: `force()` now rejects terminal states

#### Known limitations / deferred
- No concrete adapter yet — steps 4-7 downstream. Trait surface logic-tested only.
- `current_status()` returns `anyhow::Result`; typed `AgentError` deferred to Phase 4.
- Pre-existing `cargo fmt` drift in `crates/app` (not regressed here); needs a cleanup slice.

---

### 2026-05-20 — Right-Sidebar Phase 03 — Search Panel

**Status**: code complete; 476 workspace tests passing (0 failed)
**Plan**: `plans/260517-1821-right-sidebar-panels/phase-03-search-panel.md`

#### What shipped

New `crates/app/src/shell/search_panel/` module wired into the Search tab of `RightSidebar`.

| File | Role |
|---|---|
| `mod.rs` | `SearchPanel` GPUI entity; 300ms debounce; monotonic search-id cancellation; InputState event subscriptions |
| `search_state.rs` | pure `SearchOptions` (query, case/word/regex, include/exclude globs) |
| `rg_runner.rs` | async `tokio::process::Command` spawn of `rg --json`; NDJSON stream-parse; per-file cap (100) and global cap (2000); 30s hard timeout; `kill_on_drop` |
| `rows.rs` | pure `build_search_rows` interleaver: file headers + matches, collapse handling |
| `match_render.rs` | pure VSCode-style `truncate_before` (26-byte pre-match cap); multi-byte char safe via `is_char_boundary` |
| `header_render.rs` | query input + Aa/ab/.* toggle row + include/exclude glob fields + summary banner state |
| `result_row.rs` | paint file header rows (chevron + name + match count) and match rows (line# + highlighted span) |

#### Key behaviors
- Backend: shell-out to `ripgrep --json`. Detect at startup; show install hint banner if missing.
- 300ms debounce via `cx.background_executor().timer`; monotonic `latest_search_id` drops stale results.
- Cancellation: `tokio::process::Child::start_kill()` + `kill_on_drop(true)` — no zombie processes on tab switch.
- Virtualized via `gpui::uniform_list`; file rows 28px, match rows 20px.
- Left-truncation keeps match span visible at narrow widths (BEFORE_MAX = 26 bytes).
- Empty states: rg-missing / no query / no results.
- Click file row toggles collapse; click match row opens file via `open` (editor jump deferred to Phase 5).

#### Files modified outside `search_panel/`
- `shell/mod.rs` — added module export
- `shell/right_sidebar/mod.rs` — Search tab wiring; replaces "Phase 03" placeholder
- `crates/app/Cargo.toml` — adds `serde`, `serde_json`, `thiserror` workspace deps
- Tests: `tests/search_smoke.rs`, `tests/search_rg_runner.rs`, `tests/fixtures/search/` fixture dir.
- Fixed pre-existing smoke test failures by initializing `gpui_component::init` in test setup (file_explorer_smoke, right_sidebar_smoke).

#### New tests (+27 over baseline)
- 20 lib unit tests across `search_panel/*` (pure modules)
- 6 integration tests in `tests/search_rg_runner.rs` (real ripgrep against fixtures)
- 1 gpui smoke test in `tests/search_smoke.rs`

---

### 2026-05-19 — Right-Sidebar Phase 02 — File Explorer Panel

**Status**: code complete; 439 workspace tests passing (0 failed)
**Plan**: `plans/260517-1821-right-sidebar-panels/phase-02-file-explorer.md`
**Code-review score**: 7.5 → all High/Medium items addressed before merge

#### What shipped

New `crates/app/src/shell/file_explorer/` module wired into the Explorer tab of `RightSidebar`.

| File | Role |
|---|---|
| `mod.rs` | `FileExplorer` GPUI entity; state machine; action handlers; `cx.observe_window_activation` refresh |
| `tree_state.rs` | flat-row build (`flatten`), expand toggle, `should_include` filter (skips `.git`/`node_modules`/`target`) |
| `status_display.rs` | `BadgeStatus` enum, `STATUS_LABELS`/`STATUS_COLORS`, priority ladder, folder propagation (Deleted+Ignored excluded from folder badge) |
| `row_render.rs` | `build_row_plan` pure helper → `RowPlan` consumed by `uniform_list` |
| `fs_load.rs` | async `tokio::fs::read_dir` wrapper; 5s `tokio::time::timeout` per load; symlink skip; 12-deep recursion guard |

#### Key behaviors
- Virtualized via `gpui::uniform_list`; 24px row height, 16px/depth indent; targets 10k+ rows at 60 fps
- Lazy directory load with `loaded`/`loading` flags; per-repo expanded-set persistence
- Git status badges M/A/D/R/U/C right-aligned; folder propagation shows dominant child badge
- Ignored entries rendered italic+dim; Deleted entries excluded from folder propagation
- Focus-regain refresh via `cx.observe_window_activation` (reuses cached dirs, no full rescan)
- Click file → `open <path>` (macOS default app); editor integration deferred to Phase 5

#### Plan deviations (minor)
- Symlink skip and 12-deep guard added (not in original spec) — prudent safety bounds
- Deleted entries excluded from folder propagation (spec said only Ignored excluded) — UX improvement accepted in code review
- Focus-refresh mechanism clarified to reuse cache rather than rescan

#### Files modified outside `file_explorer/`
- `shell/mod.rs` — added module export
- `shell/right_sidebar/mod.rs` — Explorer tab wiring; `window: &mut Window` threaded through `new`
- `workspace_root.rs` — passes window to `RightSidebar::new`
- `crates/app/tests/right_sidebar_smoke.rs` + `file_explorer_*.rs` — 90+ new tests
- `shell/welcome_view.rs` — one-line clippy fix

**Test delta**: +90 tests (349 → 439 workspace total)

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
