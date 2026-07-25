//! xtask — repo-level lint orchestrator. CI calls these subcommands.
//!
//! Subcommands:
//!   xtask file-size-lint   Walk the Rust source roots and warn > 1500 LOC,
//!                          fail > 3000.
//!   xtask ci-check         Run all xtask checks back-to-back.
//!
//! Thresholds match GPUI reality (large render/impl files are idiomatic). A
//! ratchet allowlist (`xtask/file-size-allow.txt`) grandfathers the handful of
//! files still over the hard cap at their recorded LOC: an allowlisted file may
//! shrink freely but fails the moment it grows past its recorded size, so the
//! debt can only go down. Drop a row once the file falls under the cap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const WARN_LOC: usize = 1500;
const FAIL_LOC: usize = 3000;
const ALLOW_FILE: &str = "xtask/file-size-allow.txt";

/// Repo-relative directories the lint walks, enumerated rather than globbed.
///
/// `apps/` is NOT walked wholesale on purpose: `apps/mobile` is a React Native
/// tree (~1.2 GB, thousands of directories under `ios/`) with no Rust in it, so
/// scanning it would cost far more than the lint itself. A new Rust app is one
/// row here.
const SOURCE_ROOTS: &[&str] = &["crates", "apps/desktop"];

fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    let result = match cmd.as_str() {
        "file-size-lint" => file_size_lint(),
        "ci-check" => file_size_lint(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => Err(format!("unknown subcommand: {other}\n").into()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    let roots = SOURCE_ROOTS
        .iter()
        .map(|r| format!("{r}/**/*.rs"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "xtask — OxiMux repo checks\n\
         \n\
         USAGE:\n  xtask <command>\n\
         \n\
         COMMANDS:\n\
           file-size-lint   Enforce {WARN_LOC} warn / {FAIL_LOC} fail LOC caps across {roots}\n\
           ci-check         Run all checks (currently: file-size-lint)\n\
           help             Print this message"
    );
}

fn file_size_lint() -> Result<(), Box<dyn std::error::Error>> {
    let root = workspace_root()?;
    let allow = load_allowlist(&root)?;
    // Track which allowlist rows we actually saw over-cap, to flag stale rows.
    let mut seen_over_cap: HashMap<String, bool> = allow.keys().map(|k| (k.clone(), false)).collect();
    let mut warn = 0usize;
    let mut fail = 0usize;

    let mut sources = Vec::new();
    for source_root in SOURCE_ROOTS {
        sources.extend(collect_rs_files(&root.join(source_root))?);
    }
    // Stable order regardless of read_dir(), so output diffs cleanly run to run.
    sources.sort();

    for rs in sources {
        let loc = count_loc(&rs)?;
        let rel_path = rs.strip_prefix(&root).unwrap_or(&rs);
        let rel = rel_path.to_string_lossy().replace('\\', "/");

        if loc > FAIL_LOC {
            match allow.get(&rel) {
                Some(&budget) => {
                    seen_over_cap.insert(rel.clone(), true);
                    if loc > budget {
                        eprintln!(
                            "FAIL  {loc:>4} LOC  {rel}  (allowlisted at {budget}, grew past it — ratchet only shrinks)"
                        );
                        fail += 1;
                    } else {
                        eprintln!("ALLOW {loc:>4} LOC  {rel}  (grandfathered, budget {budget})");
                    }
                }
                None => {
                    eprintln!("FAIL  {loc:>4} LOC  {rel}  (> {FAIL_LOC}, not allowlisted)");
                    fail += 1;
                }
            }
        } else if loc > WARN_LOC {
            eprintln!("WARN  {loc:>4} LOC  {rel}  (> {WARN_LOC})");
            warn += 1;
        }
    }

    // Stale allowlist rows: file dropped under the cap (or was deleted/moved).
    // Not a failure — just nudge to drain the row so the ratchet keeps shrinking.
    let stale: Vec<&String> = seen_over_cap
        .iter()
        .filter(|(_, seen)| !**seen)
        .map(|(k, _)| k)
        .collect();
    for path in &stale {
        eprintln!("STALE       allowlist row '{path}' no longer over cap — drop it from {ALLOW_FILE}");
    }

    if fail > 0 {
        return Err(
            format!("file-size-lint: {fail} file(s) over hard cap ({FAIL_LOC} LOC)").into(),
        );
    }
    println!(
        "file-size-lint: ok ({warn} warnings, {} allowlisted, {} stale rows)",
        allow.len() - stale.len(),
        stale.len()
    );
    Ok(())
}

/// Parse `xtask/file-size-allow.txt` into a map of `repo-relative path -> LOC budget`.
/// Lines are `<path> <loc>`; blank lines and `#` comments are ignored. A missing
/// file means an empty allowlist (every over-cap file then fails).
fn load_allowlist(root: &Path) -> Result<HashMap<String, usize>, Box<dyn std::error::Error>> {
    let path = root.join(ALLOW_FILE);
    let mut map = HashMap::new();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(map),
        Err(e) => return Err(e.into()),
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let file = parts
            .next()
            .ok_or_else(|| format!("{ALLOW_FILE}:{}: missing path", n + 1))?;
        let loc: usize = parts
            .next()
            .ok_or_else(|| format!("{ALLOW_FILE}:{}: missing LOC for '{file}'", n + 1))?
            .parse()
            .map_err(|_| format!("{ALLOW_FILE}:{}: LOC for '{file}' is not a number", n + 1))?;
        map.insert(file.replace('\\', "/"), loc);
    }
    Ok(map)
}

fn workspace_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut here = std::env::current_dir()?;
    loop {
        if here.join("Cargo.toml").exists()
            && std::fs::read_to_string(here.join("Cargo.toml"))
                .map(|s| s.contains("[workspace]"))
                .unwrap_or(false)
        {
            return Ok(here);
        }
        if !here.pop() {
            return Err("workspace root not found (run xtask from repo)".into());
        }
    }
}

fn collect_rs_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(name.as_ref(), "target" | "node_modules" | ".git") {
                continue;
            }
            walk(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn count_loc(path: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.lines().filter(|l| !l.trim().is_empty()).count())
}
