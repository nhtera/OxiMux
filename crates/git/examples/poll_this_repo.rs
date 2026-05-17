//! Manual smoke for `StatusPoller`. Point at any git repo and watch state stream.
//!
//! Usage:
//!   cargo run -p oximux-git --example poll_this_repo -- [repo-path]
//!
//! repo-path defaults to the current directory. Type `f` + Enter on stdin to
//! toggle focus (pause/resume). Type `q` + Enter to quit. Ctrl-C also works.
//!
//! Set RUST_LOG=oximux_git=debug to see the failure-counter warns from H2.

use oximux_git::{PollState, Repository, StatusPoller};
use std::env;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("oximux_git=info")),
        )
        .init();

    let repo_path = env::args().nth(1).unwrap_or_else(|| ".".to_string());
    let repo = Repository::open(&repo_path).await?;
    println!("Watching: {}\n", repo.workdir().display());
    println!("Commands: f<Enter> = toggle focus  |  q<Enter> = quit  |  Ctrl-C = quit\n");

    // 500 ms cadence (default) so the recipe is realistic. Drop to 100 ms for
    // tighter feedback if you want.
    let poller = StatusPoller::spawn_with_interval(repo, Duration::from_millis(500));
    let mut rx = poller.subscribe();
    let mut focused = true;

    // Background task: print on every change.
    let mut rx_print = rx.clone();
    let printer = tokio::spawn(async move {
        loop {
            if rx_print.changed().await.is_err() {
                println!("[stream] sender closed (poller dropped)");
                break;
            }
            let snap = rx_print.borrow_and_update().clone();
            match snap {
                PollState::Loading => println!("[stream] Loading  (first poll not yet completed)"),
                PollState::Failed(e) => println!("[stream] Failed  ({e})"),
                PollState::Ready(s) => println!(
                    "[stream] branch={:?}  ahead={}  behind={}  files={}  head={}",
                    s.branch.as_deref(),
                    s.ahead,
                    s.behind,
                    s.files.len(),
                    s.head_oid.as_deref().unwrap_or("(initial)"),
                ),
            }
        }
    });

    // Foreground: stdin command loop.
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line? {
                    None => break, // EOF
                    Some(l) => {
                        match l.trim() {
                            "f" => {
                                focused = !focused;
                                poller.set_focused(focused);
                                println!("[ctl] focused = {focused}");
                            }
                            "q" => break,
                            "" => {} // ignore blank
                            other => println!("[ctl] unknown command: {other:?}"),
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\n[ctl] Ctrl-C");
                break;
            }
        }
    }

    drop(poller);
    let _ = rx.changed().await; // drain final sender-closed
    let _ = printer.await;
    Ok(())
}
