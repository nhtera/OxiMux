//! `oximux term` — list the host's terminals and attach to one. Attach is the
//! relay client's discipline over the remote protocol: build at the replay's
//! dims, write replay bytes verbatim, then stream — and on a `TermGapped`
//! notice re-attach for a fresh replay rather than rendering a screen with a
//! hole in it. Ctrl+] detaches; every other byte (Ctrl+C included) belongs to
//! the remote terminal.

use std::io::Write as _;

use oximux_remote_proto::proto::{Request, Response, RpcError};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The detach chord: Ctrl+] (0x1D), the classic escape byte no shell workflow
/// types routinely.
const DETACH: u8 = 0x1d;

/// Map a terminal RPC failure, naming the one cause a generic `Unsupported`
/// cannot: the host has no relay.
///
/// A host serves terminals only when it could start `oximux-relay`, and a
/// headless `oximux serve` that could not still serves everything else — so
/// this is the expected state on a server where the relay is missing from the
/// install, not an exotic one. `rpc_failure` renders `Unsupported` correctly
/// but with no follow-up, and "does not offer that capability" alone does not
/// tell an operator which binary to go find.
fn term_failure(err: RpcError) -> Failure {
    // The host names an unknown terminal with the session variant; say which.
    if matches!(err, RpcError::UnknownSession) {
        return Failure::new("unknown-terminal", exit::ERROR, "no such terminal on this host")
            .with_steps(["run `oximux term ls` to list terminals".into()]);
    }
    // Locked from the desktop app (it hosts this terminal). Denied, not
    // failed: retrying will not help until someone unlocks it there.
    if let RpcError::BadRequest(message) = &err
        && message == oximux_remote_proto::READ_ONLY_TERMINAL
    {
        return Failure::new("read-only", exit::DENIED, "this terminal is read-only")
            .with_steps(["unlock it in the desktop app: right-click the terminal → Allow Input".into()]);
    }
    if matches!(err, RpcError::Unsupported) {
        return Failure::new(
            "unsupported",
            exit::ERROR,
            "this host serves no terminals (it has no relay)",
        )
        .with_steps([
            "put `oximux-relay` next to the `oximux` binary — the installer does \
             this, and the release archive carries both"
                .into(),
            "or point the host at one with $OXIMUX_RELAY_BINARY, then restart it".into(),
            "the host logs the reason at startup (`relay unavailable`)".into(),
        ]);
    }
    rpc_failure(err)
}

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListTerminals).await? {
        Response::Terminals(rows) => rows,
        Response::Error(e) => return Err(term_failure(e)),
        other => return Err(unexpected_reply("ListTerminals", &other)),
    };
    let human = if rows.is_empty() {
        "no terminals".to_string()
    } else {
        rows.iter()
            .map(|t| format!("{}  {}x{}  {}", t.pty_id, t.cols, t.rows, crate::output::printable(&t.cwd)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let data = json!(rows
        .iter()
        .map(|t| json!({ "pty_id": t.pty_id, "cwd": t.cwd, "cols": t.cols, "rows": t.rows }))
        .collect::<Vec<_>>());
    Ok((data, human))
}

/// Refuse a `term send` that would type nothing or names an unknown key —
/// from the argv alone, before any host is involved.
pub fn check_send(has_text: bool, keys: &[String], enter: bool, app_cursor: bool) -> Result<(), Failure> {
    if !has_text && keys.is_empty() && !enter {
        return Err(nothing_to_send());
    }
    crate::key_encode::compose(b"", keys, enter, app_cursor)
        .map(drop)
        .map_err(|e| Failure::new("unknown-key", exit::USAGE, e))
}

fn nothing_to_send() -> Failure {
    Failure::new("nothing-to-send", exit::USAGE, "nothing to send")
        .with_steps(["give TEXT, --stdin, --keys, or --enter".into()])
}

/// Bytes per `TermInput`, well under the transport's frame limit, so piping a
/// large file through `--stdin` is several writes rather than a refused frame.
const SEND_CHUNK: usize = 64 * 1024;

/// The bytes `term send` types: the text (or stdin), the keys, then Enter.
/// Typing nothing at all (an empty TEXT or an empty stdin, no keys) is a
/// usage error, not a successful send of zero bytes.
pub fn input_bytes(
    text: Option<String>,
    stdin: bool,
    keys: &[String],
    enter: bool,
    app_cursor: bool,
) -> Result<Vec<u8>, Failure> {
    let text = if stdin {
        use std::io::Read as _;
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf).map_err(|e| {
            Failure::new("stdin", exit::ERROR, format!("could not read stdin: {e}"))
        })?;
        buf
    } else {
        text.unwrap_or_default().into_bytes()
    };
    let bytes = crate::key_encode::compose(&text, keys, enter, app_cursor)
        .map_err(|e| Failure::new("unknown-key", exit::USAGE, e))?;
    if bytes.is_empty() {
        return Err(nothing_to_send());
    }
    Ok(bytes)
}

/// Type `bytes` into a terminal — no attach, no screen — in `TermInput`s of
/// at most [`SEND_CHUNK`] bytes, in order.
pub async fn send(client: &Client, pty: &str, bytes: Vec<u8>) -> Result<(Value, String), Failure> {
    for chunk in bytes.chunks(SEND_CHUNK) {
        match client.call(Request::TermInput { pty_id: pty.into(), bytes: chunk.to_vec() }).await? {
            Response::Ack => {}
            Response::Error(e) => return Err(term_failure(e)),
            other => return Err(unexpected_reply("TermInput", &other)),
        }
    }
    let sent = bytes.len();
    Ok((json!({ "pty_id": pty, "bytes": sent }), format!("sent {sent} bytes to {pty}")))
}

/// Puts the local terminal in raw mode for the attach and guarantees restore
/// on every exit path — including a panic — via `Drop`.
struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self, Failure> {
        crossterm::terminal::enable_raw_mode().map_err(|e| {
            Failure::new("raw-mode", exit::ERROR, format!("cannot enter raw mode: {e}"))
        })?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Write remote terminal bytes to the local one verbatim, bypassing any line
/// discipline — the bytes were produced by a program drawing a grid.
fn write_raw(bytes: &[u8]) {
    let mut out = std::io::stdout();
    let _ = out.write_all(bytes);
    let _ = out.flush();
}

pub async fn attach(client: &Client, pty: &str) -> Result<(Value, String), Failure> {
    // Attach first — the replay must be drawn before anything is typed.
    let (replay, _cols, _rows) =
        match client.call(Request::TermAttach { pty_id: pty.into() }).await? {
            Response::TermAttached { replay, cols, rows } => (replay, cols, rows),
            Response::Error(e) => return Err(term_failure(e)),
            other => return Err(unexpected_reply("TermAttach", &other)),
        };
    // The notice goes to stderr before raw mode, so it never lands inside the
    // remote screen's byte stream on stdout.
    eprintln!("— attached to {pty}; Ctrl+] detaches —");

    let _raw = RawGuard::enable()?;
    write_raw(&replay);
    // Drive the shared PTY at this terminal's size (the daemon sizes to the
    // smallest attachment, exactly as the desktop's own attach does).
    let mut local_size = crossterm::terminal::size().unwrap_or((80, 24));
    let _ = client
        .call_streaming(
            Request::TermResize { pty_id: pty.into(), cols: local_size.0, rows: local_size.1 },
            |push| on_push(pty, push, &mut false),
        )
        .await;

    // Keystrokes come off a blocking reader thread: raw stdin has no async
    // story worth its complexity, and the thread dies with the process.
    let (tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut resize_tick = tokio::time::interval(std::time::Duration::from_millis(500));
    let mut exited: Option<Option<i32>> = None;
    let mut told_read_only = false;
    'attach: loop {
        tokio::select! {
            bytes = stdin_rx.recv() => {
                let Some(mut bytes) = bytes else { break 'attach };
                // The chord detaches; bytes typed before it still go through.
                let detach = bytes.iter().position(|b| *b == DETACH);
                if let Some(at) = detach {
                    bytes.truncate(at);
                }
                if !bytes.is_empty() {
                    let mut gapped = false;
                    let reply = client
                        .call_streaming(
                            Request::TermInput { pty_id: pty.into(), bytes },
                            |push| on_push(pty, push, &mut gapped),
                        )
                        .await?;
                    if gapped {
                        reattach(client, pty).await?;
                    }
                    // Locked on the desktop: keep watching, say so once. Raw
                    // mode, so the line ends with an explicit CR LF.
                    let read_only = matches!(&reply, Response::Error(RpcError::BadRequest(m))
                        if m == oximux_remote_proto::READ_ONLY_TERMINAL);
                    if read_only {
                        if !told_read_only {
                            told_read_only = true;
                            eprint!("\r\n— this terminal is read-only on the host; watching only —\r\n");
                        }
                    } else if let Response::Error(e) = reply {
                        // The guard's Drop restores the terminal on this path too.
                        return Err(term_failure(e));
                    }
                }
                if detach.is_some() {
                    break 'attach;
                }
            }
            frame = client.recv_frame() => {
                let frame = frame?;
                if let Response::TermExited { pty_id, code } = &frame
                    && pty_id == pty
                {
                    exited = Some(*code);
                    break 'attach;
                }
                let mut gapped = false;
                on_push(pty, frame, &mut gapped);
                if gapped {
                    reattach(client, pty).await?;
                }
            }
            _ = resize_tick.tick() => {
                let now = crossterm::terminal::size().unwrap_or(local_size);
                if now != local_size {
                    local_size = now;
                    let mut gapped = false;
                    let _ = client
                        .call_streaming(
                            Request::TermResize { pty_id: pty.into(), cols: now.0, rows: now.1 },
                            |push| on_push(pty, push, &mut gapped),
                        )
                        .await?;
                    if gapped {
                        reattach(client, pty).await?;
                    }
                }
            }
        }
    }
    drop(_raw);
    // Leave the cursor on a fresh line — the remote screen ended mid-row.
    eprintln!();
    match exited {
        Some(code) => {
            let code_text = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            Ok((
                json!({ "pty_id": pty, "exited": true, "code": code }),
                format!("terminal exited (code {code_text})"),
            ))
        }
        None => Ok((
            json!({ "pty_id": pty, "detached": true }),
            format!("detached from {pty} — the terminal keeps running"),
        )),
    }
}

/// Handle one pushed frame during attach: output is drawn, a gap notice is
/// recorded for the caller to re-attach on (the documented recovery — a
/// client that keeps rendering after a gap is drawing a screen with a hole).
fn on_push(pty: &str, frame: Response, gapped: &mut bool) {
    match frame {
        Response::TermOutput { pty_id, bytes } if pty_id == pty => write_raw(&bytes),
        Response::TermGapped { pty_id } if pty_id == pty => *gapped = true,
        _ => {}
    }
}

/// Re-attach after a gap: a fresh replay snapshot replaces the holed screen.
async fn reattach(client: &Client, pty: &str) -> Result<(), Failure> {
    match client.call(Request::TermAttach { pty_id: pty.into() }).await? {
        Response::TermAttached { replay, .. } => {
            write_raw(&replay);
            Ok(())
        }
        Response::Error(e) => Err(term_failure(e)),
        other => Err(unexpected_reply("TermAttach", &other)),
    }
}
