//! The daemon answers a program's OSC 7501 feature query for the PTYs it owns:
//! once per query burst, never for replayed history, and never at the cost of
//! the reader — a program that floods queries and reads nothing still gets its
//! Ctrl-C.
//!
//! Unix-only: the children are POSIX shells using `stty` read timeouts. The
//! matcher and gate are platform-neutral and unit-tested in the crate.
#![cfg(unix)]

use std::time::{Duration, Instant};

use oximux_relay::registry::{PtyRegistry, SpawnArgs};
use oximux_relay_proto::Notification;

/// The reply, as `od -An -tx1` prints it.
const REPLY_HEX: &str = "1b 5d 37 35 30 31 3b 3f 1b 5c";
const QUERY: &str = r"\033]7501;?\007";

fn spawn(reg: &PtyRegistry, script: String, prefill: Vec<u8>) -> String {
    reg.spawn(SpawnArgs {
        cwd: std::env::temp_dir(),
        cols: 200,
        rows: 24,
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), script],
        env: Vec::new(),
        prefill,
    })
    .expect("spawn")
}

/// Print every input byte received within a second of quiet, as hex, then DONE.
fn dump_input() -> &'static str {
    "dd bs=1 count=64 2>/dev/null | od -An -tx1 | tr -d '\\n'; printf '\\r\\nDONE\\r\\n'"
}

/// Attach and collect output until `marker` shows up (replay included).
async fn output_until(reg: &PtyRegistry, pty_id: &str, marker: &str) -> String {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (replay, ..) = reg.attach(pty_id, tx).expect("attach");
    let mut out = replay;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&out).contains(marker) && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(Notification::Output { bytes, .. })) => out.extend_from_slice(&bytes),
            Ok(Some(_)) | Err(_) => {}
            Ok(None) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn replies_in(output: &str) -> usize {
    // `od` spacing differs between platforms; compare word by word.
    output.split_whitespace().collect::<Vec<_>>().join(" ").matches(REPLY_HEX).count()
}

#[tokio::test]
async fn a_query_burst_gets_exactly_one_reply() {
    let reg = PtyRegistry::new();
    let pty = spawn(
        &reg,
        format!("stty raw -echo min 0 time 10; printf '{QUERY}{QUERY}'; {}", dump_input()),
        Vec::new(),
    );
    let out = output_until(&reg, &pty, "DONE").await;
    assert!(out.contains("DONE"), "{out:?}");
    assert_eq!(replies_in(&out), 1, "{out:?}");
}

#[tokio::test]
async fn a_query_in_the_seeded_history_is_never_answered() {
    let reg = PtyRegistry::new();
    let pty = spawn(
        &reg,
        format!("stty raw -echo min 0 time 10; {}", dump_input()),
        b"restored\r\n\x1b]7501;?\x07".to_vec(),
    );
    let out = output_until(&reg, &pty, "DONE").await;
    assert!(out.contains("DONE"), "{out:?}");
    assert_eq!(replies_in(&out), 0, "{out:?}");
}

/// The deadlock the writer thread exists to prevent: a program that keeps
/// asking and never reads its input. The pane must stay live and Ctrl-C must
/// still reach it.
#[tokio::test]
async fn a_query_flood_from_a_program_that_never_reads_cannot_wedge_it() {
    let reg = PtyRegistry::new();
    let pty = spawn(
        &reg,
        format!(
            "trap 'printf \"GOT_INT\\r\\n\"; exit 0' INT; i=0; \
             while [ $i -lt 200 ]; do printf '{QUERY}'; i=$((i+1)); done; printf 'FLOODED\\r\\n'; \
             while :; do sleep 1; done"
        ),
        Vec::new(),
    );
    let flooded = output_until(&reg, &pty, "FLOODED").await;
    assert!(flooded.contains("FLOODED"), "{flooded:?}");
    let started = Instant::now();
    reg.write(&pty, b"\x03").expect("ctrl-c reaches the pty");
    assert!(started.elapsed() < Duration::from_secs(2), "a write blocked behind replies");
    let out = output_until(&reg, &pty, "GOT_INT").await;
    assert!(out.contains("GOT_INT"), "{out:?}");
}

/// Client input and a reply share one writer: input lands whole even while the
/// daemon is answering.
#[tokio::test]
async fn input_written_around_a_reply_arrives_intact() {
    let reg = PtyRegistry::new();
    let pty = spawn(
        &reg,
        format!("stty raw -echo min 0 time 10; printf '{QUERY}'; {}", dump_input()),
        Vec::new(),
    );
    reg.write(&pty, b"AB").expect("write");
    let out = output_until(&reg, &pty, "DONE").await;
    let words = out.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(replies_in(&out), 1, "{out:?}");
    // `A` `B` as one run, wherever the reply landed relative to it.
    assert!(words.contains("41 42"), "{out:?}");
}
