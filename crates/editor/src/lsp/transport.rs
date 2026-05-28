//! LSP base-protocol framing — `Content-Length: N\r\n\r\n` followed by an
//! N-byte UTF-8 JSON body.
//!
//! Hand-rolled on purpose: the protocol is small enough that a
//! tower-/async-lsp shim costs more than it saves, especially given the
//! `lsp-types 0.97` pin we share with `gpui-component`.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Encode a JSON value into a framed LSP message ready to be written to
/// the child's stdin. Adds the `Content-Length` header + blank line.
pub fn encode_frame(body: &serde_json::Value) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(body).context("serialize lsp body")?;
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Convenience writer — encode then flush.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &serde_json::Value,
) -> Result<()> {
    let bytes = encode_frame(body)?;
    writer.write_all(&bytes).await.context("write lsp frame")?;
    writer.flush().await.context("flush lsp frame")?;
    Ok(())
}

/// Read one framed message from the child's stdout. The reader is wrapped
/// in `BufReader` by the caller so we can do line-at-a-time header parsing
/// without burning a heap allocation per byte.
///
/// Returns `Ok(None)` only on clean EOF — that's the signal the daemon
/// shut down and the dispatch loop should exit gracefully.
pub async fn read_frame<R: AsyncReadExt + AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<serde_json::Value>> {
    let headers = match read_headers(reader).await? {
        Some(h) => h,
        None => return Ok(None),
    };
    let content_length = headers
        .get("content-length")
        .ok_or_else(|| anyhow!("lsp frame missing Content-Length header"))?
        .parse::<usize>()
        .context("parse Content-Length value")?;
    let mut buf = vec![0u8; content_length];
    reader.read_exact(&mut buf).await.context("read lsp body")?;
    let value: serde_json::Value =
        serde_json::from_slice(&buf).context("parse lsp body as JSON")?;
    Ok(Some(value))
}

async fn read_headers<R: AsyncReadExt + AsyncBufReadExt + Unpin>(
    reader: &mut R,
) -> Result<Option<HashMap<String, String>>> {
    let mut headers = HashMap::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("read header line")?;
        if n == 0 {
            // EOF before we saw a complete header block.
            return if headers.is_empty() {
                Ok(None)
            } else {
                bail!("lsp child closed stdin mid-header block");
            };
        }
        // Stop at the blank line that terminates the header block. LSP
        // mandates CRLF but be liberal with what we accept.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return Ok(Some(headers));
        }
        let (name, value) = trimmed
            .split_once(':')
            .ok_or_else(|| anyhow!("malformed lsp header: {trimmed:?}"))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }
}

/// Convenience wrapper: build a `BufReader` around any `AsyncRead`. The
/// `LspClient` reader task pairs this with `read_frame` in a `loop`.
pub fn buffered<R: AsyncReadExt + Unpin>(reader: R) -> BufReader<R> {
    BufReader::new(reader)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn encode_and_decode_roundtrip() {
        let body = json!({ "jsonrpc": "2.0", "method": "initialized" });
        let bytes = encode_frame(&body).unwrap();
        let mut reader = BufReader::new(bytes.as_slice());
        let decoded = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, body);
    }

    #[tokio::test]
    async fn decodes_unicode_body() {
        let body = json!({ "message": "héllo — 🌍" });
        let bytes = encode_frame(&body).unwrap();
        let mut reader = BufReader::new(bytes.as_slice());
        let decoded = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(decoded, body);
    }

    #[tokio::test]
    async fn decodes_multi_frame_stream() {
        let a = json!({ "id": 1 });
        let b = json!({ "id": 2 });
        let mut buf = encode_frame(&a).unwrap();
        buf.extend(encode_frame(&b).unwrap());
        let mut reader = BufReader::new(buf.as_slice());
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), a);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let buf: Vec<u8> = vec![];
        let mut reader = BufReader::new(buf.as_slice());
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn malformed_header_errors() {
        let raw = b"not-a-header\r\n\r\n{}".to_vec();
        let mut reader = BufReader::new(raw.as_slice());
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn missing_content_length_errors() {
        let raw = b"X-Other: 1\r\n\r\n{}".to_vec();
        let mut reader = BufReader::new(raw.as_slice());
        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn eof_mid_header_block_errors() {
        // Header parser saw at least one valid line but the child closed
        // its stdout before the blank-line terminator — covers the
        // `bail!("lsp child closed stdin mid-header block")` branch that
        // the rest of the EOF/error matrix missed (tester-260522-0240).
        let raw = b"Content-Length: 5\r\n".to_vec();
        let mut reader = BufReader::new(raw.as_slice());
        assert!(read_frame(&mut reader).await.is_err());
    }
}
