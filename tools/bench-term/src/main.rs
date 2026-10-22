//! `bench-term` — measure what the terminal stack costs, so optimizations are
//! decided by numbers.
//!
//! - **relay**: a real `oximux-relay` daemon with N idle shells or N busy
//!   (`yes`) PTYs — its RSS, thread count, CPU and context switches.
//! - **model**: the client-side `TerminalState` — heap per terminal at a given
//!   scrollback, for plain, styled and wide text — and `advance` throughput.
//! - **checkpoint**: how much a gzip'd scrollback checkpoint saves.
//!
//! Output goes to stdout and, with `--report <path>`, to a Markdown file.
//! Numbers depend on the build profile; the report says which one ran.

mod alloc_count;
mod corpus;
mod model_probe;
mod relay_probe;

use std::fmt::Write as _;
use std::path::PathBuf;

#[global_allocator]
static ALLOC: alloc_count::Counting = alloc_count::Counting;

struct Args {
    relay: PathBuf,
    report: Option<PathBuf>,
    idle: Vec<usize>,
    busy: Vec<usize>,
    sample_secs: u64,
    skip_relay: bool,
}

fn parse() -> anyhow::Result<Args> {
    let exe_dir = std::env::current_exe()?.parent().map(PathBuf::from).unwrap_or_default();
    let mut a = Args {
        relay: exe_dir.join("oximux-relay"),
        report: None,
        idle: vec![1, 50, 200],
        busy: vec![1, 50],
        sample_secs: 10,
        skip_relay: false,
    };
    let list = |s: String| -> anyhow::Result<Vec<usize>> {
        s.split(',').filter(|p| !p.is_empty()).map(|p| Ok(p.trim().parse()?)).collect()
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--relay" => a.relay = it.next().ok_or_else(|| anyhow::anyhow!("--relay <path>"))?.into(),
            "--report" => a.report = Some(it.next().ok_or_else(|| anyhow::anyhow!("--report <path>"))?.into()),
            "--idle" => a.idle = list(it.next().unwrap_or_default())?,
            "--busy" => a.busy = list(it.next().unwrap_or_default())?,
            "--sample-secs" => a.sample_secs = it.next().unwrap_or_default().parse()?,
            "--no-relay" => a.skip_relay = true,
            "--smoke" => {
                a.idle = vec![1];
                a.busy = vec![1];
                a.sample_secs = 2;
            }
            "--help" | "-h" => {
                println!(
                    "bench-term [--relay PATH] [--idle 1,50,200] [--busy 1,50] [--sample-secs 10] \
                     [--no-relay] [--smoke] [--report PATH]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

fn main() -> anyhow::Result<()> {
    let args = parse()?;
    let mut out = String::new();
    let profile = if cfg!(debug_assertions) { "debug (unoptimized)" } else { "optimized" };
    writeln!(out, "# bench-term report\n\nBuild: {profile}. Host: {}.\n", std::env::consts::OS)?;

    writeln!(out, "## Client terminal model\n")?;
    model_probe::run(&mut out)?;

    writeln!(out, "\n## Checkpoint compression\n")?;
    model_probe::checkpoint_ratio(&mut out)?;

    if !args.skip_relay {
        writeln!(out, "\n## Relay daemon\n")?;
        relay_probe::run(&args.relay, &args.idle, &args.busy, args.sample_secs, &mut out)?;
    }
    print!("{out}");
    if let Some(path) = args.report {
        std::fs::write(&path, &out)?;
        eprintln!("report written to {}", path.display());
    }
    Ok(())
}
