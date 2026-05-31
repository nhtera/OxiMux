//! End-to-end smoke test for AI commit-message generation.
//!
//! Spawns the configured agent CLI against the current worktree's
//! staged diff, prints the generated message. Mirrors what the
//! sparkles button does at runtime in the OxiMux GUI, but without
//! the GUI — useful for verifying the agent dispatch works before
//! launching the full app.
//!
//! ## Usage
//!
//! ```bash
//! # Heuristic mode (no agent CLI required):
//! cargo run -p oximux-agents --example commit_message_demo -- \
//!     --mode heuristic
//!
//! # Claude:
//! cargo run -p oximux-agents --example commit_message_demo -- \
//!     --mode agent --agent claude --model sonnet
//!
//! # Codex:
//! cargo run -p oximux-agents --example commit_message_demo -- \
//!     --mode agent --agent codex --model gpt-5.5
//!
//! # Custom CLI (anything else):
//! cargo run -p oximux-agents --example commit_message_demo -- \
//!     --mode agent --agent custom --custom-command "ollama run llama3"
//!
//! # Against a different worktree:
//! cargo run -p oximux-agents --example commit_message_demo -- \
//!     --mode agent --agent claude --workdir /path/to/repo
//! ```
//!
//! Requires at least one staged file in the target worktree. The
//! script will print "no staged changes" and exit if nothing is
//! staged.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use oximux_agents::commit_message::{
    self, AgentConfig, AgentId, Mode, StagedContext,
};
use oximux_core::IndexStatus;

struct Args {
    mode: String,
    agent: String,
    model: String,
    thinking: Option<String>,
    custom_command: Option<String>,
    custom_prompt: Option<String>,
    workdir: PathBuf,
}

impl Args {
    fn parse() -> Self {
        let mut args = Args {
            mode: "heuristic".to_string(),
            agent: "claude".to_string(),
            model: "sonnet".to_string(),
            thinking: None,
            custom_command: None,
            custom_prompt: None,
            workdir: std::env::current_dir().expect("cwd"),
        };
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < raw.len() {
            let key = raw[i].as_str();
            let take_value = || raw.get(i + 1).cloned().expect("value after flag");
            match key {
                "--mode" => {
                    args.mode = take_value();
                    i += 2;
                }
                "--agent" => {
                    args.agent = take_value();
                    i += 2;
                }
                "--model" => {
                    args.model = take_value();
                    i += 2;
                }
                "--thinking" => {
                    args.thinking = Some(take_value());
                    i += 2;
                }
                "--custom-command" => {
                    args.custom_command = Some(take_value());
                    i += 2;
                }
                "--custom-prompt" => {
                    args.custom_prompt = Some(take_value());
                    i += 2;
                }
                "--workdir" => {
                    args.workdir = PathBuf::from(take_value());
                    i += 2;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("unknown flag: {other}");
                    print_help();
                    std::process::exit(1);
                }
            }
        }
        args
    }
}

fn print_help() {
    println!(
        "Usage: commit_message_demo [OPTIONS]\n\n\
         Options:\n  \
           --mode <heuristic|agent>        default: heuristic\n  \
           --agent <claude|codex|custom>   default: claude (for --mode agent)\n  \
           --model <id>                    default: sonnet\n  \
           --thinking <low|medium|high|xhigh|max>\n  \
           --custom-command <template>     required when --agent custom\n  \
           --custom-prompt <text>          optional style suffix\n  \
           --workdir <path>                default: cwd\n  \
           --help, -h                      show this message"
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    println!("=== commit-message demo ===");
    println!("mode:    {}", args.mode);
    if args.mode == "agent" {
        println!("agent:   {}", args.agent);
        println!("model:   {}", args.model);
        if let Some(t) = &args.thinking {
            println!("think:   {t}");
        }
        if let Some(c) = &args.custom_command {
            println!("custom:  {c}");
        }
    }
    println!("workdir: {}", args.workdir.display());
    println!();

    // Step 1: fetch staged context (skip for heuristic-only — it
    // doesn't need the diff, just the FileStatus list).
    let mode = build_mode(&args)?;
    let needs_context = matches!(mode, Mode::Agent(_));

    let context = if needs_context {
        println!("→ fetching staged diff from `{}`...", args.workdir.display());
        match oximux_git::staged_context::fetch(&args.workdir).await? {
            Some(ctx) => {
                println!(
                    "  branch={}  summary_bytes={}  patch_bytes={}",
                    ctx.branch.as_deref().unwrap_or("(detached)"),
                    ctx.summary.len(),
                    ctx.patch.len(),
                );
                StagedContext {
                    branch: ctx.branch,
                    summary: ctx.summary,
                    patch: ctx.patch,
                    workdir: args.workdir.clone(),
                }
            }
            None => {
                eprintln!("no staged changes in `{}` — `git add` something first.", args.workdir.display());
                std::process::exit(2);
            }
        }
    } else {
        // Heuristic only needs the FileStatus list; the prompt
        // context isn't consulted. Use a placeholder.
        StagedContext {
            branch: None,
            summary: String::new(),
            patch: String::new(),
            workdir: args.workdir.clone(),
        }
    };

    // Step 2: for heuristic mode we also need a FileStatus list.
    // Reuse the git crate's status poller to fetch it.
    let staged_files = if matches!(mode, Mode::Heuristic) {
        let repo = oximux_git::Repository::open(args.workdir.clone()).await?;
        let state = repo.status().await?;
        state
            .files
            .into_iter()
            .filter(|f| {
                !matches!(
                    f.index,
                    IndexStatus::Unmodified | IndexStatus::Untracked | IndexStatus::Ignored
                )
            })
            .collect()
    } else {
        Vec::new()
    };

    // Step 3: dispatch.
    let cancel = Arc::new(AtomicBool::new(false));
    println!("→ generating ({} mode)...\n", args.mode);
    let started = std::time::Instant::now();
    match commit_message::generate(&mode, &staged_files, context, cancel).await {
        Ok(message) => {
            let elapsed = started.elapsed();
            println!("─── generated message ({} elapsed) ───", format_elapsed(elapsed));
            println!("{message}");
            println!("─────────────────────────────────────────");
        }
        Err(err) => {
            eprintln!("✗ generation failed: {err}");
            std::process::exit(3);
        }
    }
    Ok(())
}

fn build_mode(args: &Args) -> Result<Mode, Box<dyn std::error::Error>> {
    match args.mode.as_str() {
        "off" => Ok(Mode::Off),
        "heuristic" => Ok(Mode::Heuristic),
        "agent" => {
            let agent_id = AgentId::parse(&args.agent)
                .ok_or_else(|| format!("unknown agent: {}", args.agent))?;
            Ok(Mode::Agent(AgentConfig {
                agent_id,
                model: args.model.clone(),
                thinking_level: args.thinking.clone(),
                custom_prompt: args.custom_prompt.clone(),
                custom_command: args.custom_command.clone(),
                agent_command_override: None,
            }))
        }
        other => Err(format!("unknown mode: {other}").into()),
    }
}

fn format_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs_f32();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{secs:.1}s")
    }
}
