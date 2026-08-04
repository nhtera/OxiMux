//! `oximux term` — list the host's terminals and attach to one. Attach is the
//! relay client's discipline over the remote protocol: build at the replay's
//! dims, write replay bytes verbatim, then stream — and on a `TermGapped`
//! notice re-attach for a fresh replay rather than rendering a screen with a
//! hole in it. Ctrl+] detaches; every other byte (Ctrl+C included) belongs to
//! the remote terminal.

use std::io::Write as _;

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// The detach chord: Ctrl+] (0x1D), the classic escape byte no shell workflow
/// types routinely.
const DETACH: u8 = 0x1d;

pub async fn ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::ListTerminals).await? {
        Response::Terminals(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("ListTerminals", &other)),
    };
    let human = if rows.is_empty() {
        "no terminals".to_string()
    } else {
        rows.iter()
            .map(|t| format!("{}  {}x{}  {}", t.pty_id, t.cols, t.rows, t.cwd))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let data = json!(rows
        .iter()
        .map(|t| json!({ "pty_id": t.pty_id, "cwd": t.cwd, "cols": t.cols, "rows": t.rows }))
        .collect::<Vec<_>>());
    Ok((data, human))
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
            Response::Error(e) => return Err(rpc_failure(e)),
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
                    // The guard's Drop restores the terminal on this path too.
                    if let Response::Error(e) = reply {
                        return Err(rpc_failure(e));
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
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("TermAttach", &other)),
    }
}
