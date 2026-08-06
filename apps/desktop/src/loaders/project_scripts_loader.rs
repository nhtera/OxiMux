//! Loader for per-project lifecycle scripts from `.oximux/scripts.toml`.
//!
//! The reader itself moved to `oximux-settings`, beside the `ProjectScripts`
//! type it parses, because the worktree teardown path needs it and that path
//! is now shared with `oximux serve`. This module stays as the desktop's name
//! for it so call sites read the same as they always did — and the behaviour
//! tests move with the implementation.
//!
//! Scripts are per-project only (no global tier, unlike `commands.toml`) —
//! setup/run/cleanup are inherently repo-specific. The file is intended to be
//! committed to git so a team shares it; do not store secrets there.
//!
//! A missing file is a no-op (the all-`None` default → no buttons surface). A
//! malformed file is logged and skipped; it never crashes the application.

pub use oximux_settings::load_for_project;
