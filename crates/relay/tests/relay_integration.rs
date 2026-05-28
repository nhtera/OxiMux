// End-to-end: boot the relay in-process, drive it from a raw
// UnixStream client, and verify the survival/replay contract that is
// the entire reason this daemon exists.

use std::path::PathBuf;
use std::time::Duration;

use oximux_relay::codec::{read_frame, write_frame};
use oximux_relay::server::{ServerConfig, run_server};
use oximux_relay_proto::{Frame, Hello, Notification, PROTOCOL_VERSION, Request, Response};
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::time::timeout;

struct TestRelay {
    socket: PathBuf,
    token: String,
    _dir: TempDir,
    _server_task: tokio::task::JoinHandle<()>,
}

async fn boot_relay() -> TestRelay {
    let dir = TempDir::new().expect("tempdir");
    let socket = dir.path().join("relay-v1.sock");
    let token_file = dir.path().join("relay-v1.token");
    let token = "deadbeef-test-token".to_string();
    std::fs::write(&token_file, &token).expect("write token");

    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    let server_task = tokio::spawn(async move {
        let _ = run_server(cfg).await;
    });
    // Spin until the socket file appears + accepts a connection.
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            return TestRelay {
                socket,
                token,
                _dir: dir,
                _server_task: server_task,
            };
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("relay socket never came up");
}

async fn connect_and_hello(relay: &TestRelay) -> (UnixStream, Vec<u8>) {
    let mut stream = UnixStream::connect(&relay.socket).await.expect("connect");
    let hello = Frame::Request {
        request_id: 1,
        request: Request::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            token: relay.token.clone(),
            client_id: "test-client".into(),
        }),
    };
    write_frame(&mut stream, &hello).await.expect("hello write");
    let mut buf = Vec::new();
    let ack = read_frame(&mut stream, &mut buf).await.expect("hello ack");
    let Frame::Response {
        response: Response::HelloAck(_),
        ..
    } = ack
    else {
        panic!("expected HelloAck, got {ack:?}");
    };
    (stream, buf)
}

async fn req(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    request_id: u64,
    request: Request,
) -> Response {
    let frame = Frame::Request {
        request_id,
        request,
    };
    write_frame(stream, &frame).await.expect("write request");
    // Skip any Notification frames that arrive before the matching Response.
    loop {
        let f = read_frame(stream, buf).await.expect("read response");
        match f {
            Frame::Response {
                request_id: rid,
                response,
            } if rid == request_id => return response,
            Frame::Notification(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn collect_output(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    pty_id: &str,
    overall: Duration,
) -> (Vec<u8>, Option<Option<i32>>) {
    let deadline = tokio::time::Instant::now() + overall;
    let mut out = Vec::new();
    let mut exit: Option<Option<i32>> = None;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let f = match timeout(remaining, read_frame(stream, buf)).await {
            Ok(Ok(f)) => f,
            _ => break,
        };
        match f {
            Frame::Notification(Notification::Output { pty_id: id, bytes }) if id == pty_id => {
                out.extend_from_slice(&bytes);
            }
            Frame::Notification(Notification::Exit { pty_id: id, code }) if id == pty_id => {
                exit = Some(code);
                break;
            }
            _ => {}
        }
    }
    (out, exit)
}

#[tokio::test]
async fn hello_handshake_then_echo_command() {
    let relay = boot_relay().await;
    let (mut stream, mut buf) = connect_and_hello(&relay).await;
    let resp = req(
        &mut stream,
        &mut buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await;
    let pty_id = match resp {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("spawn failed: {other:?}"),
    };
    // sh started without a command echoes nothing until we feed input.
    // Push `echo hi; exit\n` and read until Exit notification.
    let resp = req(
        &mut stream,
        &mut buf,
        3,
        Request::Write {
            pty_id: pty_id.clone(),
            bytes: b"echo hi\nexit\n".to_vec(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok), "write got {resp:?}");

    let (out, exit) = collect_output(&mut stream, &mut buf, &pty_id, Duration::from_secs(5)).await;
    assert!(
        String::from_utf8_lossy(&out).contains("hi"),
        "expected 'hi' in output; got {:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(exit.is_some(), "expected Exit notification");
}

#[tokio::test]
async fn attach_replays_buffered_output_then_streams_live() {
    let relay = boot_relay().await;

    // Client A: spawn `yes` so the buffer fills steadily.
    let (mut a, mut a_buf) = connect_and_hello(&relay).await;
    let pty_id = match req(
        &mut a,
        &mut a_buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await
    {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("spawn: {other:?}"),
    };
    let resp = req(
        &mut a,
        &mut a_buf,
        3,
        Request::Write {
            pty_id: pty_id.clone(),
            bytes: b"echo ALPHA_MARKER_A\n".to_vec(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));
    // Give the reader thread a moment to push that line into the ring.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drop client A — but the PTY (and the buffered bytes) outlives it.
    drop(a);

    // Client B: attach by id; replay must include ALPHA_MARKER_A.
    let (mut b, mut b_buf) = connect_and_hello(&relay).await;
    let resp = req(
        &mut b,
        &mut b_buf,
        2,
        Request::Attach {
            pty_id: pty_id.clone(),
        },
    )
    .await;
    let replay = match resp {
        Response::AttachOk { replay, cols, rows } => {
            // Attach must echo the PTY's live grid dims so a reattaching
            // client rebuilds its emulator at the exact captured size
            // before replaying — replaying into a mismatched grid (then
            // reflowing on the first pane resize) is what scrambled
            // restored full-screen TUIs. The PTY was spawned at 80x24
            // and never resized, so those dims must come back here.
            assert_eq!(
                (cols, rows),
                (80, 24),
                "attach must report the PTY's current grid size"
            );
            replay
        }
        other => panic!("attach: {other:?}"),
    };
    assert!(
        String::from_utf8_lossy(&replay).contains("ALPHA_MARKER_A"),
        "replay missed the marker; got {:?}",
        String::from_utf8_lossy(&replay)
    );

    // Live stream: write a second marker, observe it on client B.
    let resp = req(
        &mut b,
        &mut b_buf,
        3,
        Request::Write {
            pty_id: pty_id.clone(),
            bytes: b"echo BETA_MARKER_B\nexit\n".to_vec(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));
    let (out, exit) = collect_output(&mut b, &mut b_buf, &pty_id, Duration::from_secs(5)).await;
    assert!(
        String::from_utf8_lossy(&out).contains("BETA_MARKER_B"),
        "live stream missed marker; got {:?}",
        String::from_utf8_lossy(&out)
    );
    assert!(exit.is_some(), "expected Exit");
}

#[tokio::test]
async fn notify_fans_out_attention_to_subscribers() {
    // `oximux notify` → Request::Notify → the daemon fans a
    // Notification::Attention to every subscriber of that PTY. The
    // spawning session is auto-attached, so client A is a subscriber.
    let relay = boot_relay().await;
    let (mut a, mut a_buf) = connect_and_hello(&relay).await;
    let pty_id = match req(
        &mut a,
        &mut a_buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await
    {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("spawn: {other:?}"),
    };

    // Send Notify directly (not via `req`) so we can observe BOTH the Ok
    // response and the Attention notification regardless of wire order —
    // `req` would discard the notification while scanning for the response.
    write_frame(
        &mut a,
        &Frame::Request {
            request_id: 3,
            request: Request::Notify {
                pty_id: pty_id.clone(),
                title: "Claude".into(),
                body: "needs you".into(),
            },
        },
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut got_ok = false;
    let mut got_attention = false;
    while (!got_ok || !got_attention) && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), read_frame(&mut a, &mut a_buf)).await
        {
            Ok(Ok(Frame::Response {
                request_id: 3,
                response: Response::Ok,
            })) => got_ok = true,
            Ok(Ok(Frame::Notification(Notification::Attention {
                pty_id: p,
                title,
                body,
            }))) => {
                assert_eq!(p, pty_id, "attention for wrong pty");
                assert_eq!(title, "Claude");
                assert_eq!(body, "needs you");
                got_attention = true;
            }
            // Skip the shell's startup Output / anything else.
            _ => {}
        }
    }
    assert!(got_ok, "Notify did not return Ok");
    assert!(got_attention, "subscriber never received Attention fan-out");
}

#[tokio::test]
async fn bad_token_rejected_with_auth_failed() {
    let relay = boot_relay().await;
    let mut stream = UnixStream::connect(&relay.socket).await.unwrap();
    let hello = Frame::Request {
        request_id: 1,
        request: Request::Hello(Hello {
            protocol_version: PROTOCOL_VERSION,
            token: "wrong".into(),
            client_id: "x".into(),
        }),
    };
    write_frame(&mut stream, &hello).await.unwrap();
    let mut buf = Vec::new();
    let f = read_frame(&mut stream, &mut buf).await.unwrap();
    match f {
        Frame::Response {
            response:
                Response::Err {
                    code: oximux_relay_proto::ErrCode::AuthFailed,
                    ..
                },
            ..
        } => {}
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn version_mismatch_is_rejected() {
    // Plan's phase-07 "Tests" item: connecting with a future
    // protocol_version must produce ErrCode::VersionMismatch and the
    // daemon must close the connection (we observe that by EOF on the
    // next frame attempt).
    let relay = boot_relay().await;
    let mut stream = UnixStream::connect(&relay.socket).await.unwrap();
    let hello = Frame::Request {
        request_id: 1,
        request: Request::Hello(Hello {
            protocol_version: PROTOCOL_VERSION + 999,
            token: relay.token.clone(),
            client_id: "x".into(),
        }),
    };
    write_frame(&mut stream, &hello).await.unwrap();
    let mut buf = Vec::new();
    let f = read_frame(&mut stream, &mut buf).await.unwrap();
    match f {
        Frame::Response {
            response:
                Response::Err {
                    code: oximux_relay_proto::ErrCode::VersionMismatch,
                    ..
                },
            ..
        } => {}
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn shutdown_request_breaks_accept_loop_when_no_ptys_alive() {
    // With no PTYs alive, Request::Shutdown must drain run_server. We
    // observe that by joining the spawned server task with a timeout
    // — without the wired notify, the loop would block forever.
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("relay-v1.sock");
    let token_file = dir.path().join("relay-v1.token");
    let token = "deadbeef-test-token".to_string();
    std::fs::write(&token_file, &token).unwrap();
    let cfg = ServerConfig::idle_disabled(socket.clone(), token_file);
    let handle = tokio::spawn(async move { run_server(cfg).await });

    // Wait for readiness.
    for _ in 0..100 {
        if UnixStream::connect(&socket).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let relay = TestRelay {
        socket,
        token,
        _dir: dir,
        _server_task: tokio::spawn(async {}),
    };
    let (mut s, mut buf) = connect_and_hello(&relay).await;
    let resp = req(&mut s, &mut buf, 2, Request::Shutdown).await;
    assert!(matches!(resp, Response::Ok), "shutdown got {resp:?}");
    drop(s);

    let joined = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("server did not exit after Shutdown");
    assert!(joined.is_ok(), "server task panicked: {joined:?}");
}

#[tokio::test]
async fn shutdown_refused_while_ptys_alive() {
    let relay = boot_relay().await;
    let (mut s, mut buf) = connect_and_hello(&relay).await;
    let _ = match req(
        &mut s,
        &mut buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await
    {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("{other:?}"),
    };
    let resp = req(&mut s, &mut buf, 3, Request::Shutdown).await;
    match resp {
        Response::Err {
            code: oximux_relay_proto::ErrCode::Internal,
            ..
        } => {}
        other => panic!("expected Internal err refusing shutdown, got {other:?}"),
    }
}

#[tokio::test]
async fn stats_endpoint_returns_per_pty_counters() {
    let relay = boot_relay().await;
    let (mut s, mut buf) = connect_and_hello(&relay).await;
    let pty_id = match req(
        &mut s,
        &mut buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await
    {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("{other:?}"),
    };
    let written = b"echo STATS_PROBE\n";
    let resp = req(
        &mut s,
        &mut buf,
        3,
        Request::Write {
            pty_id: pty_id.clone(),
            bytes: written.to_vec(),
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));
    // Let the reader thread push the echoed bytes into bytes_out.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = req(&mut s, &mut buf, 4, Request::Stats).await;
    let stats = match resp {
        Response::StatsOk(s) => s,
        other => panic!("expected StatsOk, got {other:?}"),
    };
    let mine = stats
        .iter()
        .find(|s| s.pty_id == pty_id)
        .expect("stats missing the spawned pty");
    assert_eq!(mine.bytes_in, written.len() as u64);
    assert!(
        mine.bytes_out >= written.len() as u64,
        "bytes_out = {}",
        mine.bytes_out
    );
}

#[tokio::test]
async fn idle_gc_shuts_down_when_no_clients_and_no_ptys() {
    // 200ms timeout with 40ms tick: the moment both counters hit zero
    // and stay there for 5 ticks the daemon must self-exit.
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("relay-v1.sock");
    let token_file = dir.path().join("relay-v1.token");
    let token = "deadbeef-test-token".to_string();
    std::fs::write(&token_file, &token).unwrap();
    let cfg = ServerConfig {
        socket_path: socket.clone(),
        token_file,
        pid_path: None,
        idle_timeout: Some(Duration::from_millis(200)),
        idle_tick_interval: Some(Duration::from_millis(40)),
    };
    let handle = tokio::spawn(async move { run_server(cfg).await });

    // No client ever connects. The idle GC should fire and break the
    // accept loop within ~5 ticks.
    let joined = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("idle GC never triggered shutdown");
    assert!(joined.is_ok());
}

#[tokio::test]
async fn pid_file_is_written_and_removed_on_clean_exit() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("relay-v1.sock");
    let token_file = dir.path().join("relay-v1.token");
    let pid_path = dir.path().join("relay-v1.pid");
    std::fs::write(&token_file, "deadbeef-test-token").unwrap();
    let cfg = ServerConfig {
        socket_path: socket.clone(),
        token_file,
        pid_path: Some(pid_path.clone()),
        idle_timeout: Some(Duration::from_millis(80)),
        idle_tick_interval: Some(Duration::from_millis(20)),
    };
    let handle = tokio::spawn(async move { run_server(cfg).await });

    // Wait for the pid file to appear.
    let mut saw_pid = false;
    for _ in 0..50 {
        if pid_path.exists() {
            saw_pid = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(saw_pid, "pid file never appeared at {}", pid_path.display());

    let raw = std::fs::read_to_string(&pid_path).unwrap();
    let pid: u32 = raw.trim().parse().expect("pid must parse");
    assert_eq!(pid, std::process::id(), "pid file should hold OUR pid");

    // Let idle GC fire so we observe clean-exit cleanup.
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(
        !pid_path.exists(),
        "pid file should be removed on clean exit"
    );
}

#[tokio::test]
async fn close_request_removes_pty_from_list() {
    let relay = boot_relay().await;
    let (mut s, mut buf) = connect_and_hello(&relay).await;
    let pty_id = match req(
        &mut s,
        &mut buf,
        2,
        Request::Spawn {
            cwd: "/tmp".into(),
            cols: 80,
            rows: 24,
            shell: Some("/bin/sh".into()),
            env: vec![],
        },
    )
    .await
    {
        Response::SpawnOk { pty_id } => pty_id,
        other => panic!("{other:?}"),
    };
    let listed = match req(&mut s, &mut buf, 3, Request::ListPtys).await {
        Response::PtyList(v) => v,
        other => panic!("{other:?}"),
    };
    assert!(listed.iter().any(|p| p.pty_id == pty_id));
    let resp = req(
        &mut s,
        &mut buf,
        4,
        Request::Close {
            pty_id: pty_id.clone(),
            grace_ms: 200,
        },
    )
    .await;
    assert!(matches!(resp, Response::Ok));
    let listed = match req(&mut s, &mut buf, 5, Request::ListPtys).await {
        Response::PtyList(v) => v,
        other => panic!("{other:?}"),
    };
    assert!(
        listed.iter().all(|p| p.pty_id != pty_id),
        "pty still listed"
    );
}
