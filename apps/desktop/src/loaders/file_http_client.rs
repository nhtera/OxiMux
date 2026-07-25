//! `FileHttpClient` — a minimal `HttpClient` that serves `file://` URIs from
//! the local filesystem and rejects everything else.
//!
//! GPUI installs `NullHttpClient` by default (fails every request), and its
//! image element loads any `Resource::Uri` *through the http client*, never the
//! filesystem (`img.rs` hands the URL to `HttpClient::get`). The markdown
//! renderer builds image elements from the image URL verbatim, so a repo-local
//! image — which the editor rewrites to a `file://` URI before rendering — can
//! only display if the installed http client knows how to read `file://`.
//!
//! We override `get` (not just `send`): the trait's default `get` builds the
//! request via `http::request::Builder`, whose `Uri` parser rejects a
//! `file:///path` URI (empty authority → "invalid format") before `send` is
//! ever reached. Overriding `get` lets us read the file directly and skip that
//! parse. This client is installed once at startup via `cx.set_http_client`.
//! It is deliberately scoped to `file://`; remote images are out of scope.

use std::sync::Arc;

use anyhow::Context as _;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::http_client::{
    AsyncBody, HttpClient, Url,
    http::{HeaderValue, Request, Response, StatusCode},
};

/// Serves `file://` URIs from disk; bails on any other scheme.
pub struct FileHttpClient;

impl FileHttpClient {
    /// Boxed instance ready for `cx.set_http_client(...)`.
    pub fn shared() -> Arc<dyn HttpClient> {
        Arc::new(Self)
    }
}

/// Read the file a `file://` URI points at and wrap it in a 200 response.
/// Parsing through `url` mirrors how the editor encodes these URIs
/// (`Url::from_file_path`) and handles percent-decoding (`%20`, etc.).
fn serve_file(uri: &str) -> anyhow::Result<Response<AsyncBody>> {
    let url = Url::parse(uri).with_context(|| format!("parse URI {uri}"))?;
    if url.scheme() != "file" {
        anyhow::bail!("FileHttpClient serves only file:// URIs (got {uri})");
    }
    let path = url
        .to_file_path()
        .map_err(|()| anyhow::anyhow!("file URI is not a local path: {uri}"))?;
    let bytes = std::fs::read(&path).with_context(|| format!("read image {}", path.display()))?;
    Response::builder()
        .status(StatusCode::OK)
        .body(AsyncBody::from(bytes))
        .map_err(Into::into)
}

impl HttpClient for FileHttpClient {
    fn get(
        &self,
        uri: &str,
        _body: AsyncBody,
        _follow_redirects: bool,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        // Defer the (blocking) read into the future so it runs on the caller's
        // executor, not on whichever thread constructs the future.
        let uri = uri.to_owned();
        async move { serve_file(&uri) }.boxed()
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        // A `file:///path` URI can't survive `http::Uri` parsing, so anything
        // arriving here is non-file and unserviceable — but route it through
        // the same helper for consistency (it will bail on the scheme check).
        let uri = req.uri().to_string();
        async move { serve_file(&uri) }.boxed()
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }
}
