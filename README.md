# OxiMux

A Rust-native, multi-agent development cockpit for macOS. Open a repo → spawn isolated worktrees → run CLI coding agents (Claude Code, Codex, Aider) in parallel → review every change through a GitLens-grade Git UX.

- **Stack**: Rust 1.95 (edition 2024) + GPUI + [`longbridge/gpui-component`](https://github.com/longbridge/gpui-component) + SQLite + Tokio
- **Status**: Working cockpit, in active development. Dogfoodable for day-to-day repo work; pre-1.0, so expect rough edges.
- **Platform**: macOS-only in v1 (13.0+).

## Quick start

```bash
# Build the PTY relay daemon first — the app resolves it as a sibling of
# its own binary, and without it terminals fall back to in-process PTYs
# that don't survive a relaunch.
cargo build -p oximux-relay --release

# Build + run the shell (release ~slow first time due to GPUI compile)
cargo run -p oximux-app --release

# Or produce an .app bundle in dist/ (bundles oximux + oximux-relay)
./scripts/bundle-macos.sh
open dist/OxiMux.app
```

## Repo layout

```
crates/
├── app/          GPUI host shell: panes/tabs/splits, SCM, diff viewer,
│                 command palette, agents dashboard. Modules are foldered
│                 by concern (app_settings/ agent_glue/ session_restore/
│                 platform/ loaders/ shell/terminal/)
├── ui/           shared, app-agnostic widgets (FloatingSurface overlay,
│                 button variants, confirm dialog) — depends only downward
├── core/         domain types (Project, Workspace, Pane, AgentSession)
├── pty/          portable-pty + alacritty_terminal backend
├── git/          git CLI wrappers, status poller, diff parser, clone
├── agents/       AgentRuntime trait + Claude/Codex/Aider adapters
├── editor/       gpui-component editor wrapper + LSP glue
├── storage/      SQLite + migration ladder + CI guard
├── settings/     theme tokens, density, typography, TOML config
├── relay-proto/  wire protocol shared by relay daemon + client
├── relay/        out-of-process PTY relay daemon (survives relaunch)
└── relay-client/ in-app client for the relay daemon
xtask/            repo lint orchestrator (file-size cap etc.)
docs/
├── adr/                   architectural decision records (001-005)
├── design-guidelines.md   palette, density, typography (the contract)
├── gpui-pins.md           GPUI + gpui-component SHA tuple + bump log
└── brief.md               product vision and PRD
plans/
└── 260515-2012-oximux-v1-build/   nine-phase implementation plan
```

## Capabilities

- **Workspaces & worktrees** — open a repo, spin up isolated `oximux/<slug>` worktrees per task; create, switch, archive, stash.
- **Panes & terminals** — split panes, tabs, a floating terminal; PTYs run out-of-process via the relay daemon and survive an app relaunch.
- **CLI agents** — spawn Claude Code / Codex / Aider in tabs; an agents dashboard tracks per-session status (`Running`, `NeedsApproval`, `Done`, `Failed`).
- **Git / SCM** — status poller, staged/unstaged review, commit (with AI-drafted messages), commit graph, branch picker, push/pull/sync, CI badge, `gh pr create`.
- **Diff viewer** — a custom-canvas renderer with per-line geometry, word-diff, combined-diff, and folded hunks.
- **Navigation** — a command palette (Quick Open + commands, fuzzy match), file explorer, search panel.
- **Design system** — charcoal dark theme, cockpit density, typography scale (`oximux-settings`; see `docs/design-guidelines.md`).
- **Guardrails** — `xtask file-size-lint` (warn > 500 LOC / fail > 800), font-kit feature check, migration ladder count check, producer/consumer pre-commit hook.
- ADRs 001 (stack), 002 (gpui-component), 003 (dogfood gate), 004 (no ACP in v1), 005 (fresh start).

## Working agreements

- **No file > 200 LOC** unless there is a documented reason (CI cap is 500 warn / 800 fail; soft cap is 200).
- **Snake_case** for Rust files; kebab-case for shell scripts.
- **Edit existing files** in-place. No `*_v2.rs`, `*_new.rs`, or `*_enhanced.rs`.
- **Dogfood before tag**: see [ADR-003](docs/adr/adr-003-dogfood-gate.md).
- **No code reuse** from `OxideADE-old`. Ideas only. See [ADR-005](docs/adr/adr-005-fresh-start.md).

## Install pre-commit hook (optional)

```bash
ln -sf ../../scripts/pre-commit-producer-consumer.sh .git/hooks/pre-commit
```

Warns (does not block) when a staged diff deletes a public symbol so consumers can be audited before merge.

## License

MIT OR Apache-2.0 (dual). See ADR-005 for the rationale.
