//! `LspClient` — async JSON-RPC client for one language server child.
//!
//! Responsibilities:
//!   - Spawn the server binary as a tokio child process with
//!     `kill_on_drop(true)`.
//!   - Drive the `initialize` -> `initialized` handshake.
//!   - Pump outgoing frames via an unbounded mpsc to a writer task;
//!     incoming frames via a reader task that dispatches:
//!       - responses by id -> oneshot reply
//!       - `textDocument/publishDiagnostics` -> tokio broadcast channel
//!       - everything else -> trace log + drop
//!   - Expose typed helpers: `did_open`, `did_change`, `did_save`,
//!     `did_close`, `hover`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow, bail};
use lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, Hover, HoverParams, InitializeParams,
    InitializeResult, InitializedParams, Position, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, notification::Notification as _,
};
use serde::Serialize;
use serde_json::json;
use tokio::{
    io::AsyncBufReadExt,
    process::{Child, Command},
    runtime::Handle,
    sync::{Mutex, broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::{Duration, timeout},
};

use super::transport::{buffered, encode_frame, read_frame};

/// 5-second cap on every awaited LSP request. Early LSP editors
/// skipped `$/cancelRequest` and relied on the timeout instead — see
/// `phase-05-step-01-editor-lsp-spike-plan.md` OQ3.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on the `initialize` handshake. rust-analyzer typically replies in
/// <300 ms on a warm machine but the first cold-cache run can stretch.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<serde_json::Value>>>>;

/// Handle to a running language server. Drop = kill the child + cancel
/// pumps (`kill_on_drop` + JoinHandle drop).
///
/// **Field drop order matters.** Rust drops fields top-to-bottom: `sender`
/// is dropped first (closes the mpsc; writer task sees `None` and exits),
/// then the JoinHandles cancel their tasks, then `_child` is reaped via
/// `kill_on_drop(true)`. Don't reorder without re-thinking the broken-
/// pipe window (M1, code-review 260522-0240).
pub struct LspClient {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    pending: PendingMap,
    diag_tx: broadcast::Sender<PublishDiagnosticsParams>,
    next_id: AtomicI64,
    /// Tokio runtime handle captured at spawn time. `LspClient::spawn`
    /// is called from the gpui main thread where `rt.enter()` is alive
    /// (see `crates/app/src/main.rs:51`). The handle is `Send + Clone`
    /// so `LspHoverProvider` can hop futures back onto tokio's worker
    /// pool from gpui's GCD-backed background executor — without this,
    /// `tokio::time::timeout` panics with "no reactor running" the
    /// moment the future polls on a libdispatch thread (C1 fix,
    /// code-review 260522-0240).
    tokio_handle: Handle,
    /// Held only for its `Drop = kill` side effect; the writer/reader
    /// tasks talk to the child via the captured stdin/stdout.
    _writer_task: JoinHandle<()>,
    _reader_task: JoinHandle<()>,
    _stderr_task: JoinHandle<()>,
    _child: Child,
}

impl LspClient {
    /// Spawn `program` (e.g. `rust-analyzer`) as a stdio LSP server,
    /// perform the `initialize` -> `initialized` handshake, and return a
    /// ready client. `root` becomes the workspace root (`rootUri`).
    pub async fn spawn(program: &str, root: &Path) -> Result<Self> {
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn `{program}` (is it on PATH?)"))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("child stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("child stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow!("child stderr"))?;

        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (diag_tx, _) = broadcast::channel(64);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        let writer_task = spawn_writer(stdin, outgoing_rx);
        // Reader gets a clone of `outgoing_tx` so it can ship server-
        // initiated request responses back without going through the
        // public `notify`/`request` API (H1 fix — code-review 260522-0240).
        let reader_task = spawn_reader(
            stdout,
            pending.clone(),
            diag_tx.clone(),
            outgoing_tx.clone(),
        );
        let stderr_task = spawn_stderr_log(stderr, program.to_string());

        let tokio_handle =
            Handle::try_current().context("LspClient::spawn requires a live tokio runtime")?;

        let client = Self {
            sender: outgoing_tx,
            pending,
            diag_tx,
            next_id: AtomicI64::new(0),
            tokio_handle,
            _writer_task: writer_task,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: child,
        };

        timeout(INITIALIZE_TIMEOUT, client.initialize(root))
            .await
            .context("lsp initialize handshake timed out")?
            .context("lsp initialize handshake failed")?;

        Ok(client)
    }

    /// Build the `InitializeParams`, send the request, ship the
    /// `initialized` notification once the response lands.
    async fn initialize(&self, root: &Path) -> Result<InitializeResult> {
        let root_uri = path_to_file_uri(root)?;
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            #[allow(deprecated)]
            root_uri: Some(root_uri),
            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities::default()),
                ..Default::default()
            },
            initialization_options: None,
            trace: None,
            workspace_folders: None,
            client_info: Some(lsp_types::ClientInfo {
                name: "oximux".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            locale: None,
            ..Default::default()
        };
        let result: InitializeResult = self
            .request::<lsp_types::request::Initialize>(params)
            .await?;
        self.notify::<lsp_types::notification::Initialized>(InitializedParams {})?;
        Ok(result)
    }

    /// Issue a `textDocument/didOpen` notification. Carries the initial
    /// buffer text at `version` 1; each subsequent change increments the
    /// version via `did_change`.
    pub fn did_open(&self, uri: Uri, language_id: &str, version: i32, text: String) -> Result<()> {
        self.notify::<lsp_types::notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: language_id.into(),
                version,
                text,
            },
        })
    }

    /// Issue a `textDocument/didChange` notification using full-document
    /// sync (Language Server Protocol §3.17.2: `range: None` = full
    /// replacement). `version` must be strictly monotonic per open document.
    /// Sync is full-document; incremental sync is deferred to a later step.
    ///
    /// Accepts a pre-parsed `&Uri` (cached on `EditorView`) so the caller
    /// avoids re-encoding the path on every keystroke (H1 fix).
    pub fn did_change(&self, uri: &Uri, version: i32, text: String) -> Result<()> {
        self.notify::<lsp_types::notification::DidChangeTextDocument>(
            DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text,
                }],
            },
        )
    }

    /// Issue a `textDocument/didSave` notification. Sends no text payload —
    /// the server uses the last-received `didChange` text. Call only after
    /// the file has been written to disk successfully.
    ///
    /// Accepts a pre-parsed `&Uri` (cached on `EditorView`) to avoid
    /// redundant parse/allocation (H1 fix — consistent with `did_change`).
    pub fn did_save(&self, uri: &Uri) -> Result<()> {
        self.notify::<lsp_types::notification::DidSaveTextDocument>(
            DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                text: None,
            },
        )
    }

    /// Issue a `textDocument/didClose` notification. After this the server
    /// stops tracking the document. Call when the buffer is torn down.
    ///
    /// Accepts a pre-parsed `&Uri` (cached on `EditorView`) to avoid
    /// redundant parse/allocation (H1 fix — consistent with `did_change`).
    pub fn did_close(&self, uri: &Uri) -> Result<()> {
        self.notify::<lsp_types::notification::DidCloseTextDocument>(
            DidCloseTextDocumentParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
            },
        )
    }

    /// Request hover info at `position` for the file at `uri`. The
    /// server returns either `Some(Hover)` or `None` (no info there).
    pub async fn hover(&self, uri: Uri, position: Position) -> Result<Option<Hover>> {
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let res = timeout(
            REQUEST_TIMEOUT,
            self.request::<lsp_types::request::HoverRequest>(params),
        )
        .await
        .context("hover request timed out")??;
        Ok(res)
    }

    /// Subscribe to `publishDiagnostics` broadcasts. Each subscriber sees
    /// every diagnostic after they call `subscribe`; the channel uses
    /// `broadcast::channel(64)` so a slow consumer is logged + lagged
    /// rather than blocking the reader task.
    pub fn subscribe_diagnostics(&self) -> broadcast::Receiver<PublishDiagnosticsParams> {
        self.diag_tx.subscribe()
    }

    /// Tokio runtime handle captured at spawn. `LspHoverProvider` hands
    /// its futures to this handle so `tokio::time::timeout` runs on a
    /// tokio worker (with the reactor) instead of a libdispatch thread.
    pub fn tokio_handle(&self) -> Handle {
        self.tokio_handle.clone()
    }

    fn next_request_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Generic typed request. Send the JSON-RPC body, install a oneshot
    /// in `pending`, await the matching response, deserialize into the
    /// LSP type registered with `lsp_types::request::Request`.
    async fn request<R>(&self, params: R::Params) -> Result<R::Result>
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
        R::Result: for<'de> serde::Deserialize<'de>,
    {
        let id = self.next_request_id();
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": R::METHOD,
            "params": params,
        });
        let frame = encode_frame(&body)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }
        self.sender
            .send(frame)
            .map_err(|_| anyhow!("lsp writer task closed"))?;

        let raw = rx
            .await
            .context("lsp response channel dropped (server gone?)")?;
        if let Some(err) = raw.get("error") {
            bail!("lsp request {} failed: {err}", R::METHOD);
        }
        let result_value = raw
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(result_value)
            .with_context(|| format!("deserialize {} result", R::METHOD))
    }

    /// Send a one-way notification. No id, no response expected.
    fn notify<N>(&self, params: N::Params) -> Result<()>
    where
        N: lsp_types::notification::Notification,
        N::Params: Serialize,
    {
        let body = json!({
            "jsonrpc": "2.0",
            "method": N::METHOD,
            "params": params,
        });
        let frame = encode_frame(&body)?;
        self.sender
            .send(frame)
            .map_err(|_| anyhow!("lsp writer task closed"))
    }
}

/// Convert an absolute filesystem path to a `file://` Uri the LSP server
/// will accept. Canonicalization keeps rust-analyzer happy on symlinked
/// project roots (a common cause of "no information" hovers in v0.9).
///
/// Percent-encoding goes through `url::Url::from_file_path`: paths with
/// spaces, `#`, `%`, or non-ASCII characters round-trip cleanly against
/// rust-analyzer's response URIs. Naive `format!("file://{}", display)`
/// drops the encoding and silently fails the URI equality check in the
/// diagnostics pump (H2 fix, code-review 260522-0240).
pub fn path_to_file_uri(path: &Path) -> Result<Uri> {
    let abs: PathBuf = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("resolve cwd for file uri")?
            .join(path)
    };
    // best-effort canonicalize: fall back to the absolute path if the
    // target doesn't exist (allowed for the spike's hard-coded path).
    let final_path = std::fs::canonicalize(&abs).unwrap_or(abs);
    let url = url::Url::from_file_path(&final_path)
        .map_err(|_| anyhow!("cannot encode path as file URL: {}", final_path.display()))?;
    Uri::from_str(url.as_str()).with_context(|| format!("parse file uri {url}"))
}

fn spawn_writer(
    stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(frame) = rx.recv().await {
            use tokio::io::AsyncWriteExt;
            if let Err(err) = stdin.write_all(&frame).await {
                tracing::warn!(?err, "lsp writer: stdin closed");
                break;
            }
            if let Err(err) = stdin.flush().await {
                tracing::warn!(?err, "lsp writer: flush failed");
                break;
            }
        }
    })
}

fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    diag_tx: broadcast::Sender<PublishDiagnosticsParams>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = buffered(stdout);
        loop {
            match read_frame(&mut reader).await {
                Ok(Some(frame)) => dispatch_frame(frame, &pending, &diag_tx, &outgoing).await,
                Ok(None) => {
                    tracing::info!("lsp reader: server closed stdout");
                    break;
                }
                Err(err) => {
                    tracing::warn!(?err, "lsp reader: frame decode failed");
                    break;
                }
            }
        }
    })
}

async fn dispatch_frame(
    frame: serde_json::Value,
    pending: &PendingMap,
    diag_tx: &broadcast::Sender<PublishDiagnosticsParams>,
    outgoing: &mpsc::UnboundedSender<Vec<u8>>,
) {
    if let Some(id) = frame.get("id").and_then(|v| v.as_i64()) {
        // Response (has id + result/error) or server-initiated request
        // (has id + method). rust-analyzer issues `client/registerCapability`
        // + `workspace/configuration` during init; the spec requires the
        // client to respond and some server versions block waiting on
        // the response (H1 fix, code-review 260522-0240). Replying with
        // a null result unblocks them — the spike doesn't actually
        // honour the capability registration, but rust-analyzer treats
        // a null reply as "client acknowledges, no action".
        if frame.get("method").is_some() {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": serde_json::Value::Null,
            });
            if let Ok(bytes) = encode_frame(&response)
                && let Err(err) = outgoing.send(bytes)
            {
                tracing::warn!(?err, id, "lsp: failed to reply to server request");
            }
            return;
        }
        let mut pending = pending.lock().await;
        if let Some(tx) = pending.remove(&id) {
            let _ = tx.send(frame);
        } else {
            tracing::warn!(id, "lsp response without matching pending request");
        }
        return;
    }

    // Notification — no id.
    let Some(method) = frame.get("method").and_then(|v| v.as_str()) else {
        tracing::warn!(?frame, "lsp frame missing id and method");
        return;
    };
    if method == lsp_types::notification::PublishDiagnostics::METHOD {
        match serde_json::from_value::<PublishDiagnosticsParams>(
            frame
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ) {
            Ok(params) => {
                let _ = diag_tx.send(params);
            }
            Err(err) => tracing::warn!(?err, "lsp publishDiagnostics decode failed"),
        }
    } else {
        tracing::trace!(method, "lsp: notification dropped (out of scope)");
    }
}

fn spawn_stderr_log(stderr: tokio::process::ChildStderr, label: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if !trimmed.is_empty() {
                        tracing::debug!(target: "lsp.stderr", server = %label, "{trimmed}");
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, "lsp stderr reader failed");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_file_uri_absolute_rust_file() {
        let uri = path_to_file_uri(Path::new("/tmp/foo.rs")).unwrap();
        let s = uri.as_str();
        assert!(s.starts_with("file:///"), "got {s:?}");
        assert!(s.ends_with("foo.rs"), "got {s:?}");
    }

    #[test]
    fn path_to_file_uri_relative_resolves_against_cwd() {
        let uri = path_to_file_uri(Path::new("some-non-existent-file.rs")).unwrap();
        assert!(
            uri.as_str().starts_with("file:///"),
            "uri={:?}",
            uri.as_str()
        );
    }

    #[test]
    fn path_to_file_uri_percent_encodes_spaces() {
        // macOS `~/Library/Application Support/...` has a space — without
        // percent-encoding the URI doesn't round-trip against the server's
        // response URI and `params.uri != uri` silently drops diagnostics
        // (H2 / H3, code-review 260522-0240).
        let uri = path_to_file_uri(Path::new("/tmp/has space/foo.rs")).unwrap();
        let s = uri.as_str();
        assert!(
            s.contains("%20"),
            "spaces must be percent-encoded; got {s:?}"
        );
        assert!(!s.contains(' '), "raw space leaked into URI: {s:?}");
    }

}
// LSP notification serialization tests live in
// `crates/editor/tests/lsp_notification_serialization.rs` to keep this
// file under 500 LOC (xtask file-size-lint warn threshold).
