# OxiMux — Project Changelog

Entries are newest-first. Each entry links to the commit SHA and notes what shipped.

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

Implements all 12 research recommendations plus **workspace pinning** (the reference UX's model). Sort mode stays global; free drag-reorder stays gated to Manual.

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

The profile control was a cycle-on-click button (rotate to the next store, current name only in a tooltip) plus a separate "+" button to mint a new profile — two buttons, neither showing the full list. Consolidated into one menu.

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
- **Skip-permissions ON by default** (matching the reference cockpit): on a fresh profile, a one-shot migration (`seed_yolo_defaults`, mirroring the reference UX's `migrateAgentYoloDefaults`) back-fills each built-in's skip-permissions flag (`--dangerously-skip-permissions` / `--dangerously-bypass-approvals-and-sandbox` / `--yes-always`) and persists the file, so the first one-click launch starts the agent in full-autonomy mode. The `yolo_defaults_migrated` guard means a user who later clears a flag is never re-seeded; an agent already configured is left untouched.
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

Scope (v1): a single name + verb per card (not the reference UX's expandable multi-row list); the dashboard/nav badge unchanged.

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

Closes the emulator-quality gap with the three reference GPUI terminals
(gpui-terminal, crate-reorg, zed-industries/zed) that share OxiMux's
`alacritty_terminal` + `portable-pty` backend.

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
