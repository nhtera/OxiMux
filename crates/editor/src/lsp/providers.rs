//! Bridge `gpui-component`'s `HoverProvider` trait to `LspClient`. Reuses
//! the editor's `Rope` to compute the LSP `Position` from the byte offset
//! the editor hands us, then routes the async LSP request through tokio's
//! own worker pool — gpui's background executor uses libdispatch threads
//! that don't inherit the tokio runtime context from the main thread's
//! `rt.enter()` guard (C1 fix, code-review 260522-0240).

use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Task, Window};
use gpui_component::input::{HoverProvider, Rope, RopeExt as _};
use lsp_types::{Hover, Uri};
use tokio::runtime::Handle;

use super::client::LspClient;

/// HoverProvider for a single open buffer. The (`client`, `uri`) pair is
/// fixed at construction so the editor can install one per file without
/// caring about the underlying request id or framing.
pub struct LspHoverProvider {
    client: Arc<LspClient>,
    uri: Uri,
    /// Tokio handle captured at construction (lifted from `LspClient`),
    /// so the future awaiting `tokio::time::timeout` runs on a tokio
    /// worker thread with the reactor in scope. gpui's
    /// `cx.background_executor()` dispatches via libdispatch (GCD) on
    /// macOS; those threads have NO tokio thread-local context and
    /// `tokio::time::timeout::poll` would panic with "no reactor
    /// running" the moment a real hover fired.
    tokio_handle: Handle,
}

impl LspHoverProvider {
    pub fn new(client: Arc<LspClient>, uri: Uri) -> Self {
        let tokio_handle = client.tokio_handle();
        Self {
            client,
            uri,
            tokio_handle,
        }
    }
}

impl HoverProvider for LspHoverProvider {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let position = text.offset_to_position(offset);
        let uri = self.uri.clone();
        let client = self.client.clone();
        let handle = self.tokio_handle.clone();
        cx.background_executor().spawn(async move {
            // `handle.spawn` puts the future on tokio's own worker pool
            // (with the reactor); the outer `await` rides gpui's GCD
            // thread and only sees the resulting `Result<...>`.
            handle
                .spawn(async move { client.hover(uri, position).await })
                .await
                .map_err(|join_err| anyhow::anyhow!("hover task join: {join_err}"))?
        })
    }
}
