//! LSP attachment logic for `EditorView`.
//!
//! Extracted from `editor_view.rs` to keep that file under 300 LOC
//! (xtask file-size-lint gate). This module owns the async LSP spawn body:
//! handshake, `didOpen`, hover-provider install, and the
//! `publishDiagnostics` pump loop.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{Context, WeakEntity};

use crate::lsp::{LspClient, LspHoverProvider, path_to_file_uri};

/// Wire up an LSP server asynchronously from `EditorView::attach_lsp`.
///
/// Spawns a GPUI detached task that: connects to the language server,
/// runs the `initialize` + `didOpen` handshake, installs the hover
/// provider on `state_weak`, stores the ready `Arc<LspClient>` back onto
/// the `EditorView` entity via `view_weak`, and finally loops on the
/// `publishDiagnostics` broadcast channel.
///
/// All mutations on the GPUI entity happen through `WeakEntity::update`
/// which safely no-ops if the entity was dropped while the async work was
/// in flight.
pub fn spawn_attach_lsp(
    editor_view: &mut super::editor_view::EditorView,
    program: &str,
    language_id: &str,
    workspace_root: PathBuf,
    cx: &mut Context<super::editor_view::EditorView>,
) {
    let file_path = editor_view.file_path().to_path_buf();
    let language_id = language_id.to_string();
    let program = program.to_string();
    let state_weak = editor_view.state().downgrade();
    let view_weak: WeakEntity<super::editor_view::EditorView> = cx.weak_entity();

    cx.spawn(async move |_view, cx| {
        let client = match LspClient::spawn(&program, &workspace_root).await {
            Ok(c) => Arc::new(c),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    program,
                    "editor: LSP server unavailable; editor will run without LSP"
                );
                return;
            }
        };
        tracing::info!(program, "editor: LSP server ready");

        let uri = match path_to_file_uri(&file_path) {
            Ok(u) => u,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    file = %file_path.display(),
                    "editor: cannot build file URI"
                );
                return;
            }
        };

        // didOpen carries the current on-disk text at version 1. Subsequent
        // mutations are sent via didChange starting at version 2.
        // We keep `did_open_text` to hand to `set_lsp_client` so it can
        // detect whether the buffer drifted during the handshake gap and
        // fire a catch-up didChange (Fix #3, tester L1).
        let did_open_text = std::fs::read_to_string(&file_path).unwrap_or_default();
        if let Err(err) = client.did_open(uri.clone(), &language_id, 1, did_open_text.clone()) {
            tracing::warn!(?err, "editor: didOpen failed");
            return;
        }

        // Store the ready client on the EditorView so the observe callback
        // can start sending didChange on the next text mutation.
        // Pass `did_open_text` so `set_lsp_client` can detect handshake-gap
        // drift and fire a catch-up didChange immediately.
        //
        // Use `update` (not `update_in`) — the window handle is not needed
        // here, and update_in silently fails in cx.spawn closures
        // (memory note: update_in silently fails in spawn contexts; use update).
        if view_weak
            .update(cx, |editor, _cx| {
                editor.set_lsp_client(client.clone(), did_open_text);
            })
            .is_err()
        {
            // Editor entity dropped while we were connecting — nothing to do.
            return;
        }

        // Install the hover provider on the editor's Lsp slot. Must run on
        // the GPUI main thread — hop via WeakEntity::update.
        //
        // `Rc<LspHoverProvider>` is safe here because gpui's `cx.spawn`
        // uses a thread-local executor (no `Send` bound on the future).
        // The Rc is consumed inside the synchronous `.update()` closure
        // below — no await crosses it — so the local-only constraint holds.
        let hover_provider = Rc::new(LspHoverProvider::new(client.clone(), uri.clone()));
        if state_weak
            .update(cx, |state, cx| {
                state.lsp.hover_provider = Some(hover_provider);
                cx.notify();
            })
            .is_err()
        {
            return;
        }

        // `publishDiagnostics` pump. Broadcast capacity 64 — `Lagged(n)`
        // is logged and reading continues. `RecvError::Closed` means the
        // LspClient dropped (editor entity gone) so the loop exits cleanly.
        let mut diag_rx = client.subscribe_diagnostics();
        loop {
            match diag_rx.recv().await {
                Ok(params) => {
                    if params.uri != uri {
                        continue;
                    }
                    if state_weak
                        .update(cx, |state, cx| {
                            if let Some(set) = state.diagnostics_mut() {
                                set.clear();
                                set.extend(params.diagnostics);
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        // Editor entity gone — pump shutdown.
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "editor: diagnostics consumer lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
    .detach();
}
