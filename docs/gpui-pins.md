# GPUI + gpui-component Pin Log

The `(gpui-rev, gpui-component-rev)` pair is a single decision. Bump together only when blocked. Track every bump here.

## How the pin actually works

`longbridge/gpui-component` declares `gpui = { git = "https://github.com/zed-industries/zed" }` with no `rev` in its own workspace. If we *also* declare `gpui` with an explicit `rev` in our root `Cargo.toml`, cargo treats the two as different sources and produces **two copies of `gpui::App`** in the dependency graph — the type-mismatch failure mode from the v0.9 post-mortem.

The correct cargo idiom for git deps shared with an unpinned transitive crate is:

1. Declare `gpui` / `gpui_platform` / `gpui_macros` in `[workspace.dependencies]` **without** `rev` — same shape as `gpui-component`'s declaration.
2. Pin `gpui-component` itself with `rev` (it's our direct dep, not transitive).
3. **`Cargo.lock` is the canonical pin** for the gpui crates. Commit it. To bump, run `cargo update -p gpui -p gpui_platform -p gpui_macros` and record the resolved commit in the table below.

## Required features

`gpui_platform` MUST be built with `font-kit` on macOS — without it, no glyphs render and the failure is silent at compile time (caught only at runtime). See `crates/app/tests/font_kit_feature_check.rs` for the CI guard.

`gpui-component`'s own workspace enables `font-kit, x11, wayland, runtime_shaders` unconditionally. We mirror that to stay binary-compatible.

## Current pin

| Crate | Rev | Date | Source |
|---|---|---|---|
| `gpui` / `gpui_platform` / `gpui_macros` | `4e06b33eb9f292507c98e4a143438c1c3d8a2b5b` | 2026-05-15 | zed-industries/zed main |
| `gpui-component` | `ccc1e7689203d09bfc88a0aae29473d1c289fa0c` | 2026-05-15 | longbridge/gpui-component main |
| Rust toolchain | `1.95.0` | 2026-05-15 | matches `zed-industries/zed/rust-toolchain.toml` upstream |

## Bump history

| Date | Old → New | Why | What broke |
|---|---|---|---|
| 2026-05-15 | — | Initial pin (Phase 0) | n/a |

## Bump procedure

1. Identify the blocker (specific API change, bug fix, or feature needed).
2. Find a `gpui-component` rev that targets the Zed change you need. Update its `rev` in `[workspace.dependencies]`.
3. Run `cargo update -p gpui -p gpui_platform -p gpui_macros` to pull a matching Zed commit.
4. Run `cargo check --workspace`. Fix breakage.
5. Run `font_kit_feature_check` test — it must pass.
6. Run the app and verify the shell still paints.
7. Read the resolved Zed commit from `Cargo.lock` and append a row to the table above.
