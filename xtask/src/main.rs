//! xtask — repo-level lint orchestrator. CI calls these subcommands.
//!
//! Subcommands:
//!   xtask file-size-lint   Walk crates/*/src and warn > 500 LOC, fail > 800.
//!   xtask ci-check         Run all xtask checks back-to-back.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const WARN_LOC: usize = 500;
const FAIL_LOC: usize = 800;

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
    println!(
        "xtask — OxiMux repo checks\n\
         \n\
         USAGE:\n  xtask <command>\n\
         \n\
         COMMANDS:\n\
           file-size-lint   Enforce {WARN_LOC} warn / {FAIL_LOC} fail LOC caps in crates/*/src/**/*.rs\n\
           ci-check         Run all checks (currently: file-size-lint)\n\
           help             Print this message"
    );
}

fn file_size_lint() -> Result<(), Box<dyn std::error::Error>> {
    let root = workspace_root()?;
    let crates_dir = root.join("crates");
    let mut warn = 0usize;
    let mut fail = 0usize;

    for rs in collect_rs_files(&crates_dir.join(""))? {
        let loc = count_loc(&rs)?;
        let rel = rs.strip_prefix(&root).unwrap_or(&rs).display();
        if loc > FAIL_LOC {
            eprintln!("FAIL  {loc:>4} LOC  {rel}  (> {FAIL_LOC})");
            fail += 1;
        } else if loc > WARN_LOC {
            eprintln!("WARN  {loc:>4} LOC  {rel}  (> {WARN_LOC})");
            warn += 1;
        }
    }

    if fail > 0 {
        return Err(
            format!("file-size-lint: {fail} file(s) over hard cap ({FAIL_LOC} LOC)").into(),
        );
    }
    println!("file-size-lint: ok ({warn} warnings, 0 failures)");
    Ok(())
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
