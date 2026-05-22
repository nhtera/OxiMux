//! Hand-rolled LSP client. See `phase-05-step-01-editor-lsp-spike-plan.md`
//! for the design rationale (async-lsp rejected on lsp-types 0.95/0.97
//! version conflict; hand-roll ~600 LOC instead of bridging two type
//! universes).

pub mod client;
pub mod providers;
pub mod transport;

pub use client::{LspClient, REQUEST_TIMEOUT, path_to_file_uri};
pub use providers::LspHoverProvider;
