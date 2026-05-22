//! oximux-editor
//!
//! Thin wrapper around `gpui-component`'s `Input` widget configured as a
//! code editor (tree-sitter highlight built in), plus a hand-rolled LSP
//! client (`mod lsp`) that drives rust-analyzer over stdio and feeds the
//! editor's `HoverProvider` + `DiagnosticSet` surfaces.
//!
//! Phase 5 step 1 (spike): only the editor surface ships in this slice.
//! LSP modules land in step 1 days 2-3.

pub mod editor_view;
pub mod lsp;

pub use editor_view::{EditorView, language_for_path};
pub use lsp::{LspClient, LspHoverProvider, path_to_file_uri};
