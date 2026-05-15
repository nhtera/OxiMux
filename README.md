# OxiMux

A Rust-native, multi-agent development cockpit for macOS. Open a repo → spawn isolated worktrees → run CLI coding agents (Claude Code, Codex, Aider) in parallel → review every change through a GitLens-grade Git UX.

- **Stack**: Rust 1.95 (edition 2024) + GPUI + [`longbridge/gpui-component`](https://github.com/longbridge/gpui-component) + SQLite + Tokio
- **Status**: Phase 0 — foundation scaffold. Not yet usable.
- **Platform**: macOS-only in v1 (13.0+).

## Quick start

```bash
# Build + run the shell (release ~slow first time due to GPUI compile)
cargo run -p oximux-app --release

# Or produce an .app bundle in dist/
./scripts/bundle-macos.sh
open dist/OxiMux.app
```

## Repo layout

```
crates/
├── app/        GPUI shell, action routing, window bootstrap
├── core/       domain types (Project, Workspace, Pane, AgentSession)
├── pty/        portable-pty + alacritty_terminal backend         (Phase 1)
├── git/        git CLI wrappers, status poller, diff parser      (Phase 2)
├── agents/     AgentRuntime trait + Claude/Codex/Aider adapters  (Phase 3)
├── editor/     gpui-component editor wrapper + LSP glue          (Phase 5)
├── storage/    SQLite + migration ladder + CI guard
└── settings/   theme tokens, density, typography, TOML config
xtask/          repo lint orchestrator (file-size cap etc.)
docs/
├── adr/                   architectural decision records (001-005)
├── design-guidelines.md   palette, density, typography (the contract)
├── gpui-pins.md           GPUI + gpui-component SHA tuple + bump log
├── brief.md               product vision and PRD
└── reference.md           competitor inventory
plans/
└── 260515-2012-oximux-v1-build/   nine-phase implementation plan
```

## Phase 0 deliverables (this checkpoint)

- Workspace + 8 crate skeletons, MSRV 1.85, edition 2024
- GPUI + gpui-component pinned tuple (`docs/gpui-pins.md`)
- Visual shell: TopBar (36px) / HSplit(Sidebar 240px, MainArea) / StatusBar (22px)
- `oximux-settings`: charcoal theme, cockpit density, typography scale
- macOS bundle script (`scripts/bundle-macos.sh`)
- CI guards: `xtask file-size-lint` (warn > 500 LOC / fail > 800), font-kit feature check, migration ladder count check, producer/consumer pre-commit hook
- ADRs 001 (stack), 002 (gpui-component), 003 (dogfood gate), 004 (no ACP in v1), 005 (fresh start)

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
