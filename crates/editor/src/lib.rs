//! oximux-editor
//!
//! Wraps the gpui-component built-in code editor + file tree + LSP glue.
//! v1 target: rust-analyzer working out of the box. The maturity spike for
//! this crate is in Phase 5.
//!
//! Phase 0 = empty placeholder so workspace `cargo check` is green.

/// Placeholder. Phase 5 introduces `EditorView`, `FileTreeView`, `LspClient`.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
