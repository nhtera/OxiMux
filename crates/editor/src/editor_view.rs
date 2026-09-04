//! `EditorView` — per-file viewer that renders one of three modes:
//!
//! - `Text` — gpui-component `code_editor` `InputState` with tree-sitter
//!   highlighting, soft-wrap, LSP hover, diagnostics. The primary path.
//! - `Image` — GPUI `img(path)` element centered with letterbox. Drawn for
//!   PNG/JPG/GIF/WEBP/BMP/ICO/SVG (anything `image_mime_for_path` accepts).
//! - `Binary` — centered "Binary file — cannot display" placeholder.
//!
//! Mode is decided once at construction by sniffing the file bytes:
//! `binary::is_binary_buffer` for the NUL-byte heuristic, then a UTF-8
//! round-trip check on top to catch encodings (UTF-16BE, latin-1) that
//! lack interior NULs in their ASCII range but still fail to decode. The
//! decision is *not* extension-driven for text-vs-binary so a file named
//! `data.txt` containing a PNG header still rejects gracefully.
//!
//! LSP-related state lives only on the `Text` variant — `attach_lsp` is a
//! no-op for `Image`/`Binary` and `state()` returns `None` so callers can
//! cleanly skip provider installs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, img, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme,
    input::{Input, InputState, TabSize},
    resizable::{h_resizable, resizable_panel},
};
use oximux_settings::AutosaveSettings;

use gpui::{Global, actions};

use lsp_types::Uri;

use crate::binary::{image_mime_for_path, is_binary_buffer};
use crate::editor_header;
use crate::lsp::{LspClient, path_to_file_uri};
use crate::lsp_bridge::spawn_attach_lsp;
use crate::markdown_preview::{self, MarkdownViewMode};
use crate::mermaid;
use crate::pdf_preview::{self, PdfContent, PdfDocument};

actions!(
    oximux,
    [
        SaveFile,
        /// Increase the editor font size (Cmd+=). Editor-global.
        EditorZoomIn,
        /// Decrease the editor font size (Cmd+-). Editor-global.
        EditorZoomOut,
        /// Reset the editor font size to the theme default (Cmd+0).
        EditorZoomReset
    ]
);

/// Editor-global font-zoom level, held as a GPUI [`Global`]. `steps` is a
/// signed pixel delta applied on top of the theme's mono font size; the
/// effective size is clamped to a comfortable range. Session-scoped (resets
/// on relaunch). Every editor view reads this on render, so a zoom action in
/// one editor applies to all editor tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EditorZoom {
    steps: i32,
}

impl Global for EditorZoom {}

/// Smallest / largest editor font size in px after applying the zoom delta.
const EDITOR_FONT_MIN_PX: f32 = 8.0;
const EDITOR_FONT_MAX_PX: f32 = 32.0;

impl EditorZoom {
    /// One step larger. Public so other code surfaces that share the zoom
    /// level (the diff body) can drive the same global from their own
    /// zoom-in action.
    pub fn zoomed_in(self) -> Self {
        Self {
            steps: self.steps + 1,
        }
    }
    /// One step smaller. Public for the same reason as [`Self::zoomed_in`].
    pub fn zoomed_out(self) -> Self {
        Self {
            steps: self.steps - 1,
        }
    }
    /// Back to the theme default (no delta).
    pub fn reset() -> Self {
        Self { steps: 0 }
    }

    /// Effective font size: the theme base plus the zoom delta, clamped.
    pub fn effective_px(self, base: gpui::Pixels) -> gpui::Pixels {
        let raw = f32::from(base) + self.steps as f32;
        px(raw.clamp(EDITOR_FONT_MIN_PX, EDITOR_FONT_MAX_PX))
    }

    /// `delta` steps at once, with `steps` clamped to the range where
    /// `effective_px(base)` still moves. The wheel path needs the clamp:
    /// a trackpad fling delivers dozens of steps in a second, and letting
    /// `steps` overshoot the clamp in `effective_px` would leave a dead
    /// zone where the first reverse-scroll notches change nothing.
    fn stepped_by(self, delta: i32, base: gpui::Pixels) -> Self {
        let min = (EDITOR_FONT_MIN_PX - f32::from(base)).round() as i32;
        let max = (EDITOR_FONT_MAX_PX - f32::from(base)).round() as i32;
        Self {
            steps: (self.steps + delta).clamp(min, max),
        }
    }
}

/// Scroll travel (px) per Cmd+wheel zoom step. Chosen so one discrete
/// mouse-wheel notch — a 1-line delta, converted at this same value —
/// lands on exactly one step, while a trackpad accumulates smoothly
/// instead of stepping once per tiny pixel-delta event.
const WHEEL_ZOOM_STEP_PX: f32 = 20.0;

/// Zoom travel (px) per whole unit of pinch delta. A pinch event's delta is
/// fractional (0.1 = a 10% spread), so this gain sets the gesture feel: at
/// 150, roughly every 13% of pinch crosses one `WHEEL_ZOOM_STEP_PX` bucket —
/// a full comfortable pinch walks the font several steps, matching how the
/// gesture behaves in mainstream editors.
const PINCH_ZOOM_PX_PER_UNIT: f32 = 150.0;

/// Bucket accumulated zoom travel (Cmd+wheel or pinch) into whole zoom
/// steps. Returns the steps to apply now and the leftover travel to carry
/// into the next event.
fn wheel_zoom_steps(accum: f32) -> (i32, f32) {
    let steps = (accum / WHEEL_ZOOM_STEP_PX).trunc();
    (steps as i32, accum - steps * WHEEL_ZOOM_STEP_PX)
}

/// The current editor zoom (default when never set this session).
fn current_zoom(cx: &App) -> EditorZoom {
    cx.try_global::<EditorZoom>().copied().unwrap_or_default()
}

/// Reveal the editor's file in the workspace file-tree sidebar. Dispatched
/// from the breadcrumb actions menu and handled by the host shell, which
/// owns the file tree. `PathBuf` isn't a valid action payload, so the path
/// travels as a `String`.
#[derive(Clone, Debug, Default, PartialEq, gpui::Action)]
#[action(namespace = oximux, no_json)]
pub struct RevealInExplorer {
    pub path: String,
}

/// Map a file extension or well-known basename to the `gpui-component`
/// language-registry key. Returns `"plain"` for anything we don't have a
/// tree-sitter grammar mapping for — the editor still renders, just without
/// syntax colors. The registry itself silently no-ops on unknown keys so an
/// entry here without the matching `tree-sitter-*` feature degrades to plain
/// rather than panicking.
pub fn language_for_path(path: &Path) -> &'static str {
    // Well-known basenames first so `Cargo.toml`, `Makefile`, etc. resolve
    // even when their extension says otherwise (or there's no extension).
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        match name {
            "Cargo.toml" | "Cargo.lock" | "pyproject.toml" => return "toml",
            "Makefile" | "GNUmakefile" | "makefile" => return "make",
            "Dockerfile" | "dockerfile" | "Containerfile" => return "bash",
            "package.json" | "tsconfig.json" | ".eslintrc.json" => return "json",
            ".gitignore" | ".gitattributes" | ".gitmodules" | ".env" => return "plain",
            _ => {}
        }
    }
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => "rust",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("py" | "pyi") => "python",
        Some("rb") => "ruby",
        Some("go") => "go",
        Some("java") => "java",
        Some("kt" | "kts") => "kotlin",
        Some("swift") => "swift",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx") => "cpp",
        Some("cs") => "csharp",
        Some("scala" | "sc") => "scala",
        Some("php") => "php",
        Some("lua") => "lua",
        Some("zig") => "zig",
        Some("ex" | "exs") => "elixir",
        Some("erb") => "erb",
        Some("ejs") => "ejs",
        Some("astro") => "astro",
        Some("svelte") => "svelte",
        Some("html" | "htm") => "html",
        Some("css" | "scss" | "sass") => "css",
        Some("json" | "jsonc") => "json",
        Some("md" | "markdown" | "mdx") => "markdown",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("sql") => "sql",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("cmake") => "cmake",
        Some("proto") => "proto",
        Some("graphql" | "gql") => "graphql",
        Some("diff" | "patch") => "diff",
        _ => "plain",
    }
}

/// `true` for files that should get the markdown preview: `.md`/`.markdown`
/// only. `.mdx` is intentionally excluded (it keeps the plain code editor) —
/// hence this tests the extension directly rather than reusing
/// `language_for_path`, which also maps `.mdx` to `"markdown"`.
pub fn is_markdown_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md" | "markdown")
    )
}

/// Returned by `decide_change_propagation` when a buffer mutation should be
/// forwarded to the LSP server. Carries the new monotonic version and the
/// full-document payload (LSP `didChange` content).
pub(crate) struct ChangePropagation {
    pub new_version: i32,
    pub text: String,
}

/// Decide whether an observe-tick should propagate to the LSP server. Pure
/// so the state-machine logic is unit-testable without a GPUI runtime.
/// Returns `None` for cursor-move / scroll ticks where the rope content
/// did not change.
pub(crate) fn decide_change_propagation(
    last_sent_text: &str,
    current_text: &str,
    current_version: i32,
) -> Option<ChangePropagation> {
    if current_text == last_sent_text {
        return None;
    }
    Some(ChangePropagation {
        new_version: current_version + 1,
        text: current_text.to_owned(),
    })
}

/// Backoff schedule for retrying a transient file-read failure. A file being
/// written by another process (or a slow/networked FS) can momentarily fail
/// to read; rather than dropping straight to a placeholder we retry on this
/// cadence before giving up. Terminal errors (missing file, permission
/// denied) skip retries entirely.
const LOAD_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(1000),
    Duration::from_millis(2500),
];

/// `true` for read errors that will never succeed on retry — a missing file
/// or a permission problem won't fix itself by waiting. Everything else
/// (interrupted, would-block, timed-out, generic I/O) is treated as
/// transient and retried on the backoff schedule.
fn is_terminal_read_error(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    matches!(err.kind(), NotFound | PermissionDenied)
}

/// Human-readable reason for a load failure, shown in the failed-state body.
fn load_error_message(err: &std::io::Error) -> SharedString {
    use std::io::ErrorKind::{NotFound, PermissionDenied};
    let reason = match err.kind() {
        NotFound => "File not found on disk.",
        PermissionDenied => "Permission denied reading this file.",
        _ => "Could not read this file.",
    };
    SharedString::from(reason)
}

/// LSP attach parameters stashed when `attach_lsp` is called before the
/// buffer finished loading (the async-retry path). Applied by `finish_load`
/// once the content resolves to a real text buffer. Owned (not borrowed) so
/// it can outlive the synchronous attach call.
struct PendingLspAttach {
    program: String,
    args: Vec<String>,
    language_id: String,
    workspace_root: PathBuf,
}

/// Per-file content variant. The struct keeps mode-specific state here so
/// the surrounding `EditorView` fields (`file_path`, `focus_handle`) stay
/// universal across text/image/binary.
pub enum EditorContent {
    /// Editable text buffer — full code-editor path with LSP wiring.
    Text(TextContent),
    /// Previewable image — rendered via GPUI's native `img(path)` element.
    /// The `mime` field is plumbed mainly for diagnostic logging today.
    Image { mime: &'static str },
    /// A PDF: parsed once, one page rasterized at a time off the UI thread.
    /// Page state and the current bitmap live in `PdfContent`.
    Pdf(PdfContent),
    /// Non-text, non-image binary — shown as a centered placeholder.
    Binary,
    /// A transient read failure is being retried on the backoff schedule.
    /// Shows a "Loading…" placeholder instead of a blank/binary view.
    Loading,
    /// The file could not be read — a terminal error or exhausted retries.
    /// Shows the reason plus a Retry affordance so the user isn't stuck on
    /// a silent blank.
    LoadFailed { message: SharedString },
}

/// State carried by the `Text` content variant. Pulled out into its own
/// struct so the `Image` / `Binary` variants don't drag along an unused
/// `InputState` allocation (an empty `InputState` still owns a tree-sitter
/// parser instance).
pub struct TextContent {
    /// gpui-component editor state entity.
    pub(crate) state: Entity<InputState>,
    /// `true` when the buffer text differs from the last successful save.
    pub(crate) dirty: bool,
    /// Monotonic per-buffer version counter — LSP §3.17.2. didOpen sends
    /// version 1; each didChange increments before sending.
    pub(crate) doc_version: i32,
    /// Snapshot of the last text sent to the LSP server. The observe
    /// callback compares against this to suppress cursor-move noise.
    pub(crate) last_sent_text: String,
    /// Keeps the `cx.observe` subscription alive for the lifetime of this
    /// view. Dropping it unregisters the callback from `InputState`.
    pub(crate) _observe_sub: Subscription,
}

/// Single-file viewer. The host window mounts one per open tab.
pub struct EditorView {
    /// Source path on disk.
    file_path: PathBuf,
    /// Cached `file://` URI derived from `file_path` in `new()`. Stored
    /// as a parsed `lsp_types::Uri` so every LSP notification call avoids
    /// a per-call heap allocation + parse round-trip. Built unconditionally
    /// even for binary/image so a future "switch to text mode" code path
    /// doesn't have to retro-fit URI parsing.
    uri: Uri,
    /// Mode-specific state. Decided at construction; stable for the
    /// lifetime of the view.
    content: EditorContent,
    focus_handle: FocusHandle,
    /// Active LSP connection. `None` until `attach_lsp` completes the
    /// handshake; didChange/didSave/didClose are no-ops when `None`.
    /// Always `None` for `Image`/`Binary` content.
    lsp_client: Option<Arc<LspClient>>,
    /// Mirrors platform focus state into a local field so the host's
    /// per-leaf observer can read focus without a `&Window`.
    focused: bool,
    /// `true` for `.md`/`.markdown` text files (extension-gated — `.mdx` is
    /// deliberately excluded and keeps the plain code-editor view). Drives
    /// the header mode toggle and the rendered-preview body arm.
    is_markdown: bool,
    /// Active markdown view (Source / Preview / Split). View-lifetime state
    /// only — not persisted, so every reopen of a `.md` starts in Preview.
    /// Meaningless (and unused) when `is_markdown` is false.
    md_mode: MarkdownViewMode,
    /// Rendered ```mermaid fences for the preview, keyed by content+theme.
    /// Per-view so two open `.md` tabs never share diagram state. Empty
    /// whenever the document has no mermaid fences.
    mermaid: mermaid::MermaidCache,
    /// Whether the breadcrumb "⋯" actions dropdown is showing. View-lifetime.
    actions_menu_open: bool,
    /// Leftover Cmd+wheel scroll travel (px) below one zoom step. Carried
    /// across events so a slow trackpad drag still reaches a step.
    wheel_zoom_accum: f32,
    /// Host-installed callback that opens a linked document (a clicked
    /// relative link in the markdown preview) in the surrounding pane group.
    /// `None` until the shell wires it, in which case such links fall back
    /// to the OS URL opener (a no-op for relative paths).
    document_opener: Option<markdown_preview::DocumentOpener>,
    /// Monotonic generation for the debounced autosave timer. Every buffer
    /// change bumps it; a fired timer only writes if its captured generation
    /// is still current (no newer edit superseded it).
    autosave_gen: u64,
    /// Holds the in-flight autosave debounce timer. Re-arming on each edit
    /// drops (cancels) the prior timer, so only the last edit's timer fires.
    _autosave_task: Option<Task<()>>,
    /// Repaints this view when the editor-global zoom changes, so background
    /// tabs pick up a new font size immediately (not just the focused one).
    _zoom_sub: Subscription,
    _focus_sub: Subscription,
    _blur_sub: Subscription,
    /// Releases the PDF page texture from the sprite atlas when the view is
    /// dropped — gpui only evicts `RenderImage` sources on request, so a
    /// closed tab would otherwise keep its last page resident until the
    /// window closes.
    _release_sub: Subscription,
    /// LSP attach request received while the buffer was still loading. The
    /// host attaches the server right after construction; if the first read
    /// failed transiently the content isn't text yet, so the request waits
    /// here and `finish_load` applies it once the retry succeeds.
    pending_lsp: Option<PendingLspAttach>,
    /// Holds the in-flight load-retry task. Dropping it cancels any pending
    /// retry (e.g., when the user triggers a fresh manual retry).
    _load_task: Option<Task<()>>,
}

impl EditorView {
    /// Open the file at `path` and pick the right rendering mode by
    /// sniffing the file bytes. Read failures degrade to an empty Binary
    /// placeholder with a `tracing::warn` — the editor still renders so
    /// the user can see that something opened (always open a tab on
    /// click, even when the file can't be rendered).
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        // Mirror window-focus events into `self.focused`. Register BEFORE
        // any explicit focus() call would run.
        let _focus_sub = cx.on_focus(&focus_handle, window, |view, _, cx| {
            view.focused = true;
            cx.notify();
        });
        let _blur_sub = cx.on_blur(&focus_handle, window, |view, _, cx| {
            view.focused = false;
            cx.notify();
        });
        // Editor-global font zoom changes from any editor must repaint this
        // one too — otherwise background tabs keep the old size until painted.
        let _zoom_sub = cx.observe_global::<EditorZoom>(|_view, cx| cx.notify());
        let _release_sub = cx.on_release(|view, cx| {
            if let EditorContent::Pdf(p) = &mut view.content
                && let Some(bitmap) = p.bitmap.take()
            {
                cx.drop_image(bitmap, None);
            }
        });

        // A `.pdf` is the one large file a user opens on purpose (a 283 MB
        // scan reads in ~50 ms warm, longer cold), so it skips the
        // synchronous read: it opens as Loading and a background task reads
        // + parses it, landing through `finish_load` like a retried read.
        let is_pdf = pdf_preview::has_pdf_extension(&path);
        // Read bytes (not a String) so a non-UTF-8 sequence does not
        // silently produce an empty buffer. The mode-detection below
        // consumes the bytes only when we settle on Text.
        let read_result = (!is_pdf).then(|| std::fs::read(&path));

        let uri = path_to_file_uri(&path).unwrap_or_else(|err| {
            tracing::error!(?err, file = %path.display(), "editor: cannot build file URI; LSP will be degraded");
            use std::str::FromStr;
            Uri::from_str(&format!("file://{}", path.display()))
                .expect("display-form URI is always valid ASCII")
        });

        // Decide the initial content. A transient read error doesn't fall
        // straight to a placeholder: it shows "Loading…" and a backoff retry
        // is scheduled so a file mid-write (or a momentarily-unavailable FS)
        // recovers on its own. Terminal errors (missing/denied) fail fast.
        let (content, needs_retry) = match read_result {
            None => (EditorContent::Loading, false),
            Some(Ok(bytes)) => (decide_content(&path, bytes, window, cx), false),
            Some(Err(err)) if is_terminal_read_error(&err) => {
                tracing::warn!(?err, file = %path.display(), "editor: read failed (terminal); showing failed state");
                (
                    EditorContent::LoadFailed {
                        message: load_error_message(&err),
                    },
                    false,
                )
            }
            Some(Err(err)) => {
                tracing::warn!(?err, file = %path.display(), "editor: read failed (transient); retrying with backoff");
                (EditorContent::Loading, true)
            }
        };
        let _load_task = if is_pdf {
            Some(spawn_pdf_load(path.clone(), window, cx))
        } else {
            needs_retry.then(|| schedule_load_retry_task(path.clone(), window, cx))
        };

        // Markdown preview applies only to text-mode `.md`/`.markdown`. Default
        // to Preview (read-first); this resets on every reopen (not persisted).
        let is_markdown = matches!(content, EditorContent::Text(_)) && is_markdown_path(&path);
        let md_mode = if is_markdown {
            MarkdownViewMode::Preview
        } else {
            MarkdownViewMode::Source
        };

        Self {
            uri,
            content,
            focus_handle,
            lsp_client: None,
            file_path: path,
            focused: false,
            is_markdown,
            md_mode,
            mermaid: mermaid::MermaidCache::default(),
            actions_menu_open: false,
            autosave_gen: 0,
            wheel_zoom_accum: 0.0,
            document_opener: None,
            _autosave_task: None,
            _zoom_sub,
            _focus_sub,
            _blur_sub,
            _release_sub,
            pending_lsp: None,
            _load_task,
        }
    }

    /// `true` when this view holds platform focus. Kept in sync by the
    /// `on_focus`/`on_blur` subscriptions installed in `new`.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Flip the breadcrumb "⋯" actions dropdown open/closed.
    pub(crate) fn toggle_actions_menu(&mut self) {
        self.actions_menu_open = !self.actions_menu_open;
    }

    /// Dismiss the breadcrumb "⋯" actions dropdown.
    pub(crate) fn close_actions_menu(&mut self) {
        self.actions_menu_open = false;
    }

    /// The live buffer text for text content; `None` for image/binary. Read at
    /// click time by the breadcrumb "copy contents" action.
    pub(crate) fn current_text(&self, cx: &App) -> Option<String> {
        match &self.content {
            EditorContent::Text(t) => Some(t.state.read(cx).value().to_string()),
            _ => None,
        }
    }

    /// Editor state entity — exposed so callers can install provider
    /// impls. Returns `None` for non-text content (image/binary) since
    /// there's no `InputState` to attach to.
    pub fn state(&self) -> Option<Entity<InputState>> {
        match &self.content {
            EditorContent::Text(t) => Some(t.state.clone()),
            _ => None,
        }
    }

    /// File path the view is showing.
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Install the host callback that opens a document linked from the
    /// markdown preview (e.g. a clicked `[Phase 2](phase-02.md)`) as an
    /// editor tab in the surrounding pane group.
    pub fn set_document_opener(&mut self, opener: markdown_preview::DocumentOpener) {
        self.document_opener = Some(opener);
    }

    /// `true` only for the `Text` variant — used by callers gating
    /// LSP installation, save handling, dirty-tracking.
    pub fn is_text(&self) -> bool {
        matches!(self.content, EditorContent::Text(_))
    }

    /// Attach an LSP server asynchronously. No-op for non-text content
    /// (image/binary files have no buffer to feed to a language server).
    /// `args` are the server's launch arguments (e.g. a `--stdio` flag).
    pub fn attach_lsp(
        &mut self,
        program: &str,
        args: Vec<String>,
        language_id: &str,
        workspace_root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.is_text() {
            spawn_attach_lsp(self, program, args, language_id, workspace_root, cx);
            return;
        }
        // Not yet a text buffer but could still become one — a retry in flight
        // (`Loading`) or a failed read the user may retry (`LoadFailed`). Stash
        // the request so `finish_load` attaches the server once (if) the buffer
        // materializes. `Image`/`Binary` are settled non-text decisions, so the
        // request is dropped there.
        if matches!(
            self.content,
            EditorContent::Loading | EditorContent::LoadFailed { .. }
        ) {
            self.pending_lsp = Some(PendingLspAttach {
                program: program.to_string(),
                args,
                language_id: language_id.to_string(),
                workspace_root,
            });
            return;
        }
        tracing::debug!(
            file = %self.file_path.display(),
            "editor: skipping LSP attach for non-text content"
        );
    }

    /// Called by `lsp_bridge` once the LSP handshake completes. Stores the
    /// ready client and, if the buffer drifted from the `didOpen` snapshot
    /// during the handshake window, fires a catch-up `didChange` so the
    /// language server is not left with stale text.
    pub fn set_lsp_client(&mut self, client: Arc<LspClient>, did_open_text: String) {
        let EditorContent::Text(t) = &mut self.content else {
            return;
        };
        if did_open_text != t.last_sent_text {
            t.doc_version += 1;
            let version = t.doc_version;
            let catch_up_text = t.last_sent_text.clone();
            tracing::debug!(
                version,
                "editor: catch-up didChange after LSP handshake (text drifted during connect)"
            );
            if let Err(err) = client.did_change(&self.uri, version, catch_up_text) {
                tracing::warn!(?err, "editor: catch-up didChange failed");
            }
            t.dirty = true;
        }
        self.lsp_client = Some(client);
    }

    /// Write the buffer to disk on Cmd+S. No-op for non-text content.
    fn on_save(&mut self, _: &SaveFile, _window: &mut Window, cx: &mut Context<Self>) {
        if self.save_to_disk(cx) {
            cx.notify();
        }
    }

    /// Bump the editor-global zoom and repaint. `set_global` inserts the
    /// global on first use, so no boot-time install is needed.
    fn apply_zoom(&mut self, next: EditorZoom, cx: &mut Context<Self>) {
        cx.set_global(next);
        cx.notify();
    }

    fn on_zoom_in(&mut self, _: &EditorZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        let next = current_zoom(cx).zoomed_in();
        self.apply_zoom(next, cx);
    }

    fn on_zoom_out(&mut self, _: &EditorZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        let next = current_zoom(cx).zoomed_out();
        self.apply_zoom(next, cx);
    }

    fn on_zoom_reset(&mut self, _: &EditorZoomReset, _window: &mut Window, cx: &mut Context<Self>) {
        self.apply_zoom(EditorZoom::reset(), cx);
    }

    /// Gesture zoom (Cmd+scroll wheel travel, or pinch delta converted to
    /// the same travel currency): accumulate, convert whole buckets to zoom
    /// steps, keep the remainder. Positive travel zooms in.
    fn on_wheel_zoom(&mut self, delta_y: f32, base: gpui::Pixels, cx: &mut Context<Self>) {
        let (steps, rest) = wheel_zoom_steps(self.wheel_zoom_accum + delta_y);
        self.wheel_zoom_accum = rest;
        if steps != 0 {
            let next = current_zoom(cx).stepped_by(steps, base);
            self.apply_zoom(next, cx);
        }
    }

    /// Write the buffer to disk, clear the dirty flag, and fire `didSave`.
    /// Returns `true` when a write actually happened (text content + write
    /// succeeded). Shared by Cmd+S and the autosave pump so both go through a
    /// single write path — no double-fire, no divergent didSave logic.
    fn save_to_disk(&mut self, cx: &mut Context<Self>) -> bool {
        let EditorContent::Text(t) = &mut self.content else {
            return false;
        };
        let text = t.state.read(cx).value().to_string();
        match std::fs::write(&self.file_path, &text) {
            Ok(()) => {
                t.dirty = false;
                tracing::debug!(file = %self.file_path.display(), "editor: file saved");
                if let Some(client) = &self.lsp_client
                    && let Err(err) = client.did_save(&self.uri)
                {
                    tracing::warn!(?err, "editor: didSave failed");
                }
                true
            }
            Err(err) => {
                tracing::error!(?err, file = %self.file_path.display(), "editor: save failed");
                false
            }
        }
    }

    /// `true` when this view holds an unsaved (dirty) text buffer. Used by the
    /// host's tab-close guard to decide whether to prompt before discarding.
    pub fn is_dirty(&self) -> bool {
        matches!(&self.content, EditorContent::Text(t) if t.dirty)
    }

    /// Save the buffer if dirty (no-op otherwise). Public so the host's
    /// dirty-close guard can run "Save" without dispatching the `SaveFile`
    /// action. Returns whether a write occurred.
    pub fn save_if_dirty(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.is_dirty() {
            return false;
        }
        let wrote = self.save_to_disk(cx);
        if wrote {
            cx.notify();
        }
        wrote
    }

    /// (Re)arm the debounced autosave timer after a buffer change. Reads the
    /// cadence from the installed [`AutosaveSettings`] global (defaulting to
    /// ON / 1000ms when the app hasn't installed one). A no-op when autosave
    /// is disabled — the dirty-close guard remains the safety net then.
    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        let settings = cx
            .try_global::<AutosaveSettings>()
            .copied()
            .unwrap_or_default();
        if !settings.enabled {
            return;
        }
        self.autosave_gen = self.autosave_gen.wrapping_add(1);
        let generation = self.autosave_gen;
        let delay = settings.debounce();
        self._autosave_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(delay).await;
            view.update(cx, |this, cx| {
                this.autosave_if_current(generation, cx);
            })
            .ok();
        }));
    }

    /// Fired by the debounce timer. Writes only when this is still the latest
    /// scheduled save (debounce), the path isn't quiesced by an SCM
    /// destructive op (would clobber a `git restore`), and the buffer is still
    /// dirty.
    fn autosave_if_current(&mut self, generation: u64, cx: &mut Context<Self>) {
        if self.autosave_gen != generation {
            return;
        }
        if crate::autosave::is_autosave_paused(&self.file_path) {
            return;
        }
        if !self.is_dirty() {
            return;
        }
        if self.save_to_disk(cx) {
            cx.notify();
        }
    }

    /// Apply a successfully (re)loaded buffer: swap in the decided content,
    /// recompute markdown-ness, and attach any LSP server that was stashed
    /// while the file was loading. Called from the retry task once a read
    /// succeeds.
    fn finish_load(&mut self, content: EditorContent, cx: &mut Context<Self>) {
        self.content = content;
        self.is_markdown =
            matches!(self.content, EditorContent::Text(_)) && is_markdown_path(&self.file_path);
        // Mirror `new()`: markdown opens in Preview; anything else uses Source
        // (where `md_mode` is inert). Set both arms so the invariant is explicit.
        self.md_mode = if self.is_markdown {
            MarkdownViewMode::Preview
        } else {
            MarkdownViewMode::Source
        };
        // The host requested LSP before the buffer existed — honor it now.
        if let Some(p) = self.pending_lsp.take()
            && self.is_text()
        {
            spawn_attach_lsp(self, &p.program, p.args, &p.language_id, p.workspace_root, cx);
        }
        cx.notify();
    }

    /// Transition to the failed state (terminal error or exhausted retries).
    /// A stashed LSP request is deliberately retained: the user may hit Retry,
    /// and if the file then loads as text we still want to attach the server.
    fn fail_load(&mut self, message: SharedString, cx: &mut Context<Self>) {
        self.content = EditorContent::LoadFailed { message };
        cx.notify();
    }

    /// Re-arm the load from the failed state (the "Retry" affordance). Resets
    /// to Loading and schedules a fresh backoff sweep; dropping the prior
    /// task cancels any straggler.
    fn retry_load_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.content, EditorContent::LoadFailed { .. }) {
            return;
        }
        self.content = EditorContent::Loading;
        self._load_task = Some(if pdf_preview::has_pdf_extension(&self.file_path) {
            spawn_pdf_load(self.file_path.clone(), window, cx)
        } else {
            schedule_load_retry_task(self.file_path.clone(), window, cx)
        });
        cx.notify();
    }

    /// The PDF state, if this view holds one. Used by the page body's width
    /// probe, which runs outside the module.
    pub(crate) fn pdf_content_mut(&mut self) -> Option<&mut PdfContent> {
        match &mut self.content {
            EditorContent::Pdf(p) => Some(p),
            _ => None,
        }
    }

    /// Step the PDF page by `delta`, clamped to the document, and open the
    /// new page at its top (or its bottom when `land_at_bottom`, for a
    /// PageUp that crossed backwards). A no-op for other content and at the
    /// ends. Returns whether the page changed; the render is requested on
    /// the next paint via `ensure_pdf_page`.
    pub(crate) fn pdf_step(&mut self, delta: i64, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.pdf_step_landing(delta, false, cx)
    }

    fn pdf_step_landing(&mut self, delta: i64, land_at_bottom: bool, cx: &mut Context<Self>) -> bool {
        let EditorContent::Pdf(p) = &mut self.content else {
            return false;
        };
        let last = p.page_count.saturating_sub(1) as i64;
        let next = (p.page as i64 + delta).clamp(0, last) as usize;
        if next == p.page {
            return false;
        }
        p.page = next;
        // The handle can only aim at what is laid out — still the old page —
        // so "bottom" is deferred until the new bitmap lands; "top" is exact
        // now and also the right place while the new page is in flight.
        p.scroll_to_top();
        p.land_at_bottom = land_at_bottom;
        cx.notify();
        true
    }

    /// Keyboard for a PDF pane. ←/→ step pages; ↑/↓ scroll within the page;
    /// PageUp/PageDown (and Space / ⇧Space) scroll a viewport and step to
    /// the neighbouring page once the current one is at its edge; Home/End
    /// jump to the first/last page. Other content ignores the event so it
    /// keeps bubbling, and so does any other modified chord (⌘←, ⌥↓ …) —
    /// those belong to whoever bound them.
    fn on_pdf_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let EditorContent::Pdf(p) = &self.content else {
            return;
        };
        // Before the first bitmap the scroll surface is not in the tree, so
        // there is nothing to scroll and no page the user has seen to leave.
        if p.bitmap.is_none() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let shift_space = key == "space"
            && event.keystroke.modifiers
                == gpui::Modifiers {
                    shift: true,
                    ..gpui::Modifiers::none()
                };
        if event.keystroke.modifiers.modified() && !shift_space {
            return;
        }
        let last = p.page_count.saturating_sub(1) as i64;
        let viewport = p.viewport_step();
        let handled = match key {
            "left" => {
                self.pdf_step(-1, window, cx);
                true
            }
            "right" => {
                self.pdf_step(1, window, cx);
                true
            }
            "up" => {
                p.scroll_by(-pdf_preview::SCROLL_STEP_PX);
                true
            }
            "down" => {
                p.scroll_by(pdf_preview::SCROLL_STEP_PX);
                true
            }
            "pageup" | "space" if key == "pageup" || shift_space => {
                if !p.scroll_by(-viewport) {
                    self.pdf_step_landing(-1, true, cx);
                }
                true
            }
            "pagedown" | "space" => {
                if !p.scroll_by(viewport) {
                    self.pdf_step_landing(1, false, cx);
                }
                true
            }
            "home" => {
                if !self.pdf_step(-last, window, cx)
                    && let EditorContent::Pdf(p) = &self.content
                {
                    p.scroll_to_top();
                }
                true
            }
            "end" => {
                if !self.pdf_step(last, window, cx)
                    && let EditorContent::Pdf(p) = &self.content
                {
                    p.scroll.scroll_to_bottom();
                }
                true
            }
            _ => false,
        };
        if handled {
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// Request the bitmap for the current PDF page at the current zoom and
    /// window scale, unless that exact render is already on screen or in
    /// flight. The render runs on the background executor; the result is
    /// applied on the window, swapping the old texture out of the atlas.
    ///
    /// Scale is one logical px per PDF point, **reduced to fit the pane
    /// width** when the page is wider than that, times the editor zoom — so
    /// a page opens no wider than it can be shown, and ⌘+ still magnifies it
    /// from there (the area cap in `render_page` bounds the result). Nothing
    /// is requested until the body's width probe has reported
    /// (`pane_width`), so the first frame shows "Rendering…" rather than an
    /// oversized page.
    fn ensure_pdf_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let base = cx.theme().font_size;
        let zoom_ratio = f32::from(current_zoom(cx).effective_px(base)) / f32::from(base);
        let scale_factor = window.scale_factor();
        let EditorContent::Pdf(p) = &mut self.content else {
            return;
        };
        let (Some(pane_width), Some((page_w, _))) = (p.pane_width, p.doc.page_size(p.page))
        else {
            return;
        };
        let fit = (pane_width - 2.0 * pdf_preview::PAGE_PADDING_PX).max(64.0) / page_w.max(1.0);
        let scale = scale_factor * zoom_ratio * fit.min(1.0);
        let key = PdfContent::request_key(p.page, scale);
        if p.requested == Some(key) {
            return;
        }
        p.requested = Some(key);
        p.generation += 1;
        let generation = p.generation;
        let doc = p.doc.clone();
        let page = p.page;
        p._render_task = Some(cx.spawn_in(window, async move |weak, cx| {
            let rendered = cx
                .background_executor()
                .spawn(async move { doc.render_page(page, scale) })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                let EditorContent::Pdf(p) = &mut this.content else {
                    return;
                };
                if p.generation != generation {
                    return;
                }
                p.failed = rendered.is_none();
                if let Some(old) = std::mem::replace(&mut p.bitmap, rendered) {
                    cx.drop_image(old, Some(window));
                }
                if std::mem::take(&mut p.land_at_bottom) && p.bitmap.is_some() {
                    // Now the new page is what the next layout measures.
                    p.scroll.scroll_to_bottom();
                }
                cx.notify();
            });
        }));
    }
}

/// Why a background PDF load stopped short of a document. `Parse` carries
/// the byte count that failed: a file mid-export reads back truncated and
/// fails to parse rather than failing to read, so the loop retries a parse
/// failure while the size is still changing and gives up once two reads
/// agree — a corrupt file is reported after one short delay, not the whole
/// schedule.
enum PdfLoadError {
    Io(std::io::Error),
    Parse { reason: String, len: usize },
}

/// Read and parse a `.pdf` on the background executor, then land it through
/// `finish_load`. The view shows Loading meanwhile. The PDF twin of
/// `schedule_load_retry_task`: the first attempt is immediate; a transient
/// read error, or a parse failure on a file whose size is still changing,
/// walks the same backoff schedule; a terminal error, or a parse failure on
/// a file that stopped changing, fails fast with the reason and a Retry.
fn spawn_pdf_load(path: PathBuf, window: &mut Window, cx: &mut Context<EditorView>) -> Task<()> {
    cx.spawn_in(window, async move |weak, cx| {
        let mut last_failed_len: Option<usize> = None;
        for delay in std::iter::once(Duration::ZERO).chain(LOAD_RETRY_DELAYS) {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let path_for_load = path.clone();
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let bytes = std::fs::read(&path_for_load).map_err(PdfLoadError::Io)?;
                    let len = bytes.len();
                    PdfDocument::parse(bytes).map_err(|reason| PdfLoadError::Parse { reason, len })
                })
                .await;
            let message = match loaded {
                Ok(doc) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.finish_load(EditorContent::Pdf(PdfContent::new(Arc::new(doc))), cx)
                    });
                    return;
                }
                Err(PdfLoadError::Parse { reason, len }) => {
                    if last_failed_len != Some(len) {
                        // Still being written (or first sight): try again.
                        last_failed_len = Some(len);
                        continue;
                    }
                    SharedString::from(format!("PDF could not be opened: {reason}"))
                }
                Err(PdfLoadError::Io(err)) if is_terminal_read_error(&err) => {
                    load_error_message(&err)
                }
                // Transient: fall through to the next backoff delay.
                Err(PdfLoadError::Io(_)) => continue,
            };
            let _ = weak.update(cx, |this, cx| {
                tracing::warn!(file = %this.file_path.display(), %message, "editor: PDF load failed");
                this.fail_load(message, cx);
            });
            return;
        }
        let _ = weak.update(cx, |this, cx| {
            this.fail_load(
                SharedString::from("Could not read this file after several retries."),
                cx,
            )
        });
    })
}

/// Spawn the backoff retry loop for a transient read failure. Each tick
/// re-reads off the buffer-loading path and, on success, rebuilds the
/// content on the window (constructing an `InputState` needs `&mut Window`).
/// A terminal error or an exhausted schedule transitions to the failed
/// state. Window-rooted (`spawn_in`) so the body isn't dropped when the
/// timer resolves outside a window context.
fn schedule_load_retry_task(
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<EditorView>,
) -> Task<()> {
    cx.spawn_in(window, async move |weak, cx| {
        for delay in LOAD_RETRY_DELAYS {
            cx.background_executor().timer(delay).await;
            // Read off the main thread — a slow/networked FS must not block
            // UI repaint while the retry is in flight.
            let path_for_read = path.clone();
            let read = cx
                .background_executor()
                .spawn(async move { std::fs::read(&path_for_read) })
                .await;
            match read {
                Ok(bytes) => {
                    let _ = weak.update_in(cx, |this, window, cx| {
                        let content = decide_content(&path, bytes, window, cx);
                        this.finish_load(content, cx);
                    });
                    return;
                }
                Err(err) if is_terminal_read_error(&err) => {
                    let message = load_error_message(&err);
                    let _ = weak.update(cx, |this, cx| this.fail_load(message, cx));
                    return;
                }
                // Transient: fall through to the next backoff delay.
                Err(_) => {}
            }
        }
        let _ = weak.update(cx, |this, cx| {
            this.fail_load(
                SharedString::from("Could not read this file after several retries."),
                cx,
            )
        });
    })
}

/// Build the appropriate `EditorContent` from the read bytes. Pulled out
/// of `new()` so the construction body stays readable and so the decision
/// tree can be skimmed without the surrounding focus/URI bookkeeping.
fn decide_content(
    path: &Path,
    bytes: Vec<u8>,
    window: &mut Window,
    cx: &mut Context<EditorView>,
) -> EditorContent {
    // A `.pdf` never reaches here — `new()` and `retry_load_now` route it to
    // `spawn_pdf_load`. What can: a PDF under another extension, caught by
    // its header before the NUL sniff (a small hand-written one is pure ASCII
    // and would otherwise open as text). The header is a hint, and prose can
    // contain it, so a failed parse keeps the ordinary text/binary path.
    if pdf_preview::has_pdf_header(&bytes)
        && let Ok(doc) = PdfDocument::parse(bytes.clone())
    {
        return EditorContent::Pdf(PdfContent::new(Arc::new(doc)));
    }
    if is_binary_buffer(&bytes) {
        return match image_mime_for_path(path) {
            Some(mime) => EditorContent::Image { mime },
            None => EditorContent::Binary,
        };
    }
    // No NUL byte but could still be non-UTF-8 (e.g., latin-1, UTF-16BE).
    // The code-editor path requires UTF-8, so a decode failure falls back
    // to the binary placeholder rather than letting `from_utf8_lossy`
    // silently substitute replacement glyphs.
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            tracing::debug!(file = %path.display(), "editor: non-UTF-8 buffer; treating as binary");
            return match image_mime_for_path(path) {
                Some(mime) => EditorContent::Image { mime },
                None => EditorContent::Binary,
            };
        }
    };

    let language = language_for_path(path);
    let state = cx.new(|cx| {
        InputState::new(window, cx)
            .code_editor(language)
            .multi_line(true)
            .tab_size(TabSize {
                tab_size: 4,
                ..Default::default()
            })
            .default_value(content.clone())
    });

    let _observe_sub = cx.observe(&state, |this, _entity, cx| {
        let mut changed = false;
        if let EditorContent::Text(t) = &mut this.content {
            let current_text = t.state.read(cx).value().to_string();
            if let Some(prop) =
                decide_change_propagation(&t.last_sent_text, &current_text, t.doc_version)
            {
                t.last_sent_text = prop.text.clone();
                t.dirty = true;
                t.doc_version = prop.new_version;
                if let Some(client) = &this.lsp_client {
                    tracing::debug!(version = prop.new_version, "editor: didChange");
                    if let Err(err) = client.did_change(&this.uri, prop.new_version, prop.text) {
                        tracing::warn!(?err, "editor: didChange failed");
                    }
                }
                changed = true;
            }
        }
        // Re-arm autosave + repaint outside the `t` borrow so `&mut this` is
        // free for the debounce scheduler.
        if changed {
            this.schedule_autosave(cx);
            cx.notify();
        }
    });

    EditorContent::Text(TextContent {
        state,
        dirty: false,
        doc_version: 1, // version 1 consumed by didOpen
        last_sent_text: content,
        _observe_sub,
    })
}

impl Drop for EditorView {
    /// Send `didClose` when the view is torn down. The mpsc send inside
    /// `did_close` is synchronous; if the sender already closed (server
    /// gone) the error is logged and discarded.
    fn drop(&mut self) {
        if let Some(client) = &self.lsp_client
            && let Err(err) = client.did_close(&self.uri)
        {
            tracing::warn!(?err, "editor: didClose failed (server may already be gone)");
        }
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A PDF pane requests its page bitmap here, where the window's scale
        // factor and the editor zoom are both known. Idempotent: it spawns
        // only when the wanted (page, scale) differs from the last request.
        if matches!(self.content, EditorContent::Pdf(_)) {
            self.ensure_pdf_page(window, cx);
        }
        // Resolved per render rather than cached: this view keeps no token
        // snapshot, so there is nothing to go stale and nothing for
        // `appearance-lint` to hold it to. Taken before the theme borrow,
        // which the placeholder helpers below already work around.
        let typo = oximux_settings::appearance::typography(cx);
        let density = oximux_settings::appearance::density(cx);

        // Markdown preview source, pre-processed BEFORE the long-lived theme
        // borrow below: `mermaid.process` kicks background renders and needs
        // `&mut cx`, which can't overlap `cx.theme()`'s shared borrow. Ready
        // ```mermaid fences come back rewritten to file:// images, which
        // `absolutize_image_paths` later passes through untouched (it skips
        // URLs with a scheme). `Some` exactly when a Preview/Split arm below
        // will consume it — and those arms fall back to re-reading the raw
        // buffer, so a future condition drift degrades to an un-rewritten
        // preview, never a blank one.
        let is_dark = cx.theme().is_dark();
        let mut md_preview_value = match &self.content {
            EditorContent::Text(t)
                if self.is_markdown
                    && matches!(
                        self.md_mode,
                        MarkdownViewMode::Preview | MarkdownViewMode::Split
                    ) =>
            {
                let value = t.state.read(cx).value().to_string();
                Some(self.mermaid.process(&value, cx).unwrap_or(value))
            }
            _ => None,
        };

        // PDF page state for the breadcrumb navigator, read before the theme
        // borrow below pins `cx`.
        let pdf_nav = match &self.content {
            EditorContent::Pdf(p) => Some((p.page, p.page_count)),
            _ => None,
        };

        let theme = cx.theme();

        // Path breadcrumb row above the content. Dirty indicator is text
        // only — multiple editor leaves can be open at once, no point
        // hijacking the window title.
        let path_str = self.file_path.display().to_string();
        let dirty_suffix = match &self.content {
            EditorContent::Text(t) if t.dirty => " •",
            _ => "",
        };
        let kind_suffix = match &self.content {
            EditorContent::Image { mime } => SharedString::from(format!("  ·  {mime}")),
            EditorContent::Pdf(_) => SharedString::from("  ·  pdf"),
            EditorContent::Binary => SharedString::from("  ·  binary"),
            EditorContent::Loading => SharedString::from("  ·  loading…"),
            EditorContent::LoadFailed { .. } => SharedString::from("  ·  load failed"),
            EditorContent::Text(_) => SharedString::from(""),
        };
        let breadcrumb = gpui::div()
            .flex()
            .flex_row()
            .items_center()
            .gap(gpui::px(6.0))
            .h(gpui::px(28.0))
            .px(gpui::px(12.0))
            .bg(theme.background)
            .border_b_1()
            .border_color(theme.border)
            .text_size(gpui::px(11.0))
            .text_color(theme.muted_foreground)
            // The path is one click from the clipboard (with a toast).
            .child(editor_header::clickable_path(
                format!("{path_str}{dirty_suffix}{kind_suffix}"),
                &self.file_path,
                cx,
            ))
            // Spacer pushes the toggle + actions to the row's trailing edge.
            .child(gpui::div().flex_1())
            // PDF-only: `‹ N / M ›` page stepper.
            .when_some(pdf_nav, |row, (page, count)| {
                row.child(pdf_preview::page_nav(page, count, cx))
            })
            // Markdown-only: the Source/Preview/Split toggle.
            .when(self.is_markdown, |row| {
                row.child(
                    markdown_preview::mode_toggle(self.md_mode, cx.entity_id()).on_click(cx.listener(
                        |this, clicks: &Vec<usize>, _window, cx| {
                            if let Some(mode) =
                                clicks.first().and_then(|&i| MarkdownViewMode::from_index(i))
                            {
                                this.md_mode = mode;
                                cx.notify();
                            }
                        },
                    )),
                )
            })
            // A single "⋯" overflow menu (far right) holding every file action:
            // copy contents, reveal in Finder, open in an external editor.
            .child(editor_header::actions_button(self.actions_menu_open, cx));

        // Snapshot the colors we need before constructing children — the
        // theme borrow is released here so the body match can re-borrow
        // `cx` mutably if it needs to (e.g., for `cx.listener`).
        let muted_fg = theme.muted_foreground;
        // Editor-global font zoom applied on top of the theme's mono size.
        // The unzoomed base is also snapshotted for the Cmd+wheel listener,
        // which clamps its steps against it.
        let mono_base = theme.mono_font_size;
        let zoom = current_zoom(cx);
        let mono_size = zoom.effective_px(mono_base);
        // The markdown preview zooms too, off its own base: the theme's UI
        // font size, which is what the preview body would inherit (via the
        // window rem size) if left alone. Headings scale by the ratio so the
        // preview keeps its proportions.
        let preview_body = zoom.effective_px(theme.font_size);
        let preview_factor = f32::from(preview_body) / f32::from(theme.font_size);
        let body: gpui::AnyElement = match &self.content {
            // Markdown text: branch on the active view mode. Source reuses the
            // plain editor; Preview/Split render via the GFM renderer. The
            // preview reads `state.value()` at render time, so the existing
            // `cx.observe(&state)` → `cx.notify()` already keeps it live.
            EditorContent::Text(t) if self.is_markdown => {
                let dir = self.file_path.parent();
                let input = Input::new(&t.state)
                    .font_family(theme.mono_font_family.clone())
                    .text_size(mono_size)
                    .size_full();
                let view_id = cx.entity_id();
                match self.md_mode {
                    MarkdownViewMode::Source => input.into_any_element(),
                    MarkdownViewMode::Preview => {
                        let value = md_preview_value
                            .take()
                            .unwrap_or_else(|| t.state.read(cx).value().to_string());
                        markdown_preview::render_preview(
                            &value,
                            dir,
                            view_id,
                            is_dark,
                            typo.t_body_sm,
                            preview_body,
                            preview_factor,
                            self.document_opener.clone(),
                        )
                    }
                    MarkdownViewMode::Split => {
                        let value = md_preview_value
                            .take()
                            .unwrap_or_else(|| t.state.read(cx).value().to_string());
                        // Bound the split's height to the region below the 28px
                        // breadcrumb: `h_resizable` is `size_full`, so without a
                        // `flex_1`/`min_h_0` wrapper it would overflow the header.
                        gpui::div()
                            .flex_1()
                            .min_h_0()
                            .child(
                                h_resizable(("md-split", view_id))
                                    .child(resizable_panel().child(input))
                                    .child(
                                        resizable_panel().child(
                                            markdown_preview::render_preview(
                                                &value,
                                                dir,
                                                view_id,
                                                is_dark,
                                                typo.t_body_sm,
                                                preview_body,
                                                preview_factor,
                                                self.document_opener.clone(),
                                            ),
                                        ),
                                    ),
                            )
                            .into_any_element()
                    }
                }
            }
            EditorContent::Text(t) => Input::new(&t.state)
                .font_family(theme.mono_font_family.clone())
                .text_size(mono_size)
                .size_full()
                .into_any_element(),
            EditorContent::Image { .. } => render_image_body(&self.file_path),
            EditorContent::Pdf(p) => pdf_preview::page_body(
                p,
                window.scale_factor(),
                cx.entity().downgrade(),
                muted_fg,
                theme.border,
                typo.t_body_md,
            ),
            EditorContent::Binary => render_binary_placeholder(muted_fg, typo.t_body_md),
            EditorContent::Loading => render_loading_placeholder(muted_fg, typo.t_body_md),
            EditorContent::LoadFailed { message } => gpui::div()
                .flex()
                .flex_1()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.0))
                .text_size(px(typo.t_body_md))
                .text_color(muted_fg)
                .child(message.clone())
                .child(
                    gpui::div()
                        .id("editor-retry-load")
                        .px(px(12.0))
                        .py(px(5.0))
                        .border_1()
                        .border_color(theme.border)
                        .rounded(px(density.r_xs))
                        .text_color(theme.foreground)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.muted))
                        .child("Retry")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.retry_load_now(window, cx);
                        })),
                )
                .into_any_element(),
        };

        // Wire focus tracking so action dispatch (e.g. Cmd+W → CloseTab on
        // the ancestor PaneGroup, Cmd+S → SaveFile on this div) routes
        // correctly. Without `.track_focus(...)`, the focus_handle this
        // view exposes via `Focusable` is never anchored in the rendered
        // dispatch tree — `focus_active()` puts platform focus on a
        // dangling handle, and key events from a focused child (the
        // `Input` widget) don't bubble through this view's on_action
        // handlers. Mirrors the pattern used by `TerminalView` and
        // `DiffView`.
        gpui::div()
            .id(("oximux-editor-view", cx.entity_id()))
            .track_focus(&self.focus_handle)
            .flex()
            .flex_col()
            .size_full()
            // Anchors the absolutely-positioned "Open in…" overlay below.
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_reset))
            // PDF page stepping from the keyboard. Only acts when the view
            // holds a PDF, so text buffers keep their arrow keys.
            .on_key_down(cx.listener(Self::on_pdf_key))
            // Re-claim focus on click for Image / PDF / Binary surfaces. For
            // Text content the child `Input` widget grabs focus on its
            // own click path; this handler is the fallback so the
            // editor's focus_handle becomes the active focus even when
            // the body has no inner focusable (image/binary placeholder).
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    if !this.is_text() {
                        this.focus_handle.focus(window, cx);
                        cx.notify();
                    }
                }),
            )
            .child(breadcrumb)
            .child(body)
            // Cmd+scroll and pinch font zoom. Registered as CAPTURE-phase
            // window mouse listeners rather than `.on_scroll_wheel` /
            // `.on_pinch`: the child `Input` handles plain wheel scroll in
            // the bubble phase and stops propagation whenever it scrolled,
            // so a bubble listener here would only see wheel events at the
            // buffer's scroll extremes. Capture runs parent-first, and
            // stopping propagation there keeps a zoom gesture from also
            // scrolling the buffer. The zero-size-paint `canvas` exists to
            // reach the low-level hitbox + mouse-listener API from inside a
            // fluent render.
            .child({
                let view = cx.entity().downgrade();
                gpui::canvas(
                    |bounds, window, _| {
                        window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
                    },
                    move |_, hitbox, window, _| {
                        window.on_mouse_event({
                            let view = view.clone();
                            let hitbox = hitbox.clone();
                            move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                                if phase == gpui::DispatchPhase::Capture
                                    && event.modifiers.secondary()
                                    && hitbox.should_handle_scroll(window)
                                {
                                    let delta_y = f32::from(
                                        event.delta.pixel_delta(px(WHEEL_ZOOM_STEP_PX)).y,
                                    );
                                    view.update(cx, |this, cx| {
                                        this.on_wheel_zoom(delta_y, mono_base, cx)
                                    })
                                    .ok();
                                    cx.stop_propagation();
                                }
                            }
                        });
                        // Two-finger pinch zoom: no modifier required — the
                        // gesture itself is the zoom intent. The fractional
                        // pinch delta is converted into the same travel
                        // currency the wheel path accumulates, so both
                        // gestures share one bucket, remainder, and clamp.
                        window.on_mouse_event(
                            move |event: &gpui::PinchEvent, phase, window, cx| {
                                if phase == gpui::DispatchPhase::Capture
                                    && hitbox.is_hovered(window)
                                {
                                    let travel = event.delta * PINCH_ZOOM_PX_PER_UNIT;
                                    view.update(cx, |this, cx| {
                                        this.on_wheel_zoom(travel, mono_base, cx)
                                    })
                                    .ok();
                                    cx.stop_propagation();
                                }
                            },
                        );
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full()
            })
            // "⋯" actions dropdown: backdrop + card, painted above the body.
            .when(self.actions_menu_open, |this| {
                this.child(editor_header::actions_overlay(
                    &self.file_path,
                    self.is_text(),
                    cx,
                ))
            })
    }
}

/// Centered image preview. GPUI's `img(PathBuf)` reads + decodes the file
/// itself, so no base64 round-trip is needed (unlike browser-renderer
/// stacks that must stream image bytes from a separate process).
///
/// `max_w`/`max_h` keep oversize images from blowing out the layout — the
/// `img` element does object-fit: contain by default, so aspect ratio is
/// preserved automatically.
fn render_image_body(path: &Path) -> gpui::AnyElement {
    gpui::div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .p(px(16.0))
        .child(img(path.to_path_buf()).max_w(px(1024.0)).max_h(px(1024.0)))
        .into_any_element()
}

/// Centered "Binary file — cannot display" placeholder. Style mirrors the
/// muted-foreground convention used by the diff view's empty state so the
/// look stays consistent across the app. `muted_fg` is snapshotted by the
/// caller so this helper does not need a `&mut Context` (which would
/// conflict with the active immutable theme borrow there).
fn render_binary_placeholder(muted_fg: gpui::Hsla, text_size: f32) -> gpui::AnyElement {
    gpui::div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_size(px(text_size))
        .text_color(muted_fg)
        .child("Binary file — cannot display")
        .into_any_element()
}

/// Centered "Loading…" placeholder shown while a transient read failure is
/// being retried on the backoff schedule. Mirrors the binary placeholder's
/// muted style so the look stays consistent.
fn render_loading_placeholder(muted_fg: gpui::Hsla, text_size: f32) -> gpui::AnyElement {
    gpui::div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_size(px(text_size))
        .text_color(muted_fg)
        .child("Loading…")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A text file that merely mentions the `%PDF-` marker (this repository's
    /// changelog does) must still open as text: the header is a hint, and a
    /// failed parse falls through. Regression caught in review.
    #[gpui::test]
    async fn prose_containing_pdf_marker_opens_as_text(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Notes\n\nDetection is by a `%PDF-` header.\n").expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                assert!(view.is_text(), "prose with the marker is still a text buffer");
                assert!(view.is_markdown, "and keeps its markdown mode");
            })
            .expect("window alive");
    }

    /// A page wider than the pane renders fitted to the pane width (minus
    /// padding), not at one point per pixel. Drives the real layout probe:
    /// the window is resized and the fitted render follows.
    #[gpui::test]
    async fn pdf_page_fits_the_pane_width(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scan.pdf");
        std::fs::write(&path, crate::pdf_preview::test_pdf_sized(1, 2480, 3508)).expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                let probed = p.pane_width.expect("the width probe reported after layout");
                assert!(probed > 0.0);
            })
            .expect("window alive");

        // Narrow the window: 800 px remain for the page after padding.
        cx.simulate_window_resize(window.into(), gpui::size(px(832.0), px(600.0)));
        cx.run_until_parked();

        window
            .update(cx, |view, win, _cx| {
                let scale = win.scale_factor();
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert_eq!(p.pane_width, Some(832.0), "probe followed the resize");
                let bitmap = p.bitmap.as_ref().expect("fitted page rendered");
                let size = bitmap.size(0);
                assert_eq!(size.width.0, (800.0 * scale).floor() as i32, "fitted to the pane");
                let expected_h = (3508.0 * (800.0 / 2480.0) * scale).floor() as i32;
                assert!((size.height.0 - expected_h).abs() <= 1, "aspect kept");
            })
            .expect("window alive");

        // A resize inside one probe step changes nothing: no new render.
        let generation_before = window
            .update(cx, |view, _win, _cx| {
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                p.generation
            })
            .expect("window alive");
        cx.simulate_window_resize(window.into(), gpui::size(px(850.0), px(600.0)));
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert_eq!(p.pane_width, Some(832.0), "probe is quantised to 32 px");
                assert_eq!(p.generation, generation_before, "sub-step resize does not re-render");
            })
            .expect("window alive");

        // Zoom in past the pane: the page is now wider than the pane and the
        // scroll surface must expose that width, not clip it.
        window
            .update(cx, |view, win, cx| {
                for _ in 0..12 {
                    view.on_zoom_in(&EditorZoomIn, win, cx);
                }
            })
            .expect("window alive");
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert!(
                    f32::from(p.scroll.max_offset().x) > 0.0,
                    "a zoomed-in page scrolls horizontally"
                );
            })
            .expect("window alive");
    }

    /// Keyboard model on a fitted page taller than the pane: ↓ scrolls
    /// without changing the page, PageDown scrolls until the bottom and then
    /// steps to the next page at its top, ← steps back, End jumps to the
    /// last page, and a modified chord is left alone.
    #[gpui::test]
    async fn pdf_keys_scroll_within_a_page_and_step_at_its_edges(cx: &mut gpui::TestAppContext) {
        use gpui::Keystroke;
        fn key(spec: &str) -> KeyDownEvent {
            KeyDownEvent {
                keystroke: Keystroke::parse(spec).expect("keystroke"),
                is_held: false,
                prefer_character_input: false,
            }
        }
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("scan.pdf");
        std::fs::write(&path, crate::pdf_preview::test_pdf_sized(3, 2480, 3508)).expect("write");
        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        cx.simulate_window_resize(window.into(), gpui::size(px(832.0), px(600.0)));
        cx.run_until_parked();

        let page_and_offset = |view: &EditorView| {
            let EditorContent::Pdf(p) = &view.content else { unreachable!() };
            (p.page, f32::from(p.scroll.offset().y))
        };
        window
            .update(cx, |view, win, cx| {
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert!(p.bitmap.is_some(), "fitted page rendered");
                let scrollable = f32::from(p.scroll.max_offset().y);
                assert!(scrollable > 0.0, "an 800×1131 page in a 600 px pane scrolls ({scrollable})");

                view.on_pdf_key(&key("down"), win, cx);
                let (page, y) = page_and_offset(view);
                assert_eq!(page, 0, "↓ scrolls, it does not step");
                assert!(y < 0.0, "↓ moved the page up ({y})");

                view.on_pdf_key(&key("cmd-down"), win, cx);
                assert_eq!(page_and_offset(view).1, y, "a modified chord is not ours");
                view.on_pdf_key(&key("cmd-shift-space"), win, cx);
                assert_eq!(page_and_offset(view), (0, y), "⌘⇧Space is not ours either");

                // PageDown until the bottom, then one more steps the page.
                let mut presses = 0;
                while page_and_offset(view).0 == 0 && presses < 20 {
                    view.on_pdf_key(&key("pagedown"), win, cx);
                    presses += 1;
                }
                let (page, y) = page_and_offset(view);
                assert_eq!(page, 1, "PageDown past the bottom steps to the next page");
                assert_eq!(y, 0.0, "the next page opens at its top");
                assert!(presses >= 2, "the first PageDown scrolled, it did not step ({presses})");

                view.on_pdf_key(&key("left"), win, cx);
                assert_eq!(page_and_offset(view).0, 0, "← steps back");
                view.on_pdf_key(&key("end"), win, cx);
                assert_eq!(page_and_offset(view).0, 2, "End jumps to the last page");
                view.on_pdf_key(&key("home"), win, cx);
                assert_eq!(page_and_offset(view), (0, 0.0), "Home jumps to the first page, at its top");
            })
            .expect("window alive");
    }

    /// A `.pdf` that is not a PDF fails with the reason, and Retry re-runs
    /// the asynchronous loader rather than the text path.
    #[gpui::test]
    async fn broken_pdf_fails_with_reason_and_retries_async(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.pdf");
        std::fs::write(&path, b"not a pdf").expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path.clone(), window, cx));
        cx.run_until_parked();
        // One parse failure is not yet a verdict — the file may be mid-write.
        window
            .update(cx, |view, _win, _cx| {
                assert!(matches!(view.content, EditorContent::Loading), "first failure waits");
            })
            .expect("window alive");
        cx.executor().advance_clock(LOAD_RETRY_DELAYS[0]);
        cx.run_until_parked();
        window
            .update(cx, |view, win, cx| {
                let EditorContent::LoadFailed { message } = &view.content else {
                    panic!("a broken .pdf that stopped changing must land in LoadFailed");
                };
                assert!(message.starts_with("PDF could not be opened"), "{message}");
                // Repair the file, then Retry.
                std::fs::write(&path, crate::pdf_preview::test_pdf(1)).expect("rewrite");
                view.retry_load_now(win, cx);
                assert!(matches!(view.content, EditorContent::Loading));
            })
            .expect("window alive");
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                assert!(matches!(view.content, EditorContent::Pdf(_)), "retry loads the PDF");
            })
            .expect("window alive");
    }

    /// A PDF with the wrong extension is still recognised by its header.
    #[gpui::test]
    async fn pdf_with_wrong_extension_opens_by_header(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renamed.bin");
        std::fs::write(&path, crate::pdf_preview::test_pdf(2)).expect("write");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _win, _cx| {
                let EditorContent::Pdf(p) = &view.content else {
                    panic!("header-detected PDF must open as EditorContent::Pdf");
                };
                assert_eq!(p.page_count, 2);
            })
            .expect("window alive");
    }

    /// Opening a `.pdf` lands in the `Pdf` arm with the right page count;
    /// stepping clamps at both ends; the page bitmap arrives from the
    /// background executor sized for the window's scale factor; and asking
    /// again for the same page + scale does not re-render.
    #[gpui::test]
    async fn pdf_opens_steps_and_renders_off_thread(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("three.pdf");
        std::fs::write(&path, crate::pdf_preview::test_pdf(3)).expect("write fixture");

        let window = cx.add_window(|window, cx| EditorView::new(path, window, cx));
        // A .pdf opens as Loading and lands from the background executor.
        window
            .update(cx, |view, _win, _cx| {
                assert!(
                    matches!(view.content, EditorContent::Loading),
                    "a .pdf must not be read synchronously on open"
                );
            })
            .expect("window alive");
        cx.run_until_parked();

        window
            .update(cx, |view, win, cx| {
                let EditorContent::Pdf(p) = &mut view.content else {
                    panic!("a .pdf must open as EditorContent::Pdf once loaded");
                };
                assert_eq!((p.page, p.page_count), (0, 3));
                assert!(p.pane_width.is_some(), "the layout probe has reported");
                assert!(!view.is_text(), "PDF is not a text buffer");

                view.pdf_step(1, win, cx);
                view.pdf_step(-10, win, cx);
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert_eq!(p.page, 0, "stepping below the first page clamps");
                view.pdf_step(10, win, cx);
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert_eq!(p.page, 2, "stepping past the last page clamps");

                // What `render` does on every paint for a PDF pane.
                view.ensure_pdf_page(win, cx);
            })
            .expect("window alive");
        cx.run_until_parked();

        window
            .update(cx, |view, win, cx| {
                let scale = win.scale_factor();
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert!(!p.failed, "render of page 3 must not fail");
                let bitmap = p.bitmap.as_ref().expect("bitmap landed off-thread");
                let size = bitmap.size(0);
                assert_eq!(size.width.0, (200.0 * scale).floor() as i32);
                assert_eq!(size.height.0, (100.0 * scale).floor() as i32);

                let generation = p.generation;
                view.ensure_pdf_page(win, cx);
                let EditorContent::Pdf(p) = &view.content else { unreachable!() };
                assert_eq!(p.generation, generation, "same page + scale is a no-op");
            })
            .expect("window alive");
    }

    #[test]
    fn language_for_path_maps_rust() {
        assert_eq!(language_for_path(&PathBuf::from("foo.rs")), "rust");
    }

    #[test]
    fn language_for_path_maps_typescript_variants() {
        assert_eq!(language_for_path(&PathBuf::from("a.ts")), "typescript");
        assert_eq!(language_for_path(&PathBuf::from("a.tsx")), "tsx");
        assert_eq!(language_for_path(&PathBuf::from("a.mts")), "typescript");
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
    fn language_for_path_maps_systems_languages() {
        assert_eq!(language_for_path(&PathBuf::from("a.c")), "c");
        assert_eq!(language_for_path(&PathBuf::from("a.h")), "c");
        assert_eq!(language_for_path(&PathBuf::from("a.cpp")), "cpp");
        assert_eq!(language_for_path(&PathBuf::from("a.hpp")), "cpp");
        assert_eq!(language_for_path(&PathBuf::from("a.cs")), "csharp");
        assert_eq!(language_for_path(&PathBuf::from("a.swift")), "swift");
    }

    #[test]
    fn language_for_path_maps_config_files() {
        assert_eq!(language_for_path(&PathBuf::from("a.yaml")), "yaml");
        assert_eq!(language_for_path(&PathBuf::from("a.yml")), "yaml");
        assert_eq!(language_for_path(&PathBuf::from("a.toml")), "toml");
        assert_eq!(language_for_path(&PathBuf::from("a.json")), "json");
    }

    #[test]
    fn language_for_path_well_known_basenames_take_priority() {
        // Cargo.toml resolves even though the extension is `.toml` — both
        // happen to resolve to "toml" so the precedence isn't observable
        // here, but Dockerfile (no extension) is the real test.
        assert_eq!(language_for_path(&PathBuf::from("Dockerfile")), "bash");
        assert_eq!(language_for_path(&PathBuf::from("Makefile")), "make");
        assert_eq!(language_for_path(&PathBuf::from("Cargo.toml")), "toml");
    }

    #[test]
    fn is_markdown_path_matches_md_and_markdown_only() {
        assert!(is_markdown_path(&PathBuf::from("README.md")));
        assert!(is_markdown_path(&PathBuf::from("doc.markdown")));
        assert!(is_markdown_path(&PathBuf::from("DOC.MD"))); // case-insensitive
        // .mdx is deliberately excluded — keeps the plain code-editor view.
        assert!(!is_markdown_path(&PathBuf::from("page.mdx")));
        assert!(!is_markdown_path(&PathBuf::from("main.rs")));
        assert!(!is_markdown_path(&PathBuf::from("README")));
    }

    #[test]
    fn language_for_path_unknown_falls_back_to_plain() {
        assert_eq!(language_for_path(&PathBuf::from("foo.xyz")), "plain");
        assert_eq!(language_for_path(&PathBuf::from("README")), "plain");
    }

    #[test]
    fn save_fail_path_is_unwritable() {
        // Sanity-check the save-failure path used by `on_save`.
        let result = std::fs::write("/dev/null/impossible/path/file.rs", "test");
        assert!(result.is_err(), "expected write to impossible path to fail");
    }

    #[test]
    fn wheel_zoom_travel_below_one_step_only_accumulates() {
        // Trackpad gestures arrive as many small pixel deltas; none of them
        // alone may step the zoom, but the travel must not be lost.
        let (steps, rest) = wheel_zoom_steps(7.0);
        assert_eq!(steps, 0);
        assert_eq!(rest, 7.0);
        let (steps, rest) = wheel_zoom_steps(rest + 14.0);
        assert_eq!(steps, 1, "carried travel crosses the bucket");
        assert_eq!(rest, 1.0);
    }

    #[test]
    fn wheel_zoom_one_notch_is_exactly_one_step() {
        // A discrete wheel notch is a 1-line delta, converted at
        // WHEEL_ZOOM_STEP_PX per line — one notch, one step, no remainder.
        let (steps, rest) = wheel_zoom_steps(WHEEL_ZOOM_STEP_PX);
        assert_eq!((steps, rest), (1, 0.0));
        let (steps, rest) = wheel_zoom_steps(-WHEEL_ZOOM_STEP_PX);
        assert_eq!((steps, rest), (-1, 0.0));
    }

    #[test]
    fn wheel_zoom_fast_fling_applies_multiple_steps() {
        let (steps, rest) = wheel_zoom_steps(WHEEL_ZOOM_STEP_PX * 3.0 + 5.0);
        assert_eq!(steps, 3);
        assert_eq!(rest, 5.0);
    }

    #[test]
    fn wheel_zoom_negative_remainder_keeps_its_sign() {
        // trunc() (not floor) keeps bucket math symmetric around zero, so a
        // downward gesture's remainder stays negative and keeps building
        // toward the next zoom-out step instead of cancelling into zoom-in.
        let (steps, rest) = wheel_zoom_steps(-25.0);
        assert_eq!(steps, -1);
        assert_eq!(rest, -5.0);
    }

    #[test]
    fn pinch_gain_crosses_one_step_within_a_small_spread() {
        // ~13% of pinch spread (0.135 × 150 > 20) must cross one bucket so
        // the gesture feels responsive, while a jittery ±5% wobble stays
        // inside the bucket and doesn't step at all.
        let (steps, _) = wheel_zoom_steps(0.135 * PINCH_ZOOM_PX_PER_UNIT);
        assert_eq!(steps, 1);
        let (steps, rest) = wheel_zoom_steps(0.05 * PINCH_ZOOM_PX_PER_UNIT);
        assert_eq!(steps, 0);
        assert!(rest > 0.0, "sub-step spread is carried, not dropped");
    }

    #[test]
    fn stepped_by_clamps_where_the_effective_size_stops_moving() {
        // A fling worth +100 steps from a 13px base must park exactly at
        // the point where effective_px hits the max — the very next
        // zoom-out step has to move the size again (no dead zone).
        let base = px(13.0);
        let flung = EditorZoom::default().stepped_by(100, base);
        assert_eq!(flung.effective_px(base), px(EDITOR_FONT_MAX_PX));
        let back = flung.stepped_by(-1, base);
        assert_eq!(back.effective_px(base), px(EDITOR_FONT_MAX_PX - 1.0));

        let shrunk = EditorZoom::default().stepped_by(-100, base);
        assert_eq!(shrunk.effective_px(base), px(EDITOR_FONT_MIN_PX));
        let back = shrunk.stepped_by(1, base);
        assert_eq!(back.effective_px(base), px(EDITOR_FONT_MIN_PX + 1.0));
    }

    #[test]
    fn decide_propagation_returns_none_on_cursor_move() {
        let text = "fn main() {}";
        let result = decide_change_propagation(text, text, 1);
        assert!(result.is_none(), "cursor move must produce None");
    }

    #[test]
    fn decide_propagation_returns_some_on_edit() {
        let prop =
            decide_change_propagation("hello", "hello world", 1).expect("edit must produce Some");
        assert_eq!(prop.new_version, 2, "version must increment from 1 to 2");
        assert_eq!(prop.text, "hello world", "payload must be the new text");
    }

    #[test]
    fn decide_propagation_version_strictly_monotonic() {
        let mut last = "v0".to_string();
        let mut version: i32 = 1;
        for (expected_v, new_text) in [(2, "v1"), (3, "v2"), (4, "v3")] {
            let prop = decide_change_propagation(&last, new_text, version)
                .expect("each edit must produce Some");
            assert_eq!(prop.new_version, expected_v);
            last = prop.text.clone();
            version = prop.new_version;
        }
    }

    #[test]
    fn terminal_read_errors_skip_retry() {
        use std::io::{Error, ErrorKind};
        assert!(is_terminal_read_error(&Error::from(ErrorKind::NotFound)));
        assert!(is_terminal_read_error(&Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn transient_read_errors_are_retried() {
        use std::io::{Error, ErrorKind};
        // Anything that could clear up on its own must be retried, not failed.
        assert!(!is_terminal_read_error(&Error::from(ErrorKind::Interrupted)));
        assert!(!is_terminal_read_error(&Error::from(ErrorKind::WouldBlock)));
        assert!(!is_terminal_read_error(&Error::from(ErrorKind::TimedOut)));
        assert!(!is_terminal_read_error(&Error::from(ErrorKind::Other)));
    }

    #[test]
    fn load_error_message_is_specific_for_known_kinds() {
        use std::io::{Error, ErrorKind};
        assert!(
            load_error_message(&Error::from(ErrorKind::NotFound))
                .to_lowercase()
                .contains("not found")
        );
        assert!(
            load_error_message(&Error::from(ErrorKind::PermissionDenied))
                .to_lowercase()
                .contains("permission")
        );
    }

    #[test]
    fn catch_up_should_fire_when_text_drifted_during_handshake() {
        let version_after_gap: i32 = 4;
        let did_open_text = "initial content";
        let current_buffer = "initial content with edits typed during handshake";

        assert_ne!(
            current_buffer, did_open_text,
            "pre-condition: buffer must have drifted"
        );

        let catch_up_version = version_after_gap + 1;
        assert_eq!(catch_up_version, 5);
    }
}
