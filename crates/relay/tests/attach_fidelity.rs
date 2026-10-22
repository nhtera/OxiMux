//! A warm attach shows what is really on screen: the replay starts on a clean
//! line, carries the modes set before the ring's start, never overlaps or
//! misses live output, and a gap resync is fenced so nothing renders twice.
//!
//! Unix-only: the children are POSIX shell scripts.
#![cfg(unix)]

use std::time::{Duration, Instant};

use oximux_relay::registry::{PtyRegistry, REPLAY_BUFFER_BYTES, SpawnArgs};
use oximux_relay_proto::Notification;
use tokio::sync::mpsc::{Receiver, Sender, channel};

fn spawn(reg: &PtyRegistry, script: &str) -> String {
    reg.spawn(SpawnArgs {
        cwd: std::env::temp_dir(),
        cols: 120,
        rows: 30,
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        prefill: Vec::new(),
    })
    .expect("spawn")
}

/// Live output on `rx` until `marker` is seen (or 20 s pass).
async fn live_until(rx: &mut Receiver<Notification>, marker: &str, mut acc: Vec<u8>) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !contains(&acc, marker) && Instant::now() < deadline {
        if let Ok(Some(Notification::Output { bytes, .. })) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            acc.extend_from_slice(&bytes);
        }
    }
    acc
}

fn contains(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Wait until the PTY has produced `marker` (polling the replay).
async fn wait_for(reg: &PtyRegistry, pty: &str, marker: &str) {
    let (probe, _rx) = channel::<Notification>(8);
    for _ in 0..400 {
        let (replay, ..) = reg.replay(pty, &probe).expect("replay");
        if contains(&replay, marker) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{marker} never appeared");
}

/// (a) Past the ring's size, the replay starts at the beginning of a line and
/// is whole UTF-8.
#[tokio::test]
async fn a_wrapped_replay_starts_on_a_line_and_is_valid_utf8() {
    let reg = PtyRegistry::new();
    let lines = 3 * REPLAY_BUFFER_BYTES / 20;
    let pty = spawn(&reg, &format!("i=0; while [ $i -lt {lines} ]; do echo \"é line $i ok\"; i=$((i+1)); done; echo DONE; sleep 30"));
    wait_for(&reg, &pty, "DONE").await;
    let (tx, _rx) = channel(64);
    let (replay, ..) = reg.attach(&pty, tx).expect("attach");
    let text = std::str::from_utf8(&replay).expect("valid UTF-8");
    assert!(text.starts_with("é line "), "starts mid-line: {:?}", &text[..40.min(text.len())]);
}

/// (b) An alternate-screen program whose entry left the ring: no partial frame,
/// just the modes it set.
#[tokio::test]
async fn an_evicted_alt_screen_replays_as_modes_only() {
    let reg = PtyRegistry::new();
    // 1 KiB frames, so a couple of thousand iterations push the entry out.
    let frames = 2 * REPLAY_BUFFER_BYTES / 1024 + 64;
    let pty = spawn(
        &reg,
        &format!(
            "printf '\\033[?1049h\\033[?2004h'; f=$(printf '%01000d' 0); i=0; while [ $i -lt {frames} ]; do \
             printf '\\033[H%s\\n' \"$f\"; i=$((i+1)); done; printf 'DONE'; sleep 30"
        ),
    );
    // The replay of an evicted alt screen never shows `DONE`: watch live.
    let (probe, mut probe_rx) = channel(100_000);
    reg.attach(&pty, probe).expect("probe attach");
    assert!(contains(&live_until(&mut probe_rx, "DONE", Vec::new()).await, "DONE"));
    let (tx, _rx) = channel(64);
    let (replay, ..) = reg.attach(&pty, tx).expect("attach");
    let text = String::from_utf8_lossy(&replay);
    assert!(text.starts_with("\x1b[?1049h"), "{text:?}");
    assert!(text.contains("\x1b[?2004h"), "{text:?}");
    assert!(replay.len() < 64, "a body was replayed: {} bytes", replay.len());
}

/// Numbered lines in order, from `bytes`, ignoring a partial first line.
fn line_numbers(bytes: &[u8]) -> Vec<u64> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|l| l.trim().strip_prefix('L').and_then(|n| n.parse().ok()))
        .collect()
}

fn assert_contiguous(nums: &[u64]) {
    assert!(nums.len() > 100, "too little output: {}", nums.len());
    for w in nums.windows(2) {
        assert_eq!(w[1], w[0] + 1, "gap or duplicate between {} and {}", w[0], w[1]);
    }
}

/// (c) Attaching while the program is printing: replay + live is exactly the
/// program's output — nothing missing, nothing twice.
#[tokio::test]
async fn an_attach_during_output_neither_drops_nor_repeats() {
    let reg = PtyRegistry::new();
    let pty = spawn(&reg, "i=0; while [ $i -lt 60000 ]; do echo L$i; i=$((i+1)); done; echo END; sleep 30");
    wait_for(&reg, &pty, "L2000\r").await;
    let (tx, mut rx) = channel(100_000);
    let (replay, ..) = reg.attach(&pty, tx).expect("attach");
    let all = live_until(&mut rx, "END", replay).await;
    assert_contiguous(&line_numbers(&all));
}

/// (d) Two attachments of one PTY each get copies addressed to themselves.
#[tokio::test]
async fn each_attachment_gets_only_its_own_copies() {
    let reg = PtyRegistry::new();
    let pty = spawn(&reg, "sleep 0.3; i=0; while [ $i -lt 200 ]; do echo L$i; i=$((i+1)); done; echo END; sleep 30");
    let (tx_a, mut rx_a) = channel(10_000);
    let (tx_b, mut rx_b) = channel(10_000);
    let (_, _, _, a) = reg.attach(&pty, tx_a).expect("attach a");
    let (_, _, _, b) = reg.attach(&pty, tx_b).expect("attach b");
    for (rx, me) in [(&mut rx_a, a), (&mut rx_b, b)] {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen_end = false;
        while !seen_end && Instant::now() < deadline {
            if let Ok(Some(n)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                assert_eq!(n.attachment_id(), Some(me), "{n:?}");
                if let Notification::Output { bytes, .. } = &n {
                    seen_end |= contains(bytes, "END");
                }
            }
        }
        assert!(seen_end);
    }
}

/// (e) A gap resync is fenced: output queued before the fence is already in
/// the replay, output after it is new — together, contiguous.
#[tokio::test]
async fn a_resync_fence_splits_old_output_from_new() {
    let reg = PtyRegistry::new();
    let pty = spawn(&reg, "i=0; while [ $i -lt 60000 ]; do echo L$i; i=$((i+1)); done; echo END; sleep 30");
    let (tx, mut rx): (Sender<Notification>, _) = channel(200_000);
    reg.attach(&pty, tx.clone()).expect("attach");
    wait_for(&reg, &pty, "L3000\r").await;
    let (replay, ..) = reg.replay(&pty, &tx).expect("replay");
    // Everything up to the fence is in `replay` already.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "no fence");
        if let Ok(Some(Notification::Output { bytes, .. })) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            && bytes.is_empty()
        {
            break;
        }
    }
    let all = live_until(&mut rx, "END", replay).await;
    assert_contiguous(&line_numbers(&all));
}

/// The repaint nudge: one SIGWINCH per attach burst to a full-screen program,
/// none on resync.
#[tokio::test]
async fn an_attach_to_a_full_screen_program_asks_it_to_repaint_once() {
    let reg = PtyRegistry::new();
    let pty = spawn(&reg, "trap 'printf W' WINCH; printf '\\033[?1049hREADY'; while :; do sleep 0.05; done");
    wait_for(&reg, &pty, "READY").await;
    let (tx, mut rx) = channel(1000);
    reg.attach(&pty, tx.clone()).expect("attach 1");
    let (tx2, _rx2) = channel(1000);
    reg.attach(&pty, tx2).expect("attach 2 (inside the rate limit)");
    reg.replay(&pty, &tx).expect("resync");
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if let Ok(Some(Notification::Output { bytes, .. })) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            got.extend_from_slice(&bytes);
        }
    }
    let ws = got.iter().filter(|&&b| b == b'W').count();
    assert_eq!(ws, 1, "{:?}", String::from_utf8_lossy(&got));
}
