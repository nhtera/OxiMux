//! oximux-editor
//!
//! Thin wrapper around `gpui-component`'s `Input` widget configured as a
//! code editor (tree-sitter highlight built in), plus a hand-rolled LSP
//! client (`mod lsp`) that drives rust-analyzer over stdio and feeds the
//! editor's `HoverProvider` + `DiagnosticSet` surfaces.
//!
//! Step 2 adds: dirty flag, Cmd+S save, LSP didChange/didSave/didClose.
//! `lsp_bridge` is extracted from `editor_view` to keep each file ≤300 LOC.

pub mod editor_view;
pub mod lsp;
pub mod lsp_bridge;

pub use editor_view::{EditorView, SaveFile, language_for_path};
pub use lsp::{LspClient, LspHoverProvider, path_to_file_uri};
