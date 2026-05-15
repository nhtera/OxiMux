# OxiMux — Design Guidelines (v1)

This document is the source of truth for visual identity, palette, density, and typography. It drives `crates/settings/src/theme.rs`, `density.rs`, and `typography.rs`. Update both when changing any token.

## Brand

**Name**: OxiMux
**One-line**: Rust-native, multi-agent development cockpit for macOS.
**Tone**: Quiet, technical, terminal-first. Not playful. Not "AI".

`Oxi` = oxidation (Rust). `Mux` = multiplexer (the cockpit metaphor: many agents, many panes, one operator).

## Mode

Dark only in v1. Light mode deferred until after Phase 8. Every token below assumes dark canvas.

## Palette — Monochrome Charcoal

The base is near-black with graphite panels. Status hues are the only saturated color in the UI. This keeps the eye on the diff, terminal, and agent output — not chrome.

| Token | Hex | Use |
|---|---|---|
| `BG_BASE` | `#0E0F11` | Window background, dock empty area |
| `BG_PANEL` | `#15171A` | Sidebar, status bar, panel backgrounds |
| `BG_PANEL_ALT` | `#1B1E22` | Hover, selected row, nested panel |
| `BG_OVERLAY` | `#22262B` | Tooltip, popover, command palette |
| `FG_BASE` | `#E6E8EB` | Body text |
| `FG_MUTED` | `#9AA0A6` | Labels, secondary text, inactive tabs |
| `FG_SUBTLE` | `#6B7177` | Disabled, placeholder, gutter numbers |
| `BORDER_INACTIVE` | `#26292E` | Pane dividers, card edges |
| `BORDER_ACTIVE` | `#3A4047` | Focused pane border |
| `SELECTION` | `#2D3A4D` | Text selection background |
| `FOCUS_RING` | `#4A6E9C` | Keyboard focus outline |

### Status palette (single accent layer)

| Token | Hex | Use |
|---|---|---|
| `STATUS_OK` | `#6FA86A` | Clean working tree, agent done, test pass |
| `STATUS_WARN` | `#D9A441` | Dirty tree, approval pending, lint warn |
| `STATUS_ERROR` | `#D26464` | Conflict, panic, test fail |
| `STATUS_INFO` | `#5B97C9` | Running, fetching, building |
| `STATUS_MUTED` | `#6B7177` | Idle, paused, untracked |

No brand accent in v1. The brand *is* the absence of accent. Adding one is a Phase 8 decision, not a Phase 0 one.

## Density

Tight is the default. The cockpit fits more on screen than VSCode by design.

| Token | Value (px) | Use |
|---|---|---|
| `H_TOP_BAR` | 36 | Application top bar (title, workspace switcher) |
| `H_STATUS_BAR` | 22 | Bottom status bar (3 zones: left/center/right) |
| `H_TAB` | 28 | Pane tab strip |
| `H_ROW` | 24 | Sidebar list row, file tree row |
| `R_CARD` | 8 | Panels, cards |
| `R_XS` | 4 | Buttons, badges, inputs |
| `PAD_PANEL` | 8 | Panel inner padding |
| `PAD_ROW` | 6 | Row left/right padding |
| `GAP_INLINE` | 6 | Spacing between inline siblings |

## Typography

Single font family. Numbers tabular everywhere for diff alignment and counters.

| Token | Size (px) | Weight | Use |
|---|---|---|---|
| `T_LABEL_XS` | 10 | 500 | Tiny chips (LSP, branch) |
| `T_LABEL_CAPS` | 10.5 | 600, tracking +0.5 | All-caps section labels |
| `T_BODY_SM` | 11 | 400 | Status bar, gutter, tooltip |
| `T_BRAND` | 12 | 600 | Brand wordmark in top bar |
| `T_BODY_MD` | 13 | 400 | Sidebar rows, body text |
| `T_BODY_LG` | 14 | 400 | Main content, terminal default |

**Font stack**: `"Geist Mono"`, `"SF Mono"`, `Menlo`, `monospace` (terminal + editor). `"Inter"`, `"SF Pro Text"`, `system-ui` (UI chrome). System fallbacks ensure font-kit always finds a face.

## Composition rules

- **One accent per surface.** A row can use `STATUS_WARN` *or* `STATUS_INFO`, not both.
- **Borders before backgrounds.** Use `BORDER_INACTIVE` to separate panels rather than a different bg.
- **Selection ≠ focus.** `SELECTION` is for text. `BORDER_ACTIVE` + `FOCUS_RING` is for the focused pane.
- **No drop shadows** on dark UI. They look muddy. Use a 1px border in `BORDER_ACTIVE` instead.
- **No gradients** outside of the focus ring.

## Anti-patterns

| Don't | Why |
|---|---|
| Add a brand accent color | The plan locks identity-via-absence. Add it in Phase 8 if needed. |
| Use status hues as decoration | They are signals; saturating chrome trains the user to ignore them. |
| Stack > 2 grays in one panel | Reads as visual noise. Two grays + one border is enough. |
| Bump font weights above 600 | Heavy monospace weights distort glyph metrics. |

