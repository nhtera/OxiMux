//! oximux-editor
//!
//! Thin wrapper around `gpui-component`'s `Input` widget configured as a
//! code editor (tree-sitter highlight built in), plus a hand-rolled LSP
//! client (`mod lsp`) that drives rust-analyzer over stdio and feeds the
//! editor's `HoverProvider` + `DiagnosticSet` surfaces.
//!
//! Step 2 adds: dirty flag, Cmd+S save, LSP didChange/didSave/didClose.
//! `lsp_bridge` is extracted from `editor_view` to keep each file ≤300 LOC.
//!
//! Step 3 adds the `file_tree` backend: headless GPUI entity emitting
//! `FileTreeEvent` from an `ignore`-crate walker + `notify` watcher pair.

pub mod autosave;
pub mod binary;
pub mod editor_header;
pub mod editor_view;
pub mod file_tree;
pub mod lsp;
pub mod lsp_bridge;
pub mod markdown_preview;

pub use autosave::{pause_autosave, resume_autosave};
pub use binary::{image_mime_for_path, is_binary_buffer, is_previewable_image};
pub use editor_view::{EditorView, RevealInExplorer, SaveFile, language_for_path};
pub use markdown_preview::MarkdownViewMode;
pub use file_tree::{FileTree, FileTreeEvent, FileTreeNode, TreeNodeId};
pub use lsp::{LspClient, LspHoverProvider, path_to_file_uri};
