# OxiMux — Development Roadmap

**Updated**: 2026-08-08
**Gate rule**: each phase ships only after ≥7 consecutive daily-driver days with zero panics (ADR-003). Tests-passing alone is not sufficient.

Detail per shipped item lives in `docs/project-changelog.md` (newest-first, with
commit SHAs). This file tracks *status and what is next*, not history.

---

## Original v1 phase table — where it landed

| Phase | Capability | Status |
|---|---|---|
| 00 | Foundation: workspace, GPUI shell, CI guards | shipped; CI guards grew file-size lint, docs gate, cargo-tree gate, bundle-version gate |
| 01 | Terminal cockpit: multi-pane PTY, tabs, search, render perf | shipped (multiplexer + emulator-richness plans complete 2026-05-29/30; IME, OSC 7/8/52/133/633, CJK, box-drawing) |
| 02 | Git core: status, diff, stage, commit, stash, worktree UI | shipped; later: commit graph, virtualized DiffView, pull/push/sync primary button (v0.1.6) |
| 03 | CLI agent integration | superseded — the adapter/StatusMachine design was replaced by the **Agent Chat cockpit** (stream-json decoder, ACP, OSC-9999 status sideband, hooks); see workstreams below |
| 04 | Workspace persistence: SQLite + session restore | shipped (sessions, tabs, windows, agent chats, titles all restore; storage migrations through V024+) |
| 05 | Editor + LSP | paused at steps 1-5 (spike, save round-trip, file tree, pane-as-editor-host, markdown preview 2026-06-16); editor remains gpui-component `Input` by decision — no Zed port |
| 06 | Git review polish: side-by-side diff, blame, conflict UI | not started as a phase; partial via DiffView zoom/virtualization |
| 07 | Multi-agent cockpit: dashboard, approval detection, presets | shipped (two-line status cards, live tool subline, NeedsApproval via hooks, ambient titles) |
| 08 | Ship v1: docs, packaging, beta | shipped as the v0.1.x public release train (below); **v1.0 tag itself still pending** |

---

## Workstreams that emerged after the original table (all shipped unless noted)

| Workstream | What shipped | When |
|---|---|---|
| Agent Chat | Full chat UI over agent CLIs: Claude stream-json, Codex, ACP (OpenCode/Pi), permission + AskUserQuestion mediation, slash commands, image attach, fork, queue, session-history import (Claude/Codex/Copilot/OpenCode/Pi), worktree-as-workspace, model catalog cache | 2026-07 rounds 1–9 |
| Voice dictation | Offline STT (`crates/dictation`, sherpa-rs + Silero VAD), Vietnamese-first language picker, custom words, dictation history; mobile TranscribeAudio (v11) | 2026-07-16/17; mobile phase 8 |
| Remote Control + mobile | Pairing (QR + NaCl box), `SessionRegistry` visibility boundary, read-only device tier, self-unpair, projects quick-add; **React Native client in-repo** (`apps/mobile`): chat + git screens, session-list sync, connection recovery, dictation bar, EAS preview deploys | 2026-07 → 2026-08 |
| Computer use | Semantic-first driver, separate gate process, consent UX, per-project opt-in, parallel agents proven; PreToolUse hook (not `can_use_tool`) is the enforcement point | phases 1–5 complete |
| Windows port | Full desktop port (ConPTY, shell integration, frameless chrome, relay via named pipes); `windows-latest` CI is the only thing executing Windows code | merged to `main` 2026-08-03 (PR #1) |
| `oximux` CLI + `serve` | Headless host, ~35 command groups, 6-code exit taxonomy, permission `ls/allow/deny/answer` (+`--input` edit-then-approve), `--stalled-after`, schedules + heartbeats, coordination state with v19 resume cursor, worktrees, teams, signed self-update | phases 3–8, protocol v16→v19; released 2026-08-07 |
| Distribution | Styled DMG + notarization, desktop auto-update (swap at quit), first-run onboarding wizard, external-CLI auto-provisioning, CLI: 5-target release + minisign-signed manifest + Homebrew tap + curl installer | v0.1.3 → v0.1.8 |

## Release train

| Tag | Date | Highlight |
|---|---|---|
| v0.1.1–v0.1.3 | 2026-07-29 | perf, terminal↔chat sync, DMG + release automation |
| v0.1.4–v0.1.5 | 2026-07-31 | desktop auto-update, onboarding, Update pill + release notes |
| v0.1.6 | 2026-08-03 | Pull/Push/Sync, graph auto-refresh; first tag after the Windows merge |
| v0.1.7 | 2026-08-07 | **first CLI release**: `oximux` + `serve` + signed self-update, 5 targets, brew tap |
| v0.1.8 | 2026-08-08 | live list titles, destructive-typo guard, installer polish; **first real `oximux update` swap, proven** |

---

## Next

**CLI/serve (small, concrete)**
1. ~~Protocol event recording an edited permission approval~~ **DONE 2026-08-08** —
   `ThreadEvent::PermissionEdited` (proto v20); the transcript carries both the ask and the
   allow, and peers below v20 get a Notice downgrade in the same seq.
2. ~~Dual-build wire-skew test~~ **DONE 2026-08-08** — `wire_skew_e2e` drives the released
   binary against the current tree in both directions; CI downloads the latest release.
3. `truncated: true` audit on bounded read verbs; distinct `serve` exit code for
   "data dir held by another host" (systemd `RestartPreventExitStatus` fodder).
4. `x86_64-unknown-linux-musl` release target (Alpine; only `gen-homebrew-formula.sh`
   hardcodes triples).

**Verification debt**
- Clean-VM installs (Linux, Windows); `install-cli.ps1` has run only under CI parse-check.
- Agent Chat remote-seam extraction (owed refactor).

**Product**
- Editor phase 05 step 6+ — paused; revisit only with a concrete need.
- v1.0 tag: decide what beyond the v0.1.x train it means (dogfood gate below).

---

## Dogfood ledger

Journal entries live in `docs/journals/`. Each entry records: used the binary? panic? what broke?
A day with no entry does not count toward the gate.

| Phase | Days logged | Gate cleared |
|---|---|---|
| 00–02 | — | no entries logged |

The ledger has not been maintained since the public release train began; the ADR-003 gate is
effectively superseded by shipping v0.1.x to real installs. Open question for v1.0: retire the
ledger formally or restart it against the released builds.
