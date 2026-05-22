//! `EditorView` — single-buffer code editor backed by `gpui-component`.
//!
//! The buffer is owned by an `Entity<InputState>` created with
//! `code_editor(language)`. Rendering forwards to `gpui_component::Input`,
//! which already supplies tree-sitter highlighting, soft-wrap, scroll, and
//! the popover slots the LSP providers will hook into in days 2-3.
//!
//! The view itself is intentionally tiny — it holds the file path (for
//! diagnostic URI lookup later), the editor state entity, and a focus
//! handle so the host window can route key events.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, ParentElement, Render,
    Styled, Window,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputState, TabSize},
};

use crate::lsp::{LspClient, LspHoverProvider, path_to_file_uri};

/// Map a file extension to the `gpui-component` language registry key.
///
/// Returns `"plain"` for anything we don't have a tree-sitter grammar for
/// — the editor still renders, just without syntax colors. Kept tiny on
/// purpose; richer detection (shebangs, modeline) belongs elsewhere.
pub fn language_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => "rust",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => "javascript",
        Some("go") => "go",
        Some("json") => "json",
        Some("md") | Some("markdown") => "markdown",
        Some("html") | Some("htm") => "html",
        Some("lua") => "lua",
        Some("php") => "php",
        Some("zig") => "zig",
        Some("kt") | Some("kts") => "kotlin",
        _ => "plain",
    }
}

/// Single-file editor entity. The host window mounts one of these per open
/// buffer; the spike opens exactly one.
pub struct EditorView {
    /// Source path. Kept so the eventual LSP wiring can derive the
    /// `file://` URI for `textDocument/didOpen` + `publishDiagnostics`
    /// routing without re-parsing the input each time.
    file_path: PathBuf,
    /// gpui-component editor state. Owned via `Entity` so the LSP
    /// provider impls (days 2-3) can re-enter via `WeakEntity::upgrade`
    /// without needing a `Mutex` between the GPUI render path and the
    /// background `publishDiagnostics` consumer.
    state: Entity<InputState>,
    focus_handle: FocusHandle,
}

impl EditorView {
    /// Open the file at `path` and create a code-editor `InputState`.
    ///
    /// `read_to_string` failures degrade silently to an empty buffer with
    /// a `tracing::warn` — the editor still renders so the user can see
    /// the file path was wrong instead of staring at a frozen UI.
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let content = std::fs::read_to_string(&path).unwrap_or_else(|err| {
            tracing::warn!(?err, file = %path.display(), "editor: failed to read file; starting empty");
            String::new()
        });
        let language = language_for_path(&path);
        let state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .multi_line(true)
                .tab_size(TabSize {
                    tab_size: 4,
                    ..Default::default()
                })
                .default_value(content)
        });
        Self {
            file_path: path,
            state,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Phase 5 step 1 spike: spawn rust-analyzer (or other server), wire
    /// its `hover` provider to the editor, and route `publishDiagnostics`
    /// into `InputState::diagnostics_mut`.
    ///
    /// `language_id` is the LSP language identifier (e.g. `"rust"`); we
    /// reuse the tree-sitter key but the LSP spec is technically distinct.
    /// `workspace_root` is the directory the server treats as the project
    /// root — for rust-analyzer this should be the workspace cargo root,
    /// not the source file's directory.
    ///
    /// Returns `Ok(())` on successful handshake. On any failure (binary
    /// missing, handshake timeout, etc.) the editor stays usable — only
    /// the LSP features are unavailable.
    pub fn attach_lsp(
        &mut self,
        program: &str,
        language_id: &str,
        workspace_root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let file_path = self.file_path.clone();
        let language_id = language_id.to_string();
        let program = program.to_string();
        // Hold a weak ref to the editor state so the install + pump steps
        // can surface entity drop via `WeakEntity::update -> Result`. The
        // bare `AsyncApp::update` returns `R` unconditionally and can't
        // detect a torn-down editor.
        let state_weak = self.state.downgrade();

        cx.spawn(async move |_view, cx| {
            // Spawn + handshake on the tokio runtime that main.rs:51's
            // rt.enter() guard kept alive across `app.run`.
            let client = match LspClient::spawn(&program, &workspace_root).await {
                Ok(c) => Arc::new(c),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        program,
                        "editor-spike: LSP server unavailable; editor will run without LSP"
                    );
                    return;
                }
            };
            tracing::info!(program, "editor-spike: LSP server ready");

            let uri = match path_to_file_uri(&file_path) {
                Ok(u) => u,
                Err(err) => {
                    tracing::warn!(?err, file = %file_path.display(), "editor-spike: cannot build file URI");
                    return;
                }
            };

            // didOpen carries the current buffer text; the spike is
            // read-only so version stays at 1 forever.
            let text = std::fs::read_to_string(&file_path).unwrap_or_default();
            if let Err(err) = client.did_open(uri.clone(), &language_id, 1, text) {
                tracing::warn!(?err, "editor-spike: didOpen failed");
                return;
            }

            // Install the hover provider on the editor's Lsp slot. Must
            // run on the main GPUI thread — hop via WeakEntity::update.
            //
            // `Rc<LspHoverProvider>` compiles here only because gpui's
            // `cx.spawn` uses a thread-local executor (no `Send` bound
            // on the future). The Rc is consumed inside the synchronous
            // `.update()` closure below — no await crosses it — so the
            // local-only constraint is satisfied. If a future refactor
            // moves this code onto `cx.background_executor().spawn`,
            // the Rc must become `Arc` first.
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

            // `publishDiagnostics` pump. The broadcast channel survives
            // slow consumers (capacity 64) — `Lagged(n)` is logged and
            // we keep reading. `RecvError::Closed` means the LspClient
            // dropped, which happens when the editor entity drops too,
            // so the loop exits cleanly.
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
                            // Editor entity gone — pump shutdown signal.
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            dropped = n,
                            "editor-spike: diagnostics consumer lagged"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
        .detach();
    }

    /// File path the editor is showing. Day-2 LSP wiring reads this to
    /// produce the `textDocument` URI.
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Editor state entity — exposed so the LSP layer can install its
    /// provider impls directly on the underlying `Lsp` field once it
    /// lands in days 2-3.
    pub fn state(&self) -> Entity<InputState> {
        self.state.clone()
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // Outer wrapper fixes the GPUI flex collapse problem (a bare `Input`
        // child without an explicit `flex_1`/`size_full` claim renders at
        // intrinsic height 0 against a fullscreen parent — see the same
        // pattern guard in `tabbed_pane::build_node`). Theme bg+fg avoid
        // black-on-black: the spike's first window had macOS chrome but
        // empty content because Input never grabbed the canvas.
        gpui::div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                Input::new(&self.state)
                    .font_family(theme.mono_font_family.clone())
                    .text_size(theme.mono_font_size)
                    .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn language_for_path_maps_rust() {
        assert_eq!(language_for_path(&PathBuf::from("foo.rs")), "rust");
    }

    #[test]
    fn language_for_path_maps_typescript_variants() {
        assert_eq!(language_for_path(&PathBuf::from("a.ts")), "typescript");
        assert_eq!(language_for_path(&PathBuf::from("a.tsx")), "typescript");
    }

    #[test]
    fn language_for_path_maps_javascript_variants() {
        for ext in ["js", "jsx", "mjs", "cjs"] {
            assert_eq!(
                language_for_path(&PathBuf::from(format!("x.{ext}"))),
                "javascript"
            );
        }
    }

    #[test]
    fn language_for_path_unknown_falls_back_to_plain() {
        assert_eq!(language_for_path(&PathBuf::from("foo.xyz")), "plain");
        assert_eq!(language_for_path(&PathBuf::from("Makefile")), "plain");
    }
}
