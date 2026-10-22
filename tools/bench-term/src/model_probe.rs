//! The client-side terminal model: heap per terminal, and parse throughput.

use std::fmt::Write as _;
use std::time::Instant;

use oximux_pty::TerminalState;

use crate::alloc_count::live_bytes;
use crate::corpus;

const COLS: u16 = 120;
const ROWS: u16 = 40;

fn mib(b: f64) -> f64 {
    b / (1024.0 * 1024.0)
}

/// Heap held by one terminal filled to `scrollback` rows of `payload` lines.
fn heap_of(scrollback: usize, payload: fn(usize) -> Vec<u8>) -> isize {
    let bytes = payload(scrollback + ROWS as usize + 10);
    let before = live_bytes();
    let mut t = TerminalState::new(COLS, ROWS, scrollback);
    t.advance(&bytes);
    t.clear_collected();
    let held = live_bytes() - before;
    drop(t);
    held
}

pub fn run(out: &mut String) -> anyhow::Result<()> {
    writeln!(out, "Heap per terminal ({COLS}×{ROWS}), filled to its scrollback:\n")?;
    writeln!(out, "| scrollback | plain | styled | wide/emoji |\n|---|---|---|---|")?;
    for sb in [5_000usize, 50_000] {
        let row: Vec<String> = [corpus::plain as fn(usize) -> Vec<u8>, corpus::styled, corpus::wide]
            .into_iter()
            .map(|p| format!("{:.1} MiB", mib(heap_of(sb, p) as f64)))
            .collect();
        writeln!(out, "| {sb} | {} |", row.join(" | "))?;
    }
    writeln!(out, "\n`advance` throughput (8 MiB corpora):\n")?;
    writeln!(out, "| corpus | MB/s |\n|---|---|")?;
    for (name, make) in [("plain", corpus::plain as fn(usize) -> Vec<u8>), ("styled", corpus::styled), ("wide", corpus::wide)] {
        let mut data = Vec::new();
        while data.len() < 8 << 20 {
            data.extend_from_slice(&make(2000));
        }
        let mut t = TerminalState::new(COLS, ROWS, 5000);
        let start = Instant::now();
        for chunk in data.chunks(8192) {
            t.advance(chunk);
        }
        let secs = start.elapsed().as_secs_f64();
        writeln!(out, "| {name} | {:.1} |", data.len() as f64 / 1e6 / secs)?;
    }
    Ok(())
}

/// Size of a full 1 MiB checkpoint, raw vs as stored.
pub fn checkpoint_ratio(out: &mut String) -> anyhow::Result<()> {
    writeln!(out, "| corpus | raw | stored | ratio |\n|---|---|---|---|")?;
    for (name, make) in [("plain", corpus::plain as fn(usize) -> Vec<u8>), ("styled", corpus::styled), ("wide", corpus::wide)] {
        let mut raw = Vec::new();
        while raw.len() < 1 << 20 {
            raw.extend_from_slice(&make(500));
        }
        raw.truncate(1 << 20);
        let start = Instant::now();
        let stored = oximux_relay_proto::checkpoint_codec::encode(&raw);
        let enc_ms = start.elapsed().as_secs_f64() * 1e3;
        assert_eq!(oximux_relay_proto::checkpoint_codec::decode(&stored), raw);
        writeln!(
            out,
            "| {name} | {} KiB | {} KiB | {:.1}× (encode {enc_ms:.1} ms) |",
            raw.len() / 1024,
            stored.len() / 1024,
            raw.len() as f64 / stored.len().max(1) as f64
        )?;
    }
    Ok(())
}
