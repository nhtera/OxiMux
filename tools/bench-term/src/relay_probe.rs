//! A real relay daemon under load: N idle shells, or N PTYs running `yes`.
//!
//! The daemon is the binary the app ships, started in a throwaway runtime dir
//! and driven over its own protocol. Its numbers come from the kernel: RSS,
//! threads, CPU time and context switches (`proc_pidinfo` on macOS,
//! `/proc/<pid>` on Linux), sampled over a fixed window.

use std::fmt::Write as _;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use oximux_relay_client::RelayClient;
use oximux_relay_proto::{Request, Response};

/// One kernel reading of the daemon.
#[derive(Debug, Clone, Copy)]
struct Sample {
    rss: u64,
    threads: u64,
    cpu_ns: u64,
    csw: u64,
}

pub fn run(relay: &Path, idle: &[usize], busy: &[usize], sample_secs: u64, out: &mut String) -> anyhow::Result<()> {
    anyhow::ensure!(relay.exists(), "relay binary not found at {} (build it, or pass --relay)", relay.display());
    writeln!(out, "Window: {sample_secs} s per row. Daemon: `{}`.\n", relay.display())?;
    writeln!(out, "| PTYs | kind | RSS | RSS/PTY | threads | CPU | ctx sw/s |\n|---|---|---|---|---|---|---|")?;
    let rt = tokio::runtime::Runtime::new()?;
    for (kind, counts) in [("idle", idle), ("busy", busy)] {
        for &n in counts {
            let row = rt.block_on(measure(relay, n, kind == "busy", sample_secs))?;
            writeln!(out, "{row}")?;
            eprintln!("{row}");
        }
    }
    Ok(())
}

async fn measure(relay: &Path, n: usize, busy: bool, sample_secs: u64) -> anyhow::Result<String> {
    let dir = std::env::temp_dir().join(format!("bench-term-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let _cleanup = RemoveOnDrop(dir.clone());
    let socket = dir.join("r.sock");
    let token = "bench-term-token";
    std::fs::write(dir.join("token"), token)?;
    let mut daemon = Daemon(
        Command::new(relay)
            .args(["--socket", &socket.to_string_lossy(), "--token", &dir.join("token").to_string_lossy()])
            .args(["--checkpoint-dir", &dir.join("ckpt").to_string_lossy()])
            .spawn()?,
    );
    let client = connect(&socket, token).await?;
    let base = sample(daemon.0.id())?;
    let mut ptys = Vec::new();
    for _ in 0..n {
        let args = if busy { vec!["-c".into(), "yes".into()] } else { Vec::new() };
        let resp = client
            .request(Request::Spawn {
                cwd: std::env::temp_dir().to_string_lossy().into_owned(),
                cols: 120,
                rows: 40,
                shell: Some("/bin/sh".into()),
                args,
                env: Vec::new(),
                prefill: Vec::new(),
            })
            .await?;
        match resp {
            Response::SpawnOk { pty_id, .. } => ptys.push(pty_id),
            other => anyhow::bail!("spawn: {other:?}"),
        }
    }
    // Let shells start and rings fill before the window opens.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = sample(daemon.0.id())?;
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(sample_secs)).await;
    let b = sample(daemon.0.id())?;
    let secs = t0.elapsed().as_secs_f64();
    for pty_id in ptys {
        let _ = client.request(Request::Close { pty_id, grace_ms: 0 }).await;
    }
    daemon.stop();
    let cpu = (b.cpu_ns - a.cpu_ns) as f64 / 1e9 / secs * 100.0;
    let csw = (b.csw - a.csw) as f64 / secs;
    let per_pty = (b.rss.saturating_sub(base.rss)) as f64 / n.max(1) as f64;
    Ok(format!(
        "| {n} | {} | {:.1} MiB | {:.2} MiB | {} | {cpu:.1}% | {csw:.0} |",
        if busy { "busy" } else { "idle" },
        b.rss as f64 / 1048576.0,
        per_pty / 1048576.0,
        b.threads,
    ))
}

async fn connect(socket: &Path, token: &str) -> anyhow::Result<RelayClient> {
    for _ in 0..200 {
        if let Ok(c) = RelayClient::connect(socket, token).await {
            return Ok(c);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("relay never came up at {}", socket.display())
}

/// A scratch directory, removed however the measurement ends.
struct RemoveOnDrop(std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The daemon process, killed on drop.
struct Daemon(Child);

impl Daemon {
    fn stop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
// `mach_timebase_info` is libc's only route to the CPU-time unit; its
// deprecation points at another crate, not at a replacement here.
#[allow(deprecated)]
fn sample(pid: u32) -> anyhow::Result<Sample> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: `info` is a properly sized, writable proc_taskinfo.
    let got = unsafe {
        libc::proc_pidinfo(pid as i32, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size)
    };
    anyhow::ensure!(got == size, "proc_pidinfo({pid}) failed");
    // CPU times are in Mach absolute-time units.
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    // SAFETY: writes the two fields of `tb`.
    unsafe { libc::mach_timebase_info(&mut tb) };
    let ticks = info.pti_total_user + info.pti_total_system;
    Ok(Sample {
        rss: info.pti_resident_size,
        threads: info.pti_threadnum as u64,
        cpu_ns: ticks * tb.numer as u64 / tb.denom.max(1) as u64,
        csw: info.pti_csw as u64,
    })
}

#[cfg(target_os = "linux")]
fn sample(pid: u32) -> anyhow::Result<Sample> {
    let field_in = |status: &str, name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |name: &str| field_in(&status, name);
    // Context switches are per thread in /proc: sum every thread's, or an idle
    // main thread would hide the workers' wakeups.
    let mut csw = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))?.flatten() {
        if let Ok(s) = std::fs::read_to_string(task.path().join("status")) {
            csw += field_in(&s, "voluntary_ctxt_switches:") + field_in(&s, "nonvoluntary_ctxt_switches:");
        }
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after_comm = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // utime, stime are fields 14 and 15 of stat (11 and 12 after the comm).
    let ticks: u64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0)
        + fields.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Ok(Sample {
        rss: field("VmRSS:") * 1024,
        threads: field("Threads:"),
        // Assumes the usual USER_HZ of 100 (10 ms per tick).
        cpu_ns: ticks * 10_000_000,
        csw,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample(_pid: u32) -> anyhow::Result<Sample> {
    anyhow::bail!("relay probe supports macOS and Linux")
}
