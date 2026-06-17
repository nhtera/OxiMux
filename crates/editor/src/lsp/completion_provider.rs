//! Bridge `gpui-component`'s `CompletionProvider` trait to `LspClient`.
//!
//! Mirrors `LspHoverProvider`: the (`client`, `uri`) pair is fixed at
//! construction, and the async `textDocument/completion` request is hopped
//! onto tokio's worker pool via the captured `Handle` so `tokio::time::timeout`
//! polls with a live reactor (gpui's background executor runs on libdispatch
//! threads with no tokio context).

use std::sync::Arc;

use anyhow::Result;
use gpui::{Context, Task, Window};
use gpui_component::input::{CompletionProvider, InputState, Rope, RopeExt as _};
use lsp_types::{CompletionContext, CompletionResponse, Uri};
use tokio::runtime::Handle;

use super::client::LspClient;

/// CompletionProvider for a single open buffer.
pub struct LspCompletionProvider {
    client: Arc<LspClient>,
    uri: Uri,
    tokio_handle: Handle,
}

impl LspCompletionProvider {
    pub fn new(client: Arc<LspClient>, uri: Uri) -> Self {
        let tokio_handle = client.tokio_handle();
        Self {
            client,
            uri,
            tokio_handle,
        }
    }
}

impl CompletionProvider for LspCompletionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>> {
        let position = text.offset_to_position(offset);
        let uri = self.uri.clone();
        let client = self.client.clone();
        let handle = self.tokio_handle.clone();
        cx.background_executor().spawn(async move {
            let resp = handle
                .spawn(async move { client.completion(uri, position, Some(trigger)).await })
                .await
                .map_err(|join_err| anyhow::anyhow!("completion task join: {join_err}"))??;
            // The widget wants a concrete response; a `None` server reply (no
            // suggestions) maps to an empty list.
            Ok(resp.unwrap_or(CompletionResponse::Array(Vec::new())))
        })
    }

    /// Trigger as-you-type on identifier characters plus the member/scope
    /// access punctuation (`.`, `:`) common across the supported languages.
    fn is_completion_trigger(
        &self,
        _offset: usize,
        new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        new_text
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ':'))
    }
}
