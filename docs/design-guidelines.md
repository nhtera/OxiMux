# OxiMux — Design Guidelines (v1)

This document is the source of truth for visual identity, palette, density, typography,
and composition patterns. It drives `crates/settings/src/theme.rs`, `density.rs`, and
`typography.rs`; keep both in sync when changing any token.

When the doc is silent: read the closest sibling code in `crates/app/src/shell/` and
follow its lead. If the question is structural (which primitive, when), see the
Primitive-Picking Fork. If the question is "is there a token for X", check the tables
below first — every shipped surface routes through them.

## Brand

**Name**: OxiMux.
**One-line**: Rust-native, multi-agent development cockpit for macOS.
**Tone**: Quiet, technical, terminal-first. Not playful. Not "AI".

`Oxi` = oxidation (Rust). `Mux` = multiplexer (the cockpit metaphor: many agents,
many panes, one operator).

## Mode

Dark only in v1. Light mode deferred until after Phase 8. Every token below assumes
dark canvas.

## Palette — Monochrome Charcoal

The base is near-black with graphite panels. Status hues are the only saturated color
in the UI. This keeps the eye on the diff, terminal, and agent output — not chrome.

| Token | Hex | Use |
|---|---|---|
| `bg_base` | `#0E0F11` | Window background, dock empty area |
| `bg_panel` | `#15171A` | Sidebar, status bar, panel backgrounds |
| `bg_panel_alt` | `#1B1E22` | Hover, selected row, nested panel |
| `bg_overlay` | `#22262B` | Tooltip, popover, command palette, context menu |
| `fg_base` | `#E6E8EB` | Body text |
| `fg_muted` | `#9AA0A6` | Labels, secondary text, inactive tabs |
| `fg_subtle` | `#6B7177` | Disabled, placeholder, gutter numbers, chevrons |
| `border_inactive` | `#26292E` | Pane dividers, card edges |
| `border_active` | `#3A4047` | Focused pane border, popover outline |
| `selection` | `#2D3A4D` | Text selection background; also "currently-open" row tint |
| `focus_ring` | `#4A6E9C` | Keyboard focus outline |
| `match_bg_current` | `#D9A441` | Cycled "you-are-here" search match (Cmd+F) |
| `match_bg_other` | `#5A5358` | Non-current search matches |
| `match_fg` | `#0E0F11` | Foreground over `match_bg_current` (high contrast) |

### Status palette (single accent layer)

| Token | Hex | Use |
|---|---|---|
| `status_ok` | `#6FA86A` | Clean working tree, agent done, test pass |
| `status_warn` | `#D9A441` | Dirty tree, approval pending, lint warn |
| `status_error` | `#D26464` | Conflict, panic, test fail; row-level destructive verb |
| `status_info` | `#5B97C9` | Running, fetching, building |
| `status_muted` | `#6B7177` | Idle, paused, untracked |

### SCM-specific palette

| Token | Hex | Use |
|---|---|---|
| `status_added` | `#64D26B` | SCM diff +N count, "added" line tint |
| `status_removed` | `#D26464` | SCM diff −N count (aliases `status_error`) |
| `status_warning` | `#D2A864` | Conflict / in-progress operation banner |

### Theme helpers (alpha-composited backgrounds)

Constructed on the `Theme` struct so call sites stop hand-rolling `Hsla { a: 0.22, .. }`:

| Helper | Returns | Use |
|---|---|---|
| `theme.diff_added_bg()` | `Hsla { a: 0.22, ..status_added }` | Per-line added highlight in diff view |
| `theme.diff_removed_bg()` | `Hsla { a: 0.22, ..status_removed }` | Per-line removed highlight in diff view |

No brand accent in v1. The brand *is* the absence of accent. Adding one is a Phase 8
decision, not a Phase 0 one.

## Density

Tight is the default. The cockpit fits more on screen than the conventional desktop
editor by design.

| Token | Value (px) | Use |
|---|---|---|
| `h_top_bar` | 32 | Application top bar |
| `h_status_bar` | 24 | Bottom status bar |
| `h_tab` | 28 | Pane tab strip |
| `h_row` | 24 | Sidebar list row, file tree row (baseline) |
| `h_action_row` | 34 | Row hosting inline action buttons (stash entry, worktree entry) |
| `h_overlay_item` | 30 | Item row inside floating cards (context menus, pickers) |
| `r_card` | 8 | Panels, cards |
| `r_xs` | 4 | Buttons, inputs |
| `r_chip` | 3 | Inline chips, badges, toggle pills — intentionally tighter than `r_xs` |
| `pad_panel` | 8 | Panel inner padding |
| `pad_overlay` | 6 | Inner padding for floating cards |
| `pad_row` | 6 | Row left/right padding |
| `gap_inline` | 6 | Spacing between inline siblings |

## Typography

Single font family. Numbers tabular everywhere for diff alignment and counters.

| Token | Size (px) | Weight | Use |
|---|---|---|---|
| `t_sub_label` | 9.5 | 400 | Sub-label annotations (metadata subordinate to primary label) |
| `t_label_xs` | 10 | 500 | Tiny chips (LSP, branch) |
| `t_label_caps` | 10.5 | 600, tracking +0.5 | All-caps section labels |
| `t_body_sm` | 11 | 400 | Status bar, gutter, tooltip, file tree body |
| `t_brand` | 12 | 600 | Brand wordmark in top bar |
| `t_body_md` | 13 | 400 | Sidebar rows, body text |
| `t_body_lg` | 14 | 400 | Main content, terminal default |

**Font stack**: `"Geist Mono"`, `"SF Mono"`, `Menlo`, `monospace` (terminal + editor).
`"Inter"`, `"SF Pro Text"`, `system-ui` (UI chrome). System fallbacks ensure font-kit
always finds a face.

## Color Roles

Map semantic intent to tokens. When in doubt, reach for the role here before reaching
for a new hex value.

| Role | Token | Where it lands |
|---|---|---|
| Canvas | `bg_base` | Window background only |
| Panel surface | `bg_panel` | Sidebar, file tree, SCM panel, status bar |
| Raised / hover | `bg_panel_alt` | Row hover, alternating rows, selected row |
| Floating surface | `bg_overlay` | Pickers, context menus, dialogs |
| Body text | `fg_base` | Primary text in any surface |
| Secondary text | `fg_muted` | Labels, sub-rows, inactive tabs |
| Disabled / chevron | `fg_subtle` | Disabled buttons, gutter, chevrons, sentinel italics |
| Selection ("you-are-here") | `selection` | Text selection AND persistent-current row (file tree active file) |
| Focus / popover edge | `border_active` | Focused input, popover/context-menu border |
| Hairline divider | `border_inactive` | Section dividers, pane separators |
| Destructive verb | `status_error` | Danger button text, danger-row foreground |
| Status (signal) | `status_ok` / `status_warn` / `status_info` | Single-accent state indicators |
| Ongoing op banner | `status_warning` | Conflict banner, rebase-in-progress strip |

## List-Row Convention

One set of rules across every row surface (SCM file rows, file tree rows, search
results, picker rows, left-rail workspace rows).

| State | Background | Foreground | Extra |
|---|---|---|---|
| Idle | transparent | `fg_base` | — |
| Hover | `bg_panel_alt` | `fg_base` | — |
| Selected (click) | `bg_panel_alt` | `fg_base` | 1px `border_active` outline |
| Active / current (persistent) | `selection` | `fg_base` | — |

The hover/selected distinction is the 1px outline: hover is the soft prelude;
selected commits to a click. Active is reserved for "this is what you're focused
on right now" — file tree's currently-open file is the canonical example.

### Left-rail workspace ordering

Workspace rows within a project group obey a persisted sort mode, cycled from a
text chip in the "Projects" header (`r_xs` radius, `t_sub_label`, `fg_subtle` →
`fg_base` on hover — no new tokens):

- **Smart** (default): rows needing action (approval / waiting) and running
  agents float to the top, reusing the same attention tiers as the agents
  dashboard so both surfaces agree on priority. Stable within a tier.
- **Recent**: newest workspace first.
- **Manual**: stored insertion order, unchanged.

The primary (repo-root) row is pinned first in every mode — it is the project's
anchor; only the worktree tail reorders.

### Approved exceptions (documented locally; do not refactor without paired UX call)

| Surface | Height | Reason |
|---|---|---|
| `file_tree_view::FILE_TREE_ROW_H` | 22 (vs `h_row` 24) | Scan-heavy navigation; compact rows pack more files |
| `search_panel::result_row::FILE_ROW_H` | 22 (vs `h_row` 24) | Same scan-heavy rationale; consistent with file tree |
| `search_panel::result_row::MATCH_ROW_H` | 18 | Per-line match rows pack tighter so a 10-match file fits in one screenful |
| `source_control::branch_picker::ROW_HEIGHT` | 28 (vs `h_overlay_item` 30) | Branch lists are long; more items in a tight popover |
| `left_rail::row_menu::ROW_MENU_ITEM_H` | 28 (vs `h_overlay_item` 30) | Narrow rail context reads tighter at 28px |
| `project_picker::ROW_HEIGHT` | 40 + `ROW_PAD_X` 16 | Modal-scale picker, not floating overlay |
| `workspace_card::CARD_HEIGHT_MULT` | 2.2 × `h_row` | Two-line rich card (name + agent verb/diff); same local-exception pattern as `ROW_HEIGHT_MULT = 1.6` |

## Button Variants × Sizes

| Variant | When to use | Notes |
|---|---|---|
| `default` (primary) | The single affirmative action of a flow | Commit, Confirm |
| `secondary` | Lower-emphasis sibling to a primary | Cancel of a non-destructive flow |
| `outline` | Toolbar / standalone where filled feels heavy | Sparingly |
| `ghost` | Icon buttons, row-row triggers, anywhere chrome should disappear | The default in compact surfaces |
| `danger` | Destructive **modal confirm** button | Reserved for ConfirmDialog primary |
| `danger_ghost` | **Row-level destructive verb** | Stash Drop, Worktree Remove — `ui::danger_ghost()` |
| `link` | Inline text actions inside paragraphs | Help text, learn-more |

Sizes: `default` (32px), `small` (28px), `xsmall` (22px), `icon` variants.
**Match the size to the surrounding row height** — a `default` button in a 28px
toolbar reads as a layout bug.

### Rule: row-level destructive verbs use `danger_ghost`, not `danger`

Full `.danger()` produces a saturated red box that visually overwhelms the row when
it sits next to ghost siblings (Apply/Pop next to Drop, Open next to Remove). The
row reads as "one button is screaming" instead of "this verb is destructive".

`ui::danger_ghost(id, label, &theme, &density, typography, on_click)` renders a
transparent-background div sized like a `.ghost().xsmall()` button, with
`theme.status_error` foreground. The destructive intent reads through the color
without breaking the row's visual hierarchy. Defined in `crates/app/src/ui/buttons.rs`.

## Floating Surface Chrome

The canonical popover / picker / context-menu recipe:

```
bg     = theme.bg_overlay
border = 1px theme.border_active
corner = density.r_card
padding = density.pad_overlay      (6px)
item h  = density.h_overlay_item   (30px)
```

Adopt for every new floating surface. Today's compliant surfaces:
`pane_actions`, `adapter_picker`, `commit_context_menu`, `file_tree_context_menu`,
`tab_context_menu`, `git_panel/row_context_menu`, `review_note_popover`,
`pr_dialog`. See the "approved exceptions" table above for the three surfaces
that deliberately diverge.

## Diff review notes

Per-line review annotations in the diff viewer. Three surfaces, all quiet by
default:

- **Gutter marker** — a fixed-width cell reserved on every annotatable line so
  columns never shift. A line with a saved note shows a filled glyph in
  `status_info`; an un-noted line's glyph is transparent until the marker cell
  is hovered, then a faint `fg_muted` hint — discoverable without peppering
  every line with a dot. Click opens the compose popover. Markers are baked
  once per prepared-rows rebuild (never per frame), like the staged slivers.
- **Compose popover** (`review_note_popover`) — Floating Surface Chrome; a
  multi-line `InputState`. `⌘↵` saves, `Esc` cancels, Delete removes. An
  emptied body on Save deletes the note (clearing the text means "remove").
- **Notes (count) cluster** — appears in the diff toolbar only when the scope
  carries at least one note: a count label (`Notes (3)`) plus `Send` / `Copy` / `Clear`
  text chips. Send formats all notes as one markdown prompt (file-grouped,
  each note carrying its line's code as a fenced block) and dispatches it to
  the active agent via `SendTextToActiveAgent`; Copy puts the same markdown on
  the clipboard; Clear drops the scope's notes.

Notes anchor to a stable `(repo, diff_ref, path, side, line)` coordinate — not a
render-row index — so they re-attach to the right line across folds, scroll,
split mode, and app restart. Persistence is SQLite (`diff_review_notes`); the
diff view rehydrates on every load. Send/Copy include code context in both
inline and split mode (the prompt is built from the render plan, not the
on-screen rows). Split mode does not yet paint the gutter markers (inline
only) — a deliberate v1 boundary; notes added inline still send correctly
while viewing split.

## CI checks + pull requests

The Source Control panel surfaces the branch's open-PR checks and PR actions —
quiet by default, no chrome when there's no PR.

- **Checks section** (`source_control/checks_section`) — replaces the one-line CI
  summary with a per-check list (status glyph in `status_ok` / `status_error` /
  `status_warning` + name + blurb) under a headline ("CI failing (4)") with
  `Refresh` and, when something failed, a `Fix failing` chip. A failing check is
  clickable: it expands an inline log peek (the run's `--log-failed` tail,
  monospace, byte-capped, in a `bg_base` scroll box). `Fix failing` bundles the
  failing logs into one markdown prompt and sends it to the active agent via
  `SendTextToActiveAgent`. Check data rides the existing PR-status poll (no
  dedicated cadence); the section collapses to nothing when there are no real
  checks. This lives in the SCM panel, not a separate activity-bar tab — checks
  belong with source control and reuse its poll.
- **Create-PR dialog** (`pr_dialog`) — Floating Surface Chrome, centered modal.
  Editable title + multi-line body + a draft toggle; `Draft from commits` fills
  the fields from the branch's commit subjects. `⌘↵` creates, `Esc` cancels;
  Create is disabled until the title is non-empty. Replaces the bare
  commit-derived auto-create so the user reviews/edits before opening the PR.
- **Merge** — when a PR is open, the SCM dropdown grows one row per method
  (squash / merge commit / rebase), disabled while any op is in flight. Merging
  never deletes the branch (the user's explicit choice). Remote PR ops show an
  In-Flight status frame ("Creating/Merging pull request…").

All forge calls (checks, log peek, create, merge) go through the `forge`
provider seam (`ForgeProvider`, one `gh`-CLI impl today) — never the CLI
directly — so a second host can slot in without touching these surfaces.

## Tasks page (issue/PR browser)

The left rail's nav rows are body-replacing **pages**, not just shells. The
workspace list is the **home** view: `active_nav` is `Option<NavItem>` where
`None` = home (nothing highlighted), so the app cold-starts on the workspace
list. Clicking a nav opens its page; clicking the active nav again toggles back
home. Agents (dashboard) and Tasks are the implemented pages.

- **Tasks body** (`tasks_view`) — a GitHub issue/PR browser for the active
  project's repo, fetched through the `ForgeProvider` seam. A two-row header of
  text chips (mirroring the sort-mode chip: `r_xs`, active = `bg_panel_alt` +
  `fg_base`, inactive = `bg_panel` + `fg_muted`): `Issues`/`PRs`, then
  `Open`/`Closed`/`All` + `Mine` + `Refresh`. The body is a virtualized
  `uniform_list` (constrained sizing → rows fit-and-truncate the rail width, not
  horizontal-scroll). Loading / empty / no-project states render a centered
  `fg_subtle` hint; the empty state names the gh-auth requirement rather than
  implying a bug.
- **Task row** — two lines: `#<n> <title>` (title `overflow_hidden` +
  `whitespace_nowrap`) + a state chip (`status_ok` open / `status_info` merged /
  `fg_muted` closed); second line is a truncating cluster (label chips +
  `@assignee`) plus a fixed right-side actions cluster that always stays visible:
  `↗` (open in browser) and `+ Workspace`.
- **Create workspace from a task** — reuses `create_workspace_async`; the branch
  is `oximux/<slug>` derived from `"{issue|pr} {n} {title}"` so the number is
  legible. The workspace persists a `linked_issue` (`#<n>`, V011 column) shown as
  a `status_info`-tinted badge on its card after the branch chip, and the rail
  auto-activates it (selects + returns home). Manually-created workspaces have no
  linked issue and don't auto-activate.

Open-in-browser uses the shared `shell/open_url` helper (`open <url>`).

## Per-workspace tint

The **only** chrome customization in v1: an optional per-workspace identifier
hue, so parallel workspaces are distinguishable at a glance. It reads as an
*identifier*, not decoration — the dark-only "brand IS the absence of accent"
contract holds.

- **Vocabulary:** reuse the existing 9-swatch tab-color palette (`TabColor`) —
  no second color set, no new tokens. Persisted as a slug (`"blue"`) in the
  `workspaces.tint` column; `None` = default (pure charcoal).
- **Set via** the workspace row's "…" menu → a "Color" swatch row (clear + 9),
  dispatching `WorkspaceRoot::set_workspace_tint`. The current swatch gets a
  contrasting ring.
- **Appearance allowlist (accent only, ≤2px, never a fill or text background):**
  - ✅ the workspace's left-rail row — a 2px left-edge bar.
  - ✅ the active workspace's tab-strip — the active tab's top edge takes the
    tint (replacing the focus-ring blue); workspace-level, so every group's
    active tab in a split wears it.
  - 🚫 no full-panel washes, no tinted backgrounds, no tinted text.
- Contrast: the swatch is a theme-independent hex used only as a thin accent, so
  it never fights the charcoal surfaces or the single status-accent layer.

## Top-bar command center

The center chrome zone hosts a single VS Code–style command center — a
centered, clickable field that opens Quick Open. It deliberately does **not**
follow the reference editor's draggable tab strip in the title bar: chip
drag-reorder breaks inside AppKit's title-bar drag zone (`y < 28`), so the
title bar carries only plain click targets (toggles, this field), never drag
sources/targets.

- **Field recipe:** `bg_panel_alt`, 1px `border_inactive` → `border_active` on
  hover, `r_xs` corner, 22px tall, `max_w` 520px, centered. Content: a
  `search.svg` glyph (`fg_subtle`) + the active project name (`fg_muted`,
  `t_body_sm`) + a trailing `⌘P` hint (`fg_subtle`, `t_label_xs`). No active
  project → label "Search".
- **Behavior:** click dispatches `OpenQuickOpen`. Only the field takes
  mouse-down; the `flex_1` flanks stay window-drag regions, so the window is
  still draggable by the empty space on either side.
- Lives in `top_bar::command_center`, passed as the `center_header` center zone
  from `workspace_root`.

## Primitive-Picking Fork

When a control has multiple plausible primitives, use this fork:

| You want… | Reach for | Don't use |
|---|---|---|
| Hover-only label on an icon button | `Tooltip` | Custom hover div, title attr |
| Hover preview with richer content | Custom popover (no primitive yet) | `Tooltip` (no rich content) |
| Click → list of actions | Context-menu entity (see `commit_context_menu`) | Inline overlay |
| Right-click contextual actions | Context-menu entity, mounted at `WorkspaceRoot` | `DropdownButton` (different invocation) |
| Click → arbitrary content (form, picker) | Picker entity (see `branch_picker`) | `ConfirmDialog` |
| Decision-required blocking dialog | `ConfirmDialog` (`confirm_dialog/mod.rs`) | Inline overlay |
| Long-form modal (project list, settings) | Modal entity (see `project_picker`) | `ConfirmDialog` |
| Single choice from short list | Inline radio / split-button | Custom listbox |
| Single choice with filter | Picker entity with `InputState` filter | `Select` (no search) |

If you find yourself styling around a primitive (a context menu acting like a dialog,
or vice versa), stop — the dispatch semantics differ and a future contributor will
be misled by the mismatch.

## In-Flight Feedback

The right question isn't *"should this control change while it's working?"* — it's
*"how long does the action take, and what does the user need to know during that time?"*

| Duration | Feedback |
|---|---|
| 0–100ms | None. Anything visible reads as a glitch. |
| 100ms–1s | Button disabled only. |
| 1s–3s | Disabled + spinner OR label swap. |
| 3s+ or multi-step | Stage labels ("Pushing…", "Fetching…", "Cherry-picking…") |

Two corollaries:

- **Pre-reserve any space you'll later occupy.** If a button may swap to a longer
  label, fix its footprint up front — a button that resizes mid-action looks broken.
- **Don't pick worst-case feedback for everyone.** Local-first ops feel instant;
  remote-only ops need stage labels. Bind the *disabled* state immediately so
  double-clicks don't double-submit, and the *visible* spinner on a ~200ms timer.

Reference: `source_control::commit_ops::run_remote` and `run_commit_verb` both
encode this contract via a single-flight `in_flight: Arc<AtomicBool>` flag +
status-row label updates.

## Toasts (transient feedback)

Quiet, transient, bottom-right. For *fleeting cross-surface events that have no
permanent home* — agent finished/failed, commit/push failed, PR opened, clipboard
copies. The status bar carries persistent repo/agent *state*; a toast carries the
one-shot "this just happened" beat that would otherwise be silent or buried in a
panel the user has scrolled away from.

Appearance (honor the floating-surface contract):

- `bg_overlay` card, 1px `border_active`, **no shadow, no gradient**.
- A single 2px left accent bar in the status hue — `status_ok` (success),
  `status_error` (error), `status_info` (info). Text stays `fg_base`. One accent
  per card; the bar *is* the accent.
- Auto-dismiss after a few seconds; stack capped (oldest trims) so a burst can't
  grow without bound. Non-interactive — never steals clicks from beneath.

When NOT to toast:

- **Routine fast local ops** (0–100ms per In-Flight Feedback) — no toast; the
  result is its own feedback.
- **Persistent state** that belongs in the status bar or a panel — toasts are for
  events, not status.
- **Approval / waiting-for-input** agent edges — those already raise the in-pane
  attention ring + OS banner; a toast on top is noise. Only terminal edges
  (finished / failed) toast.

Implementation: `shell::toast::ToastLayer` (per-window, mounted topmost so it
shows over modals). Events route via the free `shell::toast::toast(cx, kind, text)`
to the active window's layer (`ToastBus` global, refreshed on window activation),
or `WorkspaceRoot::push_toast` when the call site already holds the root.

## Composition rules

- **One accent per surface.** A row uses `status_warn` *or* `status_info`, not both.
- **Borders before backgrounds.** Use `border_inactive` to separate panels rather
  than a different bg.
- **Selection ≠ focus.** `selection` is for text and the active-current row.
  `border_active` + `focus_ring` is for the focused pane.
- **Focus-gain flash (splits only).** When focus moves between pane groups in a
  split, the newly-focused leaf gets a brief `focus_ring` rim (≤2px, ~0.28s,
  ease-out to transparent). It's a transient locator, not a state — the steady
  focus marker stays the tab strip's bottom border. Single-group layouts don't
  flash (no ambiguity). Reuses `focus_ring`; no new token.
- **No drop shadows** on dark UI. They look muddy. Use a 1px border in `border_active`
  instead.
- **No gradients** outside of the focus ring.

## Anti-patterns

Common drift, paired with the right fix:

| Don't | Do | Why |
|---|---|---|
| `rgb(0x141518)` | `theme.bg_panel` | Theme swaps must propagate; raw hex outside `settings/` is drift |
| `.text_size(px(typography.t_body_sm * 0.85))` | `.text_size(px(typography.t_sub_label))` | The arithmetic is a de-facto token without a name |
| `.h(px(density.h_row * 1.4))` | `.h(px(density.h_action_row))` | Same: arithmetic is a token in disguise |
| `const CARD_PADDING: f32 = 6.0;` (per-file) | `density.pad_overlay` | Eight pickers were each carrying this — one source of truth |
| `.rounded(px(3.0))` | `.rounded(px(density.r_chip))` | The chip radius is intentional, not magic |
| `.danger()` in a compact row | `ui::danger_ghost(...)` | Full danger weight overwhelms row hierarchy |
| `Hsla { a: 0.22, ..theme.status_added }` | `theme.diff_added_bg()` | Alpha tints belong on `Theme`, not at call sites |
| Two grays + a brand accent | Two grays + status hue when warranted | The brand IS the absence of accent in v1 |
| Bumping a font weight above 600 | Stick to `w_regular` / `w_medium` / `w_semibold` | Heavy monospace weights distort glyph metrics |
| Naming a new sc-style constant `TEXT` | `BODY_TEXT`, `GRAPH_META_TEXT` | Self-documenting names; shadow consts that mirror tokens are drift |

## Source-control panel local consts

`source_control/style.rs` carries a small set of SCM-specific consts that
intentionally diverge from the global tokens — kept local because the SCM surface
runs a touch looser than the rest of the cockpit:

| Const | Value | Why local |
|---|---|---|
| `PAD_H` | 12 | SCM panel uses 12px horizontal padding vs `pad_panel = 8` |
| `BODY_TEXT` | 12 | SCM body text vs `t_body_sm = 11`; file names scan from arm's length |
| `GRAPH_META_TEXT` | 11 | Graph metadata (author/date) — equals `t_body_sm` but named for context |
| `CAPS_TEXT` | 11 | Uppercase section labels |
| `SUB_LABEL_TEXT` | 10 | Conflict-kind sub-labels under file rows |
| `TAB_H` / `TOOLBAR_H` | 32 | SCM tab strip + toolbar; intentionally looser than `h_tab = 28` |
| `COMMIT_ROW_H` | 34 | Commit graph row height with room for ref-chip sub-rows |
| `ICON_CLUSTER_GAP` | 2 | Tight gap inside dense icon-button clusters |
| `LINE_COUNT_GAP` | 4 | Gap between `+N` / `−N` diff fragments |
| `ICON` | 14 | Inline toolbar/filter/chevron icon size |
| `COMMIT_H` | 46 | Commit message textarea height (compact 2-line composer) |

Future density preset (`Density::compact()` for narrow sidebars, v1.1) tightens
`PAD_H` to 8; render sites already route through `sc_style::pad_h(density)` so
the branch lights up without sweeping call-site changes.
